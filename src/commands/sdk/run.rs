//! SDK run command implementation.

#[cfg(unix)]
use crate::utils::signing_service::{generate_helper_script, SigningService, SigningServiceConfig};
use anyhow::{Context, Result};
#[cfg(unix)]
use std::path::PathBuf;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::validate_and_log_target,
};

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

    /// Setup signing service for runtime if signing is configured
    #[cfg(unix)]
    async fn setup_signing_service(
        &mut self,
        config: &Config,
        runtime_name: &str,
    ) -> Result<Option<(PathBuf, PathBuf, String, String)>> {
        // Check if runtime has signing configuration
        let signing_key_name = match config.get_runtime_signing_key(runtime_name) {
            Some(keyid) => {
                // Get the key name from signing_keys mapping
                let signing_keys = config.get_signing_keys();
                signing_keys
                    .and_then(|keys| {
                        keys.iter()
                            .find(|(_, v)| *v == &keyid)
                            .map(|(k, _)| k.clone())
                    })
                    .context("Signing key ID not found in signing_keys mapping")?
            }
            None => {
                // No signing configured for this runtime
                if self.verbose {
                    print_info(
                        "No signing key configured for runtime. Signing service will not be started.",
                        OutputLevel::Verbose,
                    );
                }
                return Ok(None);
            }
        };

        let keyid = config
            .get_runtime_signing_key(runtime_name)
            .context("Failed to get signing key ID")?;

        // Get checksum algorithm (defaults to sha256)
        let checksum_str = config
            .runtime
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

    /// Setup signing service stub for non-Unix platforms
    #[cfg(not(unix))]
    async fn setup_signing_service(
        &mut self,
        _config: &Config,
        _runtime_name: &str,
    ) -> Result<Option<(std::path::PathBuf, std::path::PathBuf, String, String)>> {
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
                "You must either provide a --command (-c) or use --interactive (-i)."
            ));
        }

        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Early target validation and logging - fail fast if target is unsupported
        let target = validate_and_log_target(self.target.as_deref(), &config)?;

        // Get merged SDK configuration for the target
        let merged_sdk_config = config.get_merged_sdk_config(&target, &self.config_path)?;

        // Get repo_url and repo_release from merged config
        let repo_url = merged_sdk_config.repo_url.as_ref();
        let repo_release = merged_sdk_config.repo_release.as_ref();

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

        // Build the command to execute
        let command = if let Some(ref cmd) = self.command {
            let user_command = cmd.join(" ");
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
            self.setup_signing_service(&config, &runtime_name).await?
        } else {
            None
        };

        // Use the container helper to run the command
        let container_helper =
            SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

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
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
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

        if success {
            print_success("SDK command completed successfully.", OutputLevel::Normal);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .contains("You must either provide a --command"));
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
