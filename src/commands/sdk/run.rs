//! SDK run command implementation.

#[cfg(unix)]
use crate::utils::signing_service::{generate_helper_script, SigningService, SigningServiceConfig};
use anyhow::{Context, Result};
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::feeds::FeedStage;
use crate::utils::{
    config::{ComposedConfig, Config, SdkConfig},
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, print_warning, OutputLevel},
    shell::shell_escape,
    target::validate_and_log_target,
};

/// Join a user-supplied argv into one shell-safe command string.
///
/// The result is spliced into a shell script inside the container, so each
/// element must be quoted or the container shell re-splits it.
fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Validate an explicit `-r/--runtime` against the composed config.
///
/// A typo'd runtime name otherwise falls through to an unsigned run that still
/// exits 0 (ENG-2352). When no runtimes are defined, the only runtime that can
/// exist is the implicit `default` that the composed loader synthesizes but does
/// not inject into `config.runtimes`, so accept that name and reject any other.
fn validate_runtime_arg(
    runtime: Option<&str>,
    runtimes: Option<&std::collections::HashMap<String, crate::utils::config::RuntimeConfig>>,
) -> Result<()> {
    let Some(runtime_name) = runtime else {
        return Ok(());
    };

    let Some(map) = runtimes.filter(|m| !m.is_empty()) else {
        // No runtimes defined: only the implicit `default` is valid.
        if runtime_name == "default" {
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "Runtime '{runtime_name}' not found in configuration. No runtimes are defined; only the implicit 'default' runtime is available."
        ));
    };

    if map.contains_key(runtime_name) {
        return Ok(());
    }
    let mut available: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
    available.sort_unstable();
    Err(anyhow::anyhow!(
        "Runtime '{runtime_name}' not found in configuration. Available runtimes: {}",
        available.join(", ")
    ))
}

/// The feed URL and releasever for the SDK container. These are the values
/// `sdk install` and `sdk dnf` use, so a pinned snapshot in avocado.lock
/// applies here too.
fn sdk_repo(config: &Config) -> (Option<String>, Option<String>) {
    (config.get_sdk_repo_url(), config.get_sdk_repo_release())
}

/// The config that `sdk run` resolves its feed from. `sdk run` also reads
/// `repo_url` and `repo_release` from the legacy `sdk.<target>` block (see
/// the schema). A value set there replaces the base `sdk` value. An explicit
/// `repo_release` there also turns the lock pin off, as `sdk.repo_release`
/// does.
fn run_feed_config(config: &Config, merged: &SdkConfig) -> Config {
    let mut feed_config = config.clone();
    let sdk = feed_config.sdk.get_or_insert_with(SdkConfig::default);
    if merged.repo_release.is_some() && merged.repo_release != sdk.repo_release {
        sdk.repo_release = merged.repo_release.clone();
        feed_config.locked_releasever = None;
    }
    if merged.repo_url.is_some() {
        sdk.repo_url = merged.repo_url.clone();
    }
    feed_config
}

/// Named feeds are bind-mounted from a host tempdir that is removed when the
/// CLI exits. A detached container outlives the CLI, and the remote
/// `--runs-on` path refuses named feeds, so both run without them.
fn named_feeds_supported(detach: bool, runs_on: bool) -> bool {
    !detach && !runs_on
}

