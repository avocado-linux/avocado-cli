//! Container utilities for SDK operations.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_error, print_info, OutputLevel};
use crate::utils::volume::{VolumeManager, VolumeState};

/// Configuration for running commands in containers
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub container_image: String,
    pub target: String,
    pub command: String,
    pub container_name: Option<String>,
    pub detach: bool,
    pub rm: bool,
    pub env_vars: Option<HashMap<String, String>>,
    pub verbose: bool,
    pub source_environment: bool,
    pub use_entrypoint: bool,
    pub interactive: bool,
    pub repo_url: Option<String>,
    pub repo_release: Option<String>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub extension_sysroot: Option<String>,
    pub runtime_sysroot: Option<String>,
    pub no_bootstrap: bool,
    pub disable_weak_dependencies: bool,
    pub signing_socket_path: Option<PathBuf>,
    pub signing_helper_script_path: Option<PathBuf>,
    pub signing_key_name: Option<String>,
    pub signing_checksum_algorithm: Option<String>,
    /// Remote host to run on (format: user@host)
    pub runs_on: Option<String>,
    /// NFS port for remote execution (auto-selected if None)
    pub nfs_port: Option<u16>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            container_image: String::new(),
            target: String::new(),
            command: String::new(),
            container_name: None,
            detach: false,
            rm: true,
            env_vars: None,
            verbose: false,
            source_environment: true,
            use_entrypoint: true,
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: None,
            dnf_args: None,
            extension_sysroot: None,
            runtime_sysroot: None,
            no_bootstrap: false,
            disable_weak_dependencies: false,
            signing_socket_path: None,
            signing_helper_script_path: None,
            signing_key_name: None,
            signing_checksum_algorithm: None,
            runs_on: None,
            nfs_port: None,
        }
    }
}

/// Container helper for SDK operations
pub struct SdkContainer {
    pub container_tool: String,
    pub cwd: PathBuf,
    pub src_dir: Option<PathBuf>,
    pub verbose: bool,
}

impl Default for SdkContainer {
    fn default() -> Self {
        Self::new()
    }
}

impl SdkContainer {
    /// Create a new SdkContainer instance
    pub fn new() -> Self {
        Self {
            container_tool: "docker".to_string(),
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            src_dir: None,
            verbose: false,
        }
    }

    /// Create a new SdkContainer with custom container tool
    #[allow(dead_code)]
    pub fn with_tool(container_tool: String) -> Self {
        Self {
            container_tool,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            src_dir: None,
            verbose: false,
        }
    }

    /// Set verbose mode
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set custom source directory for mounting
    pub fn with_src_dir(mut self, src_dir: Option<PathBuf>) -> Self {
        self.src_dir = src_dir;
        self
    }

    /// Create a new SdkContainer with configuration from config file
    pub fn from_config(config_path: &str, config: &crate::utils::config::Config) -> Result<Self> {
        let src_dir = config.get_resolved_src_dir(config_path);
        Ok(Self::new().with_src_dir(src_dir))
    }