/// Implementation of the 'sdk run' command.
pub struct SdkRunCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Assign a name to the container
    pub name: Option<String>,
    /// Run container in background and print container ID
    pub detach: bool,
    /// Automatically remove the container when it exits
    pub rm: bool,
    /// Drop into interactive shell in container
    pub interactive: bool,
    /// Enable verbose output
    pub verbose: bool,
    /// Source the avocado SDK environment before running command
    pub env: bool,
    /// Mount extension sysroot and change working directory to it
    pub extension: Option<String>,
    /// Mount runtime sysroot and change working directory to it
    pub runtime: Option<String>,
    /// Command and arguments to run in container
    pub command: Option<Vec<String>>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Skip SDK bootstrap initialization
    pub no_bootstrap: bool,
    /// Remote host to run on (format: user@host)
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Signing service handle (Unix only)
    #[cfg(unix)]
    signing_service: Option<SigningService>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl SdkRunCommand {
    /// Create a new SdkRunCommand instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_path: String,
        name: Option<String>,
        detach: bool,
        rm: bool,
        interactive: bool,
        verbose: bool,
        env: bool,
        extension: Option<String>,
        runtime: Option<String>,
        command: Option<Vec<String>>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
        no_bootstrap: bool,
    ) -> Self {
        Self {
            config_path,
            name,
            detach,
            rm,
            interactive,
            verbose,
            env,
            extension,
            runtime,
            command,
            target,
            container_args,
            dnf_args,
            no_bootstrap,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            #[cfg(unix)]
            signing_service: None,
            composed_config: None,
        }
    }

    /// Set remote execution options
    pub fn with_runs_on(mut self, runs_on: Option<String>, nfs_port: Option<u16>) -> Self {
        self.runs_on = runs_on;
        self.nfs_port = nfs_port;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Setup signing service for runtime if signing is configured
    #[cfg(unix)]
    async fn setup_signing_service(
        &mut self,
        config: &Config,
        runtime_name: &str,
    ) -> Result<Option<(PathBuf, PathBuf, String, String)>> {
        // An unresolvable declared key aborts rather than warning: without the
        // signing service the container skips signing and exits 0 unsigned.
        let Some((signing_key_name, keyid)) = config.resolve_runtime_signing_key(runtime_name)?
        else {
            if self.verbose {
                print_info(
                    "No signing key configured for runtime. Signing service will not be started.",
                    OutputLevel::Verbose,
                );
            }
            return Ok(None);
        };

        // Get checksum algorithm (defaults to sha256)
        let checksum_str = config
            .runtimes
            .as_ref()
            .and_then(|r| r.get(runtime_name))
            .and_then(|rc| rc.signing.as_ref())
            .map(|s| s.checksum_algorithm.as_str())
            .unwrap_or("sha256");

        // Create temporary directory for socket and helper script
        let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
        let socket_path = temp_dir.path().join("sign.sock");
        let helper_script_path = temp_dir.path().join("avocado-sign-request");

        // Write helper script
        let helper_script = generate_helper_script();
        std::fs::write(&helper_script_path, helper_script)
            .context("Failed to write helper script")?;

        // Make helper script executable
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&helper_script_path, perms)
                .context("Failed to set helper script permissions")?;
        }

        if self.verbose {
            print_info(
                &format!(
                    "Starting signing service with key '{signing_key_name}' using {checksum_str} checksums"
                ),
                OutputLevel::Verbose,
            );
        }

        // Start signing service
        let service_config = SigningServiceConfig {
            socket_path: socket_path.clone(),
            runtime_name: runtime_name.to_string(),
            key_name: signing_key_name.clone(),
            keyid,
            verbose: self.verbose,
        };

        let service = SigningService::start(service_config, temp_dir).await?;

        // Store the service handle for cleanup
        self.signing_service = Some(service);

        Ok(Some((
            socket_path,
            helper_script_path,
            signing_key_name,
            checksum_str.to_string(),
        )))
    }

    /// The signing service needs Unix domain sockets, so it cannot run here:
    /// runs on this platform are unsigned regardless of key resolvability.
    /// Warns rather than failing; both conditions are said out loud so a
    /// typo'd key does not pass with no output at all.
    #[cfg(not(unix))]
    async fn setup_signing_service(
        &mut self,
        config: &Config,
        runtime_name: &str,
    ) -> Result<Option<(std::path::PathBuf, std::path::PathBuf, String, String)>> {
        match config.resolve_runtime_signing_key(runtime_name) {
            Ok(Some((declared, _))) => print_warning(
                &format!(
                    "Runtime '{runtime_name}' declares signing key '{declared}', but the signing \
                     service is unavailable on this platform. It will not be started."
                ),
                OutputLevel::Normal,
            ),
            Ok(None) => {}
            Err(e) => print_warning(
                &format!("{e}. The signing service is unavailable on this platform anyway."),
                OutputLevel::Normal,
            ),
        }
        Ok(None)
    }

    /// Cleanup signing service resources
    #[cfg(unix)]
    async fn cleanup_signing_service(&mut self) -> Result<()> {
        if let Some(service) = self.signing_service.take() {
            service.shutdown().await?;
        }
        Ok(())
    }

    /// Cleanup signing service stub for non-Unix platforms
    #[cfg(not(unix))]
    async fn cleanup_signing_service(&mut self) -> Result<()> {
        Ok(())
    }

    /// Execute the sdk run command
    pub async fn execute(mut self) -> Result<()> {
        // Validate arguments
        if self.interactive && self.detach {
            return Err(anyhow::anyhow!(
                "Cannot specify both --interactive (-i) and --detach (-d) simultaneously."
            ));
        }

        // Validate that extension and runtime are not both specified
        if self.extension.is_some() && self.runtime.is_some() {
            return Err(anyhow::anyhow!(
                "Cannot specify both --extension (-e) and --runtime (-r) simultaneously."
            ));
        }

        // Require either a command or --interactive flag
        if !self.interactive && self.command.is_none() {
            return Err(anyhow::anyhow!(
                "You must either provide a command to run or use --interactive (-i)."
            ));
        }

        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;

        // A typo'd -r/--runtime otherwise falls through to an unsigned run that
        // still exits 0 (ENG-2352).
        validate_runtime_arg(self.runtime.as_deref(), config.runtimes.as_ref())?;

        // Early target validation and logging - fail fast if target is unsupported
        let target = validate_and_log_target(self.target.as_deref(), config)?;

        // Get merged SDK configuration for the target
        let merged_sdk_config = config.get_merged_sdk_config(&target, &self.config_path)?;

        let feed_config = run_feed_config(config, &merged_sdk_config);
        let (repo_url, repo_release) = sdk_repo(&feed_config);
        let feeds = if named_feeds_supported(self.detach, self.runs_on.is_some()) {
            feed_config
                .materialize_feeds(&target, FeedStage::Sdk, &self.config_path)
                .await?
        } else {
            if feed_config.repos.is_some()
                || feed_config
                    .distro
                    .as_ref()
                    .is_some_and(|d| d.feeds.is_some())
            {
                print_warning(
                    "Named feeds (repos: / distro.feeds) are not available in a detached or --runs-on 'sdk run'. dnf in this container sees only the distro feed.",
                    OutputLevel::Normal,
                );
            }
            None
        };

        // Merge container args from merged config with CLI args
        let config_container_args = merged_sdk_config.container_args.as_ref();
        let merged_container_args = match (config_container_args, self.container_args.as_ref()) {
            (Some(config_args), Some(cli_args)) => {
                let mut processed_args =
                    Config::process_container_args(Some(config_args)).unwrap_or_default();
                processed_args.extend_from_slice(cli_args);
                Some(processed_args)
            }
            (Some(config_args), None) => Config::process_container_args(Some(config_args)),
            (None, Some(cli_args)) => Some(cli_args.clone()),
            (None, None) => None,
        };

        // Get the SDK image from merged configuration
        let container_image = merged_sdk_config.image.ok_or_else(|| {
            anyhow::anyhow!(
                "No container image specified in config under 'sdk.image' or 'sdk.{target}.image'"
            )
        })?;

        if let Some(ref name) = self.name {
            println!("Container name: {name}");
        }

        // Build the command to execute.
        //
        // The argv vector is spliced into a shell script inside the container, so
        // each element has to be quoted or the container shell re-splits it. A bare
        // join(" ") turns `-- bash -lc 'X=1; echo $X'` into
        // `bash -lc X=1; echo $X`, which the shell reads as two commands and
        // expands `$X` in the *outer* shell (to nothing). That silently produces
        // wrong output instead of an error, which is worse than failing.
        let command = if let Some(ref cmd) = self.command {
            let user_command = join_argv(cmd);
            if self.env {
                format!(". avocado-env && {user_command}")
            } else {
                user_command
            }
        } else if self.env {
            ". avocado-env && bash".to_string()
        } else {
            "bash".to_string()
        };

        // Setup signing service if a runtime is specified
        let signing_config = if let Some(runtime_name) = self.runtime.clone() {
            self.setup_signing_service(config, &runtime_name).await?
        } else {
            None
        };

        // Use the container helper to run the command
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Create RunConfig - detach mode is now handled by the shared run_in_container
        let mut run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: command.clone(),
            container_name: self.name.clone(),
            detach: self.detach,
            rm: self.rm,
            verbose: self.verbose,
            source_environment: self.env,
            interactive: self.interactive,
            repo_url,
            repo_release,
            feeds,
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            extension_sysroot: self.extension.clone(),
            runtime_sysroot: self.runtime.clone(),
            no_bootstrap: self.no_bootstrap,
            runs_on: self.runs_on.clone(),
            nfs_port: self.nfs_port,
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        // Add signing configuration to run_config if available
        if let Some((socket_path, helper_script_path, key_name, checksum_algo)) = signing_config {
            run_config.signing_socket_path = Some(socket_path);
            run_config.signing_helper_script_path = Some(helper_script_path);
            run_config.signing_key_name = Some(key_name);
            run_config.signing_checksum_algorithm = Some(checksum_algo);
        }

        // Use shared run_in_container for both detached and non-detached modes
        let success = container_helper.run_in_container(run_config).await?;

        // Cleanup signing service
        self.cleanup_signing_service().await?;

        result_from_container_success(success)
    }
}