    /// Run a command in the container
    pub async fn run_in_container(&self, config: RunConfig) -> Result<bool> {
        // Check if we should run on a remote host
        if let Some(ref runs_on) = config.runs_on {
            return self.run_in_container_remote(&config, runs_on).await;
        }

        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        // Build environment variables
        let mut env_vars = config.env_vars.clone().unwrap_or_default();

        // Set host platform environment variable
        let host_platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        env_vars.insert(
            "AVOCADO_HOST_PLATFORM".to_string(),
            host_platform.to_string(),
        );

        if let Some(url) = &config.repo_url {
            env_vars.insert("AVOCADO_SDK_REPO_URL".to_string(), url.clone());
        }
        if let Some(release) = &config.repo_release {
            env_vars.insert("AVOCADO_SDK_REPO_RELEASE".to_string(), release.clone());
        }
        if let Some(dnf_args) = &config.dnf_args {
            env_vars.insert("AVOCADO_DNF_ARGS".to_string(), dnf_args.join(" "));
        }
        if config.verbose || self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Build the complete command
        let mut full_command = String::new();

        // Conditionally include the entrypoint script
        if config.use_entrypoint {
            full_command.push_str(&self.create_entrypoint_script(
                config.source_environment,
                config.extension_sysroot.as_deref(),
                config.runtime_sysroot.as_deref(),
                &config.target,
                config.no_bootstrap,
                config.disable_weak_dependencies,
            ));
            full_command.push('\n');
        }

        full_command.push_str(&config.command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command with volume state
        let container_cmd =
            self.build_container_command(&config, &bash_cmd, &env_vars, &volume_state)?;

        // Execute the command
        self.execute_container_command(
            &container_cmd,
            config.detach,
            config.verbose || self.verbose,
        )
        .await
    }

    /// Run a command in a container on a remote host via NFS
    async fn run_in_container_remote(&self, config: &RunConfig, runs_on: &str) -> Result<bool> {
        use crate::utils::runs_on::RunsOnContext;

        // Get or create local docker volume (we need this to export via NFS)
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        let src_dir = self.src_dir.as_ref().unwrap_or(&self.cwd);

        print_info(
            &format!("Setting up remote execution on {}...", runs_on),
            OutputLevel::Normal,
        );

        // Setup remote execution context
        let mut context = RunsOnContext::setup(
            runs_on,
            config.nfs_port,
            src_dir,
            &volume_state.volume_name,
            &self.container_tool,
            &config.container_image,
            config.verbose || self.verbose,
        )
        .await
        .context("Failed to setup remote execution")?;

        // Setup signing tunnel if signing is configured
        #[cfg(unix)]
        if let Some(ref socket_path) = config.signing_socket_path {
            let _ = context.setup_signing_tunnel(socket_path).await;
        }

        // Build environment variables
        let mut env_vars = config.env_vars.clone().unwrap_or_default();

        // Set host platform - the remote is running the container
        env_vars.insert("AVOCADO_HOST_PLATFORM".to_string(), "linux".to_string());

        if let Some(url) = &config.repo_url {
            env_vars.insert("AVOCADO_SDK_REPO_URL".to_string(), url.clone());
        }
        if let Some(release) = &config.repo_release {
            env_vars.insert("AVOCADO_SDK_REPO_RELEASE".to_string(), release.clone());
        }
        if let Some(dnf_args) = &config.dnf_args {
            env_vars.insert("AVOCADO_DNF_ARGS".to_string(), dnf_args.join(" "));
        }
        if config.verbose || self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Set target and SDK-related env vars
        env_vars.insert("AVOCADO_TARGET".to_string(), config.target.clone());
        env_vars.insert("AVOCADO_SDK_TARGET".to_string(), config.target.clone());
        env_vars.insert("AVOCADO_SRC_DIR".to_string(), "/opt/src".to_string());

        // Set host UID/GID for bindfs permission mapping on remote
        // This maps the local host user's files to root inside the container
        let (host_uid, host_gid) = crate::utils::config::resolve_host_uid_gid(None);
        env_vars.insert("AVOCADO_HOST_UID".to_string(), host_uid.to_string());
        env_vars.insert("AVOCADO_HOST_GID".to_string(), host_gid.to_string());

        // Build the complete command with entrypoint
        // NFS src volume is mounted to /mnt/src, bindfs remaps to /opt/src with UID translation
        let mut full_command = String::new();
        if config.use_entrypoint {
            full_command.push_str(&self.create_entrypoint_script_for_remote(
                config.source_environment,
                config.extension_sysroot.as_deref(),
                config.runtime_sysroot.as_deref(),
                &config.target,
                config.no_bootstrap,
                config.disable_weak_dependencies,
            ));
            full_command.push('\n');
        }
        full_command.push_str(&config.command);

        // Build extra Docker args
        let mut extra_args: Vec<String> = vec![
            "--device".to_string(),
            "/dev/fuse".to_string(),
            "--cap-add".to_string(),
            "SYS_ADMIN".to_string(),
        ];

        if let Some(ref args) = config.container_args {
            extra_args.extend(args.clone());
        }

        if config.interactive {
            extra_args.push("-it".to_string());
        }

        let extra_args_refs: Vec<&str> = extra_args.iter().map(|s| s.as_str()).collect();

        print_info(
            &format!("Running command on remote host {}...", runs_on),
            OutputLevel::Normal,
        );

        // Run the container on the remote
        let result = context
            .run_container_command(
                &config.container_image,
                &full_command,
                env_vars,
                &extra_args_refs
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .await;

        // Always cleanup, even on error
        if let Err(e) = context.teardown().await {
            print_error(
                &format!("Warning: Failed to cleanup remote resources: {}", e),
                OutputLevel::Normal,
            );
        }

        result.context("Remote container execution failed")
    }

    /// Build the complete container command
    fn build_container_command(
        &self,
        config: &RunConfig,
        command: &[String],
        env_vars: &HashMap<String, String>,
        volume_state: &VolumeState,
    ) -> Result<Vec<String>> {
        let mut container_cmd = vec![self.container_tool.clone(), "run".to_string()];

        // Container options
        if config.rm {
            container_cmd.push("--rm".to_string());
        }
        if let Some(name) = &config.container_name {
            container_cmd.push("--name".to_string());
            container_cmd.push(name.to_string());
        }
        if config.detach {
            container_cmd.push("-d".to_string());
        }
        if config.interactive {
            container_cmd.push("-i".to_string());
            container_cmd.push("-t".to_string());
        }

        // Add FUSE device and capability for bindfs support
        container_cmd.push("--device".to_string());
        container_cmd.push("/dev/fuse".to_string());
        container_cmd.push("--cap-add".to_string());
        container_cmd.push("SYS_ADMIN".to_string());

        // Volume mounts: docker volume for persistent state, bind mount for source
        // Source is mounted to /mnt/src, then bindfs remounts it to /opt/src with permission translation
        container_cmd.push("-v".to_string());
        let src_path = self.src_dir.as_ref().unwrap_or(&self.cwd);
        container_cmd.push(format!("{}:/mnt/src:rw", src_path.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Mount signing socket directory if provided
        if let Some(socket_path) = &config.signing_socket_path {
            if let Some(socket_dir) = socket_path.parent() {
                container_cmd.push("-v".to_string());
                container_cmd.push(format!("{}:/run/avocado:rw", socket_dir.display()));
            }
        }

        // Mount signing helper script if provided
        if let Some(helper_script_path) = &config.signing_helper_script_path {
            container_cmd.push("-v".to_string());
            container_cmd.push(format!(
                "{}:/usr/local/bin/avocado-sign-request:ro",
                helper_script_path.display()
            ));
        }

        // Mount signing keys directory if it exists (read-only for security)
        let signing_keys_env =
            if let Ok(signing_keys_dir) = crate::utils::signing_keys::get_signing_keys_dir() {
                if signing_keys_dir.exists() {
                    container_cmd.push("-v".to_string());
                    container_cmd.push(format!(
                        "{}:/opt/signing-keys:ro",
                        signing_keys_dir.display()
                    ));
                    // Return environment variable so container knows where keys are mounted
                    Some("/opt/signing-keys".to_string())
                } else {
                    None
                }
            } else {
                None
            };

        // Note: Working directory is handled in the entrypoint script based on sysroot parameters

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_TARGET={}", config.target));
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={}", config.target));
        container_cmd.push("-e".to_string());
        container_cmd.push("AVOCADO_SRC_DIR=/opt/src".to_string());

        // Pass host UID/GID for bindfs permission translation
        let (host_uid, host_gid) = crate::utils::config::resolve_host_uid_gid(None);
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_HOST_UID={}", host_uid));
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_HOST_GID={}", host_gid));

        // Add signing-related environment variables
        if config.signing_socket_path.is_some() {
            container_cmd.push("-e".to_string());
            container_cmd.push("AVOCADO_SIGNING_SOCKET=/run/avocado/sign.sock".to_string());
            container_cmd.push("-e".to_string());
            container_cmd.push("AVOCADO_SIGNING_ENABLED=1".to_string());
        }

        if let Some(key_name) = &config.signing_key_name {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_KEY_NAME={}", key_name));
        }

        if let Some(checksum_algo) = &config.signing_checksum_algorithm {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_CHECKSUM={}", checksum_algo));
        }

        // Add signing keys directory env var if mounted
        if let Some(keys_dir) = signing_keys_env {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_KEYS_DIR={}", keys_dir));
        }

        for (key, value) in env_vars {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("{key}={value}"));
        }

        // Add additional container arguments if provided
        if let Some(args) = &config.container_args {
            for arg in args {
                container_cmd.extend(Self::parse_container_arg(arg));
            }
        }

        // Add the container image
        container_cmd.push(config.container_image.to_string());

        // Add the command to execute
        container_cmd.extend(command.iter().cloned());

        Ok(container_cmd)
    }

    /// Run a command in the container and capture its output
    pub async fn run_in_container_with_output(&self, config: RunConfig) -> Result<Option<String>> {
        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        // Build environment variables
        let mut env_vars = config.env_vars.clone().unwrap_or_default();

        // Set host platform environment variable
        let host_platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        env_vars.insert(
            "AVOCADO_HOST_PLATFORM".to_string(),
            host_platform.to_string(),
        );

        if let Some(url) = &config.repo_url {
            env_vars.insert("AVOCADO_SDK_REPO_URL".to_string(), url.clone());
        }
        if let Some(release) = &config.repo_release {
            env_vars.insert("AVOCADO_SDK_REPO_RELEASE".to_string(), release.clone());
        }
        if let Some(dnf_args) = &config.dnf_args {
            env_vars.insert("AVOCADO_DNF_ARGS".to_string(), dnf_args.join(" "));
        }
        if config.verbose || self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Build the complete command
        let mut full_command = String::new();

        // Conditionally include the entrypoint script
        if config.use_entrypoint {
            full_command.push_str(&self.create_entrypoint_script(
                config.source_environment,
                config.extension_sysroot.as_deref(),
                config.runtime_sysroot.as_deref(),
                &config.target,
                config.no_bootstrap,
                config.disable_weak_dependencies,
            ));
            full_command.push('\n');
        }

        full_command.push_str(&config.command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command with volume state
        let container_cmd =
            self.build_container_command(&config, &bash_cmd, &env_vars, &volume_state)?;

        if config.verbose || self.verbose {
            print_info(
                &format!(
                    "Mounting source directory: {} -> /mnt/src (bindfs -> /opt/src)",
                    self.cwd.display()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Container command: {}", container_cmd.join(" ")),
                OutputLevel::Normal,
            );
        }

        // Execute command and capture output
        let mut cmd = AsyncCommand::new(&container_cmd[0]);
        cmd.args(&container_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .with_context(|| "Failed to execute container command")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(Some(stdout))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if config.verbose || self.verbose {
                print_error(
                    &format!("Container execution failed: {stderr}"),
                    OutputLevel::Normal,
                );
            }
            Ok(None)
        }
    }

    /// Query installed package versions from a sysroot using rpm -q
    ///
    /// This runs an rpm query command inside the container to get the actual
    /// installed versions of packages. Used for lock file generation.
    ///
    /// # Arguments
    /// * `sysroot` - The sysroot type to query
    /// * `packages` - List of package names to query
    /// * `container_image` - Container image to use
    /// * `target` - Target architecture
    /// * `repo_url` - Optional repository URL
    /// * `repo_release` - Optional repository release
    /// * `container_args` - Optional additional container arguments
    ///
    /// # Returns
    /// A HashMap of package name to version string (NEVRA format without name prefix)
    #[allow(clippy::too_many_arguments)]
    pub async fn query_installed_packages(
        &self,
        sysroot: &crate::utils::lockfile::SysrootType,
        packages: &[String],
        container_image: &str,
        target: &str,
        repo_url: Option<String>,
        repo_release: Option<String>,
        container_args: Option<Vec<String>>,
    ) -> Result<std::collections::HashMap<String, String>> {
        if packages.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rpm_config = sysroot.get_rpm_query_config();
        let query_command = rpm_config.build_query_command(packages);

        if self.verbose {
            print_info(
                &format!(
                    "Querying installed packages for lock file (sysroot: {:?}): {}",
                    sysroot, query_command
                ),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: query_command,
            verbose: self.verbose,
            // Don't source environment-setup for RPM queries - we only need the
            // basic env vars which are set in the entrypoint, not the full SDK env
            source_environment: false,
            use_entrypoint: true,
            interactive: false,
            repo_url,
            repo_release,
            container_args,
            ..Default::default()
        };

        match self.run_in_container_with_output(run_config).await? {
            Some(output) => {
                // For SDK sysroots, strip architecture to make lock file portable across host architectures
                let strip_arch = matches!(sysroot, crate::utils::lockfile::SysrootType::Sdk);
                let versions = crate::utils::lockfile::parse_rpm_query_output(&output, strip_arch);
                if self.verbose {
                    print_info(
                        &format!(
                            "Found {} installed package versions for lock file",
                            versions.len()
                        ),
                        OutputLevel::Normal,
                    );
                    for (name, version) in &versions {
                        print_info(&format!("  {} = {}", name, version), OutputLevel::Normal);
                    }
                }
                // Warn if we expected packages but got none (likely a parse or query issue)
                if versions.is_empty() && !packages.is_empty() && self.verbose {
                    print_info(
                        &format!(
                            "Warning: RPM query returned no parseable packages. Raw output: {}",
                            if output.len() > 200 {
                                format!("{}...", &output[..200])
                            } else {
                                output
                            }
                        ),
                        OutputLevel::Normal,
                    );
                }
                Ok(versions)
            }
            None => {
                // Command failed - this is important for lock file accuracy, so always warn
                print_info(
                    &format!(
                        "Warning: RPM query for lock file failed for packages: {}",
                        packages.join(", ")
                    ),
                    OutputLevel::Normal,
                );
                Ok(std::collections::HashMap::new())
            }
        }
    }

    /// Execute the container command
    async fn execute_container_command(
        &self,
        container_cmd: &[String],
        detach: bool,
        verbose: bool,
    ) -> Result<bool> {
        if verbose {
            print_info(
                &format!(
                    "Mounting source directory: {} -> /mnt/src (bindfs -> /opt/src)",
                    self.cwd.display()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Container command: {}", container_cmd.join(" ")),
                OutputLevel::Normal,
            );
        }

        let mut cmd = AsyncCommand::new(&container_cmd[0]);
        cmd.args(&container_cmd[1..]);

        if detach {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let output = cmd
                .output()
                .await
                .with_context(|| "Failed to execute container command")?;

            if output.status.success() {
                let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                print_info(
                    &format!("Container started in detached mode with ID: {container_id}"),
                    OutputLevel::Normal,
                );
                Ok(true)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                print_error(
                    &format!("Container execution failed: {stderr}"),
                    OutputLevel::Normal,
                );
                Ok(false)
            }
        } else {
            // In non-detached mode, we need to capture output to ensure stderr is visible
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            let status = cmd
                .status()
                .await
                .with_context(|| "Failed to execute container command")?;
            Ok(status.success())
        }
    }

    /// Run a simple command in the container without the full SDK entrypoint.
    ///
    /// This is useful for quick one-off operations like chown, cp, etc.
    /// that don't need the SDK environment setup.
    ///
    /// # Arguments
    /// * `container_image` - The container image to use
    /// * `command` - The bash command to run
    /// * `rm` - Whether to remove the container after exit
    ///
    /// # Returns
    /// `true` if the command succeeded, `false` otherwise
    pub async fn run_simple_command(
        &self,
        container_image: &str,
        command: &str,
        rm: bool,
    ) -> Result<bool> {
        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        let mut container_cmd = vec![self.container_tool.clone(), "run".to_string()];

        if rm {
            container_cmd.push("--rm".to_string());
        }

        // Add FUSE device and capability for bindfs support
        container_cmd.push("--device".to_string());
        container_cmd.push("/dev/fuse".to_string());
        container_cmd.push("--cap-add".to_string());
        container_cmd.push("SYS_ADMIN".to_string());

        // Volume mounts: docker volume for persistent state, bind mount for source
        container_cmd.push("-v".to_string());
        let src_path = self.src_dir.as_ref().unwrap_or(&self.cwd);
        container_cmd.push(format!("{}:/mnt/src:rw", src_path.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Pass host UID/GID for bindfs permission translation
        let (host_uid, host_gid) = crate::utils::config::resolve_host_uid_gid(None);
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_HOST_UID={}", host_uid));
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_HOST_GID={}", host_gid));

        // Add the container image
        container_cmd.push(container_image.to_string());

        // Add the command
        container_cmd.push("bash".to_string());
        container_cmd.push("-c".to_string());

        // Prepend bindfs check and setup to the command
        // If host UID is 0 (root), skip bindfs and use simple bind mount
        let full_command = if host_uid == 0 && host_gid == 0 {
            format!(
                "mkdir -p /opt/src && mount --bind /mnt/src /opt/src && {}",
                command
            )
        } else {
            format!(
                r#"if ! command -v bindfs >/dev/null 2>&1; then
    echo "[ERROR] bindfs is not installed in this container image."
    echo ""
    echo "bindfs is required for proper file permission handling between the host and container."
    echo ""
    echo "To install bindfs in your container image, add one of the following to your Dockerfile:"
    echo ""
    echo "  # For Ubuntu/Debian-based images:"
    echo "  RUN apt-get update && apt-get install -y bindfs"
    echo ""
    echo "  # For Fedora/RHEL-based images:"
    echo "  RUN dnf install -y bindfs"
    echo ""
    echo "  # For Alpine-based images:"
    echo "  RUN apk add --no-cache bindfs"
    echo ""
    echo "  # For Arch-based images:"
    echo "  RUN pacman -S --noconfirm bindfs"
    echo ""
    exit 1
fi
mkdir -p /opt/src && bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 /mnt/src /opt/src && {}"#,
                command
            )
        };
        container_cmd.push(full_command);

        if self.verbose {
            print_info(
                &format!(
                    "Mounting source directory: {} -> /mnt/src (bindfs -> /opt/src)",
                    src_path.display()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Simple container command: {}", container_cmd.join(" ")),
                OutputLevel::Normal,
            );
        }

        let mut cmd = AsyncCommand::new(&container_cmd[0]);
        cmd.args(&container_cmd[1..]);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        let status = cmd
            .status()
            .await
            .with_context(|| "Failed to execute simple container command")?;

        Ok(status.success())
    }

    /// Create the entrypoint script for remote execution (NFS volumes)
    /// This skips the bindfs setup since NFS volumes are already mounted to /opt/src and /opt/_avocado
    pub fn create_entrypoint_script_for_remote(
        &self,
        source_environment: bool,
        extension_sysroot: Option<&str>,
        runtime_sysroot: Option<&str>,
        target: &str,
        _no_bootstrap: bool,
        disable_weak_dependencies: bool,
    ) -> String {
        // Conditionally add install_weak_deps flag
        let weak_deps_flag = if disable_weak_dependencies {
            "--setopt=install_weak_deps=0 \\\n"
        } else {
            ""
        };

        // For remote execution:
        // - NFS src volume is mounted to /mnt/src (needs bindfs for UID mapping)
        // - NFS state volume is mounted directly to /opt/_avocado (no mapping needed)
        let mut script = format!(
            r#"
set -e

# Remote execution mode - NFS volumes mounted
if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Remote execution mode - using NFS-mounted volumes"; fi

# Remount source directory with permission translation via bindfs
# This maps host UID/GID to root inside the container for seamless file access
mkdir -p /opt/src

# Check if bindfs is available
if ! command -v bindfs >/dev/null 2>&1; then
    echo "[ERROR] bindfs is not installed in this container image."
    echo ""
    echo "bindfs is required for proper file permission handling."
    exit 1
fi

if [ -n "$AVOCADO_HOST_UID" ] && [ -n "$AVOCADO_HOST_GID" ]; then
    # If host user is already root (UID 0), no mapping needed - just bind mount
    if [ "$AVOCADO_HOST_UID" = "0" ] && [ "$AVOCADO_HOST_GID" = "0" ]; then
        mount --bind /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (host is root, no mapping needed)"; fi
    else
        # Use --map with colon-separated user and group mappings
        # Maps host UID -> 0 (root) and host GID -> 0 (root group)
        bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src with UID/GID mapping ($AVOCADO_HOST_UID:$AVOCADO_HOST_GID -> 0:0)"; fi
    fi
else
    # Fallback: simple bind mount without permission translation
    mount --bind /mnt/src /opt/src
    if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (no UID/GID mapping)"; fi
fi

# Get repo url from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_URL" ]; then
    REPO_URL="$AVOCADO_SDK_REPO_URL"
else
    REPO_URL="https://repo.avocadolinux.org"
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo URL: '$REPO_URL'"; fi

# Get repo release from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_RELEASE" ]; then
    REPO_RELEASE="$AVOCADO_SDK_REPO_RELEASE"
else
    REPO_RELEASE="https://repo.avocadolinux.org"

    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        REPO_RELEASE=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    REPO_RELEASE=${{REPO_RELEASE:-dev}}
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo release: '$REPO_RELEASE'"; fi

export AVOCADO_PREFIX="/opt/_avocado/${{AVOCADO_TARGET}}"
export AVOCADO_SDK_ARCH="$(uname -m)"
export AVOCADO_SDK_PREFIX="${{AVOCADO_PREFIX}}/sdk/${{AVOCADO_SDK_ARCH}}"
export AVOCADO_EXT_SYSROOTS="${{AVOCADO_PREFIX}}/extensions"
export DNF_SDK_HOST_PREFIX="${{AVOCADO_SDK_PREFIX}}"
export DNF_SDK_TARGET_PREFIX="${{AVOCADO_SDK_PREFIX}}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
{weak_deps_flag}--setopt=check_config_file_age=0 \
${{AVOCADO_DNF_ARGS:-}} \
"

export DNF_NO_SCRIPTS="--setopt=tsflags=noscripts"
export SSL_CERT_FILE=${{AVOCADO_SDK_PREFIX}}/etc/ssl/certs/ca-certificates.crt

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${{DNF_SDK_HOST_PREFIX}}/var/cache \
--setopt=logdir=${{DNF_SDK_HOST_PREFIX}}/var/log \
--setopt=persistdir=${{DNF_SDK_HOST_PREFIX}}/var/lib/dnf \
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_HOST_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

mkdir -p /etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/target-repoconf/etc/dnf/vars

echo "${{REPO_URL}}" > /etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars/repo_url
"#
        );

        script.push_str(
            r#"
export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

"#,
        );

        // Conditionally change to sysroot directory or default to /opt/src
        if let Some(extension_name) = extension_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/extensions/{extension_name}\n"
            ));
        } else if let Some(runtime_name) = runtime_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/runtimes/{runtime_name}\n"
            ));
        } else {
            script.push_str("cd /opt/src\n");
        }

        // Conditionally add environment sourcing based on the source_environment parameter
        if source_environment {
            script.push_str(
                r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"

    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
fi
"#,
            );
        }

        script
    }

    /// Create the entrypoint script for SDK initialization
    pub fn create_entrypoint_script(
        &self,
        source_environment: bool,
        extension_sysroot: Option<&str>,
        runtime_sysroot: Option<&str>,
        target: &str,
        _no_bootstrap: bool,
        disable_weak_dependencies: bool,
    ) -> String {
        // Conditionally add install_weak_deps flag
        let weak_deps_flag = if disable_weak_dependencies {
            "--setopt=install_weak_deps=0 \\\n"
        } else {
            ""
        };

        let mut script = format!(
            r#"
set -e

# Remount source directory with permission translation via bindfs
# This maps host UID/GID to root inside the container for seamless file access
mkdir -p /opt/src

# Check if bindfs is available
if ! command -v bindfs >/dev/null 2>&1; then
    echo "[ERROR] bindfs is not installed in this container image."
    echo ""
    echo "bindfs is required for proper file permission handling between the host and container."
    echo ""
    echo "To install bindfs in your container image, add one of the following to your Dockerfile:"
    echo ""
    echo "  # For Ubuntu/Debian-based images:"
    echo "  RUN apt-get update && apt-get install -y bindfs"
    echo ""
    echo "  # For Fedora/RHEL-based images:"
    echo "  RUN dnf install -y bindfs"
    echo ""
    echo "  # For Alpine-based images:"
    echo "  RUN apk add --no-cache bindfs"
    echo ""
    echo "  # For Arch-based images:"
    echo "  RUN pacman -S --noconfirm bindfs"
    echo ""
    exit 1
fi

if [ -n "$AVOCADO_HOST_UID" ] && [ -n "$AVOCADO_HOST_GID" ]; then
    # If host user is already root (UID 0), no mapping needed - just bind mount
    if [ "$AVOCADO_HOST_UID" = "0" ] && [ "$AVOCADO_HOST_GID" = "0" ]; then
        mount --bind /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (host is root, no mapping needed)"; fi
    else
        # Use --map with colon-separated user and group mappings
        # Maps host UID -> 0 (root) and host GID -> 0 (root group)
        # Format: --map=uid1/uid2:@gid1/@gid2
        bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 /mnt/src /opt/src
        if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src with UID/GID mapping ($AVOCADO_HOST_UID:$AVOCADO_HOST_GID -> 0:0)"; fi
    fi
else
    # Fallback: simple bind mount without permission translation
    mount --bind /mnt/src /opt/src
    if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Mounted /mnt/src -> /opt/src (no UID/GID mapping)"; fi
fi

# Get repo url from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_URL" ]; then
    REPO_URL="$AVOCADO_SDK_REPO_URL"
else
    REPO_URL="https://repo.avocadolinux.org"
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo URL: '$REPO_URL'"; fi

# Get repo release from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_RELEASE" ]; then
    REPO_RELEASE="$AVOCADO_SDK_REPO_RELEASE"
else
    REPO_RELEASE="https://repo.avocadolinux.org"

    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        REPO_RELEASE=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    REPO_RELEASE=${{REPO_RELEASE:-dev}}
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo release: '$REPO_RELEASE'"; fi

export AVOCADO_PREFIX="/opt/_avocado/${{AVOCADO_TARGET}}"
export AVOCADO_SDK_ARCH="$(uname -m)"
export AVOCADO_SDK_PREFIX="${{AVOCADO_PREFIX}}/sdk/${{AVOCADO_SDK_ARCH}}"
export AVOCADO_EXT_SYSROOTS="${{AVOCADO_PREFIX}}/extensions"
export DNF_SDK_HOST_PREFIX="${{AVOCADO_SDK_PREFIX}}"
export DNF_SDK_TARGET_PREFIX="${{AVOCADO_SDK_PREFIX}}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
{weak_deps_flag}--setopt=check_config_file_age=0 \
${{AVOCADO_DNF_ARGS:-}} \
"

export DNF_NO_SCRIPTS="--setopt=tsflags=noscripts"
export SSL_CERT_FILE=${{AVOCADO_SDK_PREFIX}}/etc/ssl/certs/ca-certificates.crt

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${{DNF_SDK_HOST_PREFIX}}/var/cache \
--setopt=logdir=${{DNF_SDK_HOST_PREFIX}}/var/log \
--setopt=persistdir=${{DNF_SDK_HOST_PREFIX}}/var/lib/dnf \
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_HOST_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

mkdir -p /etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/target-repoconf/etc/dnf/vars

echo "${{REPO_URL}}" > /etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars/repo_url
"#
        );

        script.push_str(
            r#"
export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

"#,
        );

        // Conditionally change to sysroot directory or default to /opt/src
        if let Some(extension_name) = extension_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/extensions/{extension_name}\n"
            ));
        } else if let Some(runtime_name) = runtime_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/runtimes/{runtime_name}\n"
            ));
        } else {
            script.push_str("cd /opt/src\n");
        }

        // Conditionally add environment sourcing based on the source_environment parameter
        if source_environment {
            script.push_str(
                r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"

    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
fi
"#,
            );
        }

        script
    }

    /// Parse a container argument, splitting on spaces while respecting quotes
    fn parse_container_arg(arg: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let chars = arg.chars().peekable();

        for ch in chars {
            match ch {
                '"' => {
                    in_quotes = !in_quotes;
                }
                ' ' if !in_quotes => {
                    if !current.is_empty() {
                        result.push(current.trim().to_string());
                        current.clear();
                    }
                }
                _ => {
                    current.push(ch);
                }
            }
        }

        if !current.is_empty() {
            result.push(current.trim().to_string());
        }

        // If no spaces were found and no quotes, or if result is empty, return the original string
        if (result.len() == 1 && result[0] == arg) || result.is_empty() {
            vec![arg.to_string()]
        } else {
            result
        }
    }

    /// Write signature files to a Docker volume using docker cp
    ///
    /// This creates a temporary container, copies signature files into it,
    /// then removes the container.
    pub async fn write_signatures_to_volume(
        &self,
        volume_name: &str,
        signatures: &[crate::utils::image_signing::SignatureData],
    ) -> Result<()> {
        if signatures.is_empty() {
            return Ok(());
        }

        // Create temporary directory for signature files
        let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

        // Write signature files to temp directory with flattened names
        let mut file_mappings = Vec::new();
        for (idx, sig) in signatures.iter().enumerate() {
            let temp_file_name = format!("sig_{}.json", idx);
            let temp_file_path = temp_dir.path().join(&temp_file_name);
            std::fs::write(&temp_file_path, &sig.content).with_context(|| {
                format!(
                    "Failed to write signature file to temp: {}",
                    temp_file_path.display()
                )
            })?;

            file_mappings.push((temp_file_path, sig.container_path.clone()));
        }

        // Create a temporary container with the volume mounted
        let container_name = format!("avocado-sig-writer-{}", uuid::Uuid::new_v4());
        let volume_mount = format!("{}:/opt/_avocado:rw", volume_name);

        let create_cmd = [
            &self.container_tool,
            &"create".to_string(),
            &"--name".to_string(),
            &container_name,
            &"-v".to_string(),
            &volume_mount,
            &"alpine:latest".to_string(),
            &"true".to_string(),
        ];

        if self.verbose {
            print_info(
                &format!(
                    "Creating temporary container for signature writing: {}",
                    container_name
                ),
                OutputLevel::Verbose,
            );
        }

        let mut cmd = AsyncCommand::new(create_cmd[0]);
        cmd.args(&create_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .context("Failed to create temporary container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create temporary container: {}", stderr);
        }

        // Copy each signature file into the container
        for (temp_path, container_path) in &file_mappings {
            let temp_path_str = temp_path.display().to_string();
            let container_dest = format!("{}:{}", container_name, container_path);

            let cp_cmd = [
                &self.container_tool,
                &"cp".to_string(),
                &temp_path_str,
                &container_dest,
            ];

            if self.verbose {
                print_info(
                    &format!("Copying signature to {}", container_path),
                    OutputLevel::Verbose,
                );
            }

            let mut cmd = AsyncCommand::new(cp_cmd[0]);
            cmd.args(&cp_cmd[1..]);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            let output = cmd.output().await.with_context(|| {
                format!(
                    "Failed to copy signature file to container: {}",
                    container_path
                )
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);

                // Clean up container before returning error
                let _ = self.remove_container(&container_name).await;

                anyhow::bail!(
                    "Failed to copy signature file {}: {}",
                    container_path,
                    stderr
                );
            }
        }

        // Remove the temporary container
        self.remove_container(&container_name).await?;

        if self.verbose {
            print_info(
                &format!(
                    "Successfully wrote {} signature file(s) to volume",
                    signatures.len()
                ),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Remove a container by name
    async fn remove_container(&self, container_name: &str) -> Result<()> {
        let container_name_str = container_name.to_string();
        let rm_cmd = [
            &self.container_tool,
            &"rm".to_string(),
            &"-f".to_string(),
            &container_name_str,
        ];

        let mut cmd = AsyncCommand::new(rm_cmd[0]);
        cmd.args(&rm_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .context("Failed to remove temporary container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.verbose {
                print_error(
                    &format!(
                        "Warning: Failed to remove temporary container {}: {}",
                        container_name, stderr
                    ),
                    OutputLevel::Verbose,
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sdk_container_creation() {
        let container = SdkContainer::new();
        assert_eq!(container.container_tool, "docker");
        assert!(!container.verbose);
    }

    #[test]
    fn test_sdk_container_with_tool() {
        let container = SdkContainer::with_tool("podman".to_string());
        assert_eq!(container.container_tool, "podman");
    }

    #[test]
    fn test_sdk_container_verbose() {
        let container = SdkContainer::new().verbose(true);
        assert!(container.verbose);
    }

    #[test]
    fn test_build_container_command() {
        use crate::utils::volume::VolumeState;
        let container = SdkContainer::new();
        let command = vec!["echo".to_string(), "test".to_string()];
        let env_vars = HashMap::new();
        let volume_state = VolumeState::new(std::env::current_dir().unwrap(), "docker".to_string());

        let config = RunConfig {
            container_image: "test-image".to_string(),
            target: "test-target".to_string(),
            command: "".to_string(),
            container_name: None,
            detach: false,
            rm: true,
            env_vars: None,
            verbose: false,
            source_environment: false,
            use_entrypoint: false,
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: None,
            dnf_args: None,
            extension_sysroot: None,
            runtime_sysroot: None,
            no_bootstrap: false,
            disable_weak_dependencies: false,
            signing_socket_path: None,
            signing_helper_script_path: None,
            signing_key_name: None,
            signing_checksum_algorithm: None,
            runs_on: None,
            nfs_port: None,
        };

        let result = container.build_container_command(&config, &command, &env_vars, &volume_state);

        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.contains(&"docker".to_string()));
        assert!(cmd.contains(&"run".to_string()));
        assert!(cmd.contains(&"--rm".to_string()));
        assert!(cmd.contains(&"test-image".to_string()));
        assert!(cmd.contains(&"echo".to_string()));
        assert!(cmd.contains(&"test".to_string()));
        // Verify AVOCADO_SRC_DIR is set
        assert!(cmd.contains(&"AVOCADO_SRC_DIR=/opt/src".to_string()));
        // Verify FUSE device and capability for bindfs support
        assert!(cmd.contains(&"--device".to_string()));
        assert!(cmd.contains(&"/dev/fuse".to_string()));
        assert!(cmd.contains(&"--cap-add".to_string()));
        assert!(cmd.contains(&"SYS_ADMIN".to_string()));
        // Verify host UID/GID are passed as env vars
        let has_uid_env = cmd.iter().any(|s| s.starts_with("AVOCADO_HOST_UID="));
        let has_gid_env = cmd.iter().any(|s| s.starts_with("AVOCADO_HOST_GID="));
        assert!(has_uid_env, "AVOCADO_HOST_UID should be set");
        assert!(has_gid_env, "AVOCADO_HOST_GID should be set");
        // Verify source mount uses /mnt/src (bindfs will remount to /opt/src)
        let has_mnt_src_mount = cmd.iter().any(|s| s.contains(":/mnt/src:"));
        assert!(has_mnt_src_mount, "Source should be mounted to /mnt/src");
    }

    #[test]
    fn test_entrypoint_script() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, None, None, "x86_64", false, false);
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));
        assert!(script.contains("environment-setup"));
        assert!(script.contains("cd /opt/src"));
        // Verify bindfs check is included
        assert!(script.contains("command -v bindfs"));
        assert!(script.contains("[ERROR] bindfs is not installed"));
        // Verify bindfs setup is included with correct syntax
        // --map=uid1/uid2:@gid1/@gid2 for combined user and group mapping
        assert!(script
            .contains("bindfs --map=$AVOCADO_HOST_UID/0:@$AVOCADO_HOST_GID/@0 /mnt/src /opt/src"));
        assert!(script.contains("mkdir -p /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_extension_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(
            true,
            Some("test-ext"),
            None,
            "x86_64",
            false,
            false,
        );
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/extensions/test-ext"));
        assert!(!script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_runtime_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(
            true,
            None,
            Some("test-runtime"),
            "x86_64",
            false,
            false,
        );
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/runtimes/test-runtime"));
        assert!(!script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_no_bootstrap() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, None, None, "x86_64", true, false);

        // Should still contain environment variables
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));

        // Should NOT contain bootstrap initialization
        assert!(!script.contains("Initializing Avocado SDK"));
        assert!(!script.contains("install \"avocado-sdk-"));
        assert!(!script.contains("install avocado-sdk-toolchain"));
        assert!(!script.contains("Installing rootfs sysroot"));

        // Should still change to /opt/src
        assert!(script.contains("cd /opt/src"));

        // Should still contain environment sourcing (this is separate from bootstrap)
        assert!(script.contains("source \"${AVOCADO_SDK_PREFIX}/environment-setup\""));
    }

    #[test]
    fn test_parse_container_arg_single() {
        let result = SdkContainer::parse_container_arg("--rm");
        assert_eq!(result, vec!["--rm"]);
    }

    #[test]
    fn test_parse_container_arg_with_spaces() {
        let result = SdkContainer::parse_container_arg("-v /host:/container");
        assert_eq!(result, vec!["-v", "/host:/container"]);
    }

    #[test]
    fn test_parse_container_arg_with_quotes() {
        let result = SdkContainer::parse_container_arg("-v \"/path with spaces:/container\"");
        assert_eq!(result, vec!["-v", "/path with spaces:/container"]);
    }

    #[test]
    fn test_parse_container_arg_complex() {
        let result = SdkContainer::parse_container_arg("-e \"VAR=value with spaces\" --name test");
        assert_eq!(
            result,
            vec!["-e", "VAR=value with spaces", "--name", "test"]
        );
    }
}