/// Map the in-container exit status onto the CLI result.
///
/// A `false` success means the in-container command exited non-zero, so the
/// CLI must also exit non-zero. Without this, a failed command (or a failed
/// in-container build) still exited 0, and scripts and CI could not see the
/// failure. See ENG-2352 and bug report B-7.
fn result_from_container_success(success: bool) -> Result<()> {
    if success {
        print_success("SDK command completed successfully.", OutputLevel::Normal);
        Ok(())
    } else {
        anyhow::bail!("SDK command failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed in-container command must surface as an error so the CLI exits
    /// non-zero; a success must stay `Ok`. On `main` the failure branch returned
    /// `Ok(())`, so this test fails there and guards the ENG-2352 / B-7 fix.
    #[test]
    fn container_failure_becomes_error() {
        assert!(
            result_from_container_success(false).is_err(),
            "a failed container command must return an error"
        );
        assert!(
            result_from_container_success(true).is_ok(),
            "a successful container command must return Ok"
        );
    }

    /// A typo'd `-r` must error and list the defined runtimes, not fall through
    /// to an unsigned run that exits 0 (ENG-2352).
    #[test]
    fn sdk_run_rejects_unknown_runtime() {
        use crate::utils::config::RuntimeConfig;
        let rt = || serde_yaml::from_str::<RuntimeConfig>("{}").unwrap();
        let mut runtimes = std::collections::HashMap::new();
        runtimes.insert("prod".to_string(), rt());
        runtimes.insert("dev".to_string(), rt());

        let err = validate_runtime_arg(Some("prodd"), Some(&runtimes))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Runtime 'prodd' not found"), "got: {err}");
        assert!(
            err.contains("dev, prod"),
            "lists sorted runtimes, got: {err}"
        );

        // A defined runtime passes.
        assert!(validate_runtime_arg(Some("prod"), Some(&runtimes)).is_ok());
        // No -r passes.
        assert!(validate_runtime_arg(None, Some(&runtimes)).is_ok());
        // No runtimes defined: the synthesized `default` is accepted, but any
        // other name is still rejected, with both an absent and an empty map.
        assert!(validate_runtime_arg(Some("default"), None).is_ok());
        assert!(validate_runtime_arg(Some("bogus"), None).is_err());
        let empty = std::collections::HashMap::new();
        assert!(validate_runtime_arg(Some("default"), Some(&empty)).is_ok());
        assert!(validate_runtime_arg(Some("bogus"), Some(&empty)).is_err());
    }

    /// `sdk run` used to read the legacy `sdk.repo_release` only, so the
    /// container fell back to the image codename. It must use the configured
    /// feed and the snapshot that avocado.lock pins, like `sdk dnf`.
    #[test]
    #[serial_test::serial]
    fn sdk_run_uses_the_locked_snapshot() {
        use crate::utils::lockfile::{LockFile, RepoSnapshot};
        for var in [
            "AVOCADO_RELEASEVER",
            "AVOCADO_SDK_REPO_RELEASE",
            "AVOCADO_DISTRO_RELEASE",
            "AVOCADO_DISTRO_CHANNEL",
            "AVOCADO_TARGET",
        ] {
            std::env::remove_var(var);
        }
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("avocado.yaml");
        std::fs::write(
            &config_path,
            "default_target: qemux86-64\ndistro:\n  release: 2026\n  channel: next\nsdk:\n  image: docker.io/avocadolinux/sdk:2026\n",
        )
        .unwrap();
        let config_path = config_path.to_string_lossy().to_string();

        let unpinned = Config::load_composed(&config_path, None).unwrap();
        assert_eq!(sdk_repo(&unpinned.config).1.as_deref(), Some("2026/next"));

        let mut lock = LockFile::new();
        lock.set_repo_snapshot(
            "qemux86-64",
            RepoSnapshot {
                release: "2026".to_string(),
                channel: "next".to_string(),
                snapshot: "320".to_string(),
                repo_url: None,
                created: None,
            },
        );
        lock.save(dir.path()).unwrap();
        let pinned = Config::load_composed(&config_path, None).unwrap();
        assert_eq!(
            sdk_repo(&pinned.config).1.as_deref(),
            Some("2026/next/snapshots/320")
        );
        let merged = pinned
            .config
            .get_merged_sdk_config("qemux86-64", &config_path)
            .unwrap();
        assert_eq!(
            sdk_repo(&run_feed_config(&pinned.config, &merged))
                .1
                .as_deref(),
            Some("2026/next/snapshots/320"),
            "with no sdk.<target> block the lock pin still applies"
        );
    }

    /// The schema says `sdk run` reads `repo_url` and `repo_release` from the
    /// legacy `sdk.<target>` block. An explicit `repo_release` there wins
    /// over the lock pin, as the base `sdk.repo_release` does.
    #[test]
    #[serial_test::serial]
    fn sdk_run_uses_the_target_sdk_repo_values() {
        use crate::utils::lockfile::{LockFile, RepoSnapshot};
        for var in [
            "AVOCADO_RELEASEVER",
            "AVOCADO_SDK_REPO_RELEASE",
            "AVOCADO_REPO_URL",
            "AVOCADO_SDK_REPO_URL",
            "AVOCADO_DISTRO_RELEASE",
            "AVOCADO_DISTRO_CHANNEL",
            "AVOCADO_TARGET",
        ] {
            std::env::remove_var(var);
        }
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("avocado.yaml");
        std::fs::write(
            &config_path,
            "default_target: qemux86-64\ndistro:\n  release: 2026\n  channel: next\nsdk:\n  image: docker.io/avocadolinux/sdk:2026\n  qemux86-64:\n    repo_url: https://target.example\n    repo_release: custom/rv\n",
        )
        .unwrap();
        let config_path = config_path.to_string_lossy().to_string();
        let mut lock = LockFile::new();
        lock.set_repo_snapshot(
            "qemux86-64",
            RepoSnapshot {
                release: "2026".to_string(),
                channel: "next".to_string(),
                snapshot: "320".to_string(),
                repo_url: None,
                created: None,
            },
        );
        lock.save(dir.path()).unwrap();

        let composed = Config::load_composed(&config_path, None).unwrap();
        let merged = composed
            .config
            .get_merged_sdk_config("qemux86-64", &config_path)
            .unwrap();
        let (url, release) = sdk_repo(&run_feed_config(&composed.config, &merged));
        assert_eq!(url.as_deref(), Some("https://target.example"));
        assert_eq!(release.as_deref(), Some("custom/rv"));
    }

    /// The feed tempdir is removed when the CLI exits, so a detached
    /// container must not bind-mount it.
    #[test]
    fn detached_and_remote_runs_get_no_named_feeds() {
        assert!(named_feeds_supported(false, false));
        assert!(!named_feeds_supported(true, false));
        assert!(!named_feeds_supported(false, true));
    }

    #[test]
    fn test_new() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            Some("test-container".to_string()),
            false,
            true,
            false,
            true,
            false, // env
            None,  // extension
            None,  // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
            false, // no_bootstrap
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert_eq!(cmd.name, Some("test-container".to_string()));
        assert!(!cmd.detach);
        assert!(cmd.rm);
        assert!(!cmd.interactive);
        assert!(cmd.verbose);
        assert_eq!(
            cmd.command,
            Some(vec!["echo".to_string(), "test".to_string()])
        );
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_interactive_with_env_and_command() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false, // detach
            false, // rm
            true,  // interactive
            false, // verbose
            true,  // env
            None,  // extension
            None,  // runtime
            Some(vec!["ls".to_string(), "-la".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
            false, // no_bootstrap
        );

        // Verify that the command struct stores the values correctly
        assert!(cmd.interactive);
        assert!(cmd.env);
        assert_eq!(cmd.command, Some(vec!["ls".to_string(), "-la".to_string()]));
    }

    #[test]
    fn test_join_argv_preserves_argv_boundaries() {
        // A bare join(" ") would emit `bash -lc X=1; echo $X`, which the
        // container shell splits into two commands and expands `$X` itself.
        let argv = vec![
            "bash".to_string(),
            "-lc".to_string(),
            "X=1; echo $X".to_string(),
        ];
        assert_eq!(join_argv(&argv), "'bash' '-lc' 'X=1; echo $X'");
    }

    #[test]
    fn test_join_argv_escapes_embedded_single_quotes() {
        let argv = vec!["echo".to_string(), "it's".to_string()];
        assert_eq!(join_argv(&argv), "'echo' 'it'\\''s'");
    }

    #[tokio::test]
    async fn test_invalid_arguments() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            true, // detach
            false,
            true, // interactive
            false,
            false, // env
            None,  // extension
            None,  // runtime
            None,
            None,
            None,
            None,
            false, // no_bootstrap
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Cannot specify both"));
    }

    #[tokio::test]
    async fn test_no_command_or_interactive() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false, // not interactive
            false,
            false, // env
            None,  // extension
            None,  // runtime
            None,  // no command
            None,
            None,
            None,
            false, // no_bootstrap
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("You must either provide a command to run"));
    }

    #[test]
    fn test_env_flag_command_building() {
        // Test with env flag and command
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            true, // env = true
            None, // extension
            None, // runtime
            Some(vec![
                "vm".to_string(),
                "--mem".to_string(),
                "512".to_string(),
            ]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert!(cmd.env);
        assert_eq!(
            cmd.command,
            Some(vec![
                "vm".to_string(),
                "--mem".to_string(),
                "512".to_string()
            ])
        );

        // Test without env flag
        let cmd_no_env = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false, // env = false
            None,  // extension
            None,  // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert!(!cmd_no_env.env);
    }

    #[tokio::test]
    async fn test_extension_and_runtime_conflict() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            Some("test-ext".to_string()),     // extension
            Some("test-runtime".to_string()), // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Cannot specify both --extension (-e) and --runtime (-r)"));
    }

    #[test]
    fn test_extension_sysroot_creation() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            Some("test-ext".to_string()), // extension
            None,                         // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert_eq!(cmd.extension, Some("test-ext".to_string()));
        assert_eq!(cmd.runtime, None);
    }

    #[test]
    fn test_runtime_sysroot_creation() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            None,                             // extension
            Some("test-runtime".to_string()), // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert_eq!(cmd.extension, None);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
    }

    #[test]
    fn test_no_bootstrap_flag() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            None, // extension
            None, // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            true, // no_bootstrap = true
        );

        assert!(cmd.no_bootstrap);
        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.env);
    }
}
