#[cfg(unix)]
use crate::utils::signing_service::{generate_helper_script, SigningService, SigningServiceConfig};
use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    remote::{RemoteHost, SshClient},
    stamps::{
        generate_batch_read_stamps_script, generate_write_stamp_script,
        resolve_required_stamps_for_arch, validate_stamps_batch, Stamp, StampCommand,
        StampComponent, StampInputs, StampOutputs,
    },
    target::resolve_target_required,
    volume::VolumeManager,
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct RuntimeProvisionConfig {
    pub runtime_name: String,
    pub config_path: String,
    pub verbose: bool,
    pub force: bool,
    pub target: Option<String>,
    pub provision_profile: Option<String>,
    pub env_vars: Option<HashMap<String, String>>,
    pub out: Option<String>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    /// Path to state file relative to src_dir for persisting state between provision runs.
    /// Resolved from provision profile config or defaults to `.avocado/provision-{profile}.state`.
    pub state_file: Option<String>,
    /// Disable stamp validation and writing
    pub no_stamps: bool,
    /// Remote host to run on (format: user@host)
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
}

pub struct RuntimeProvisionCommand {
    config: RuntimeProvisionConfig,
    #[cfg(unix)]
    signing_service: Option<SigningService>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RuntimeProvisionCommand {
    pub fn new(config: RuntimeProvisionConfig) -> Self {
        Self {
            config,
            #[cfg(unix)]
            signing_service: None,
            composed_config: None,
        }
    }

    /// Set pre-composed configuration to avoid reloading
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&mut self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(
                &self.config.config_path,
                self.config.target.as_deref(),
            )?),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        // Get runtime configuration
        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        // Check if runtime exists
        let runtime_spec = runtime_config
            .get(&self.config.runtime_name)
            .with_context(|| {
                format!(
                    "Runtime '{}' not found in configuration",
                    self.config.runtime_name
                )
            })?;

        // Get target from runtime config
        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.config.target.as_deref(), config)?;

        // Detect remote host architecture if using --runs-on
        // This is needed to check if the SDK is installed for the remote's architecture
        let remote_arch = if let Some(ref runs_on) = self.config.runs_on {
            let remote_host = RemoteHost::parse(runs_on)?;
            let ssh = SshClient::new(remote_host).with_verbose(self.config.verbose);
            let arch = ssh.get_architecture().await.with_context(|| {
                format!("Failed to detect architecture of remote host '{runs_on}'")
            })?;
            if self.config.verbose {
                print_info(
                    &format!("Remote host architecture: {arch}"),
                    OutputLevel::Normal,
                );
            }
            Some(arch)
        } else {
            None
        };

        // Validate stamps before proceeding (unless --no-stamps)
        if !self.config.no_stamps {
            let container_helper = SdkContainer::from_config(&self.config.config_path, config)?
                .verbose(self.config.verbose);

            // Provision requires runtime build stamp
            // When using --runs-on, check for SDK stamp matching remote's architecture
            let required = resolve_required_stamps_for_arch(
                StampCommand::Provision,
                StampComponent::Runtime,
                Some(&self.config.runtime_name),
                &[],
                remote_arch.as_deref(),
            );

            // Batch all stamp reads into a single container invocation for performance
            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                runs_on: self.config.runs_on.clone(),
                nfs_port: self.config.nfs_port,
                sdk_arch: self.config.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            // If output is None, the container command failed - show a helpful error
            let output_str = match output {
                Some(ref s) => s.as_str(),
                None => {
                    return Err(anyhow::anyhow!(
                        "Failed to check stamps: container command failed.\n\
                        This may be caused by mount permission issues.\n\
                        Try running with --verbose or use --no-stamps to skip validation."
                    ));
                }
            };

            // Validate all stamps from batch output
            let validation = validate_stamps_batch(&required, output_str, None);

            if !validation.is_satisfied() {
                // Include the --runs-on target in error message for SDK install hints
                let error = validation.into_error_with_runs_on(
                    &format!("Cannot provision runtime '{}'", self.config.runtime_name),
                    self.config.runs_on.as_deref(),
                );
                return Err(error.into());
            }
        }

        print_info(
            &format!("Provisioning runtime '{}'", self.config.runtime_name),
            OutputLevel::Normal,
        );

        // Determine extensions required for this runtime/target combination
        // This includes local extensions, external extensions, and versioned extensions from ext repos
        // For package repository extensions, we query the RPM database to get actual installed versions
        let resolved_extensions = self
            .collect_runtime_extensions(
                parsed,
                config,
                &self.config.runtime_name,
                target_arch.as_str(),
                &self.config.config_path,
                container_image,
            )
            .await?;

        // Merge CLI env vars with AVOCADO_EXT_LIST if any extensions exist
        let mut env_vars = self.config.env_vars.clone().unwrap_or_default();
        if !resolved_extensions.is_empty() {
            env_vars.insert(
                "AVOCADO_EXT_LIST".to_string(),
                resolved_extensions.join(" "),
            );
        }

        // Set AVOCADO_VERBOSE=1 when verbose mode is enabled
        if self.config.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Set standard avocado environment variables for provision scripts
        // AVOCADO_TARGET - Used for all bundle.manifest.[].target values
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.clone());

        // AVOCADO_RUNTIME_NAME - Runtime name (e.g., "dev")
        env_vars.insert(
            "AVOCADO_RUNTIME_NAME".to_string(),
            self.config.runtime_name.clone(),
        );

        // AVOCADO_RUNTIME_VERSION - Runtime version from distro.version (e.g., "0.1.0")
        if let Some(distro_version) = config.get_distro_version() {
            env_vars.insert(
                "AVOCADO_RUNTIME_VERSION".to_string(),
                distro_version.clone(),
            );
        }

        // Set AVOCADO_PROVISION_OUT if --out is specified
        if let Some(out_path) = &self.config.out {
            // Construct the absolute path from the container's perspective
            // The src_dir is mounted at /opt/src in the container
            let container_out_path = format!("/opt/src/{out_path}");
            env_vars.insert("AVOCADO_PROVISION_OUT".to_string(), container_out_path);
        }

        // Set AVOCADO_STONE_INCLUDE_PATHS if configured
        if let Some(stone_paths) = config.get_stone_include_paths_for_runtime(
            &self.config.runtime_name,
            &target_arch,
            &self.config.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_INCLUDE_PATHS".to_string(), stone_paths);
        }

        // Set AVOCADO_STONE_MANIFEST if configured
        if let Some(stone_manifest) = config.get_stone_manifest_for_runtime(
            &self.config.runtime_name,
            &target_arch,
            &self.config.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_MANIFEST".to_string(), stone_manifest);
        }

        // Set AVOCADO_RUNTIME_BUILD_DIR
        env_vars.insert(
            "AVOCADO_RUNTIME_BUILD_DIR".to_string(),
            format!(
                "/opt/_avocado/{}/runtimes/{}",
                target_arch, self.config.runtime_name
            ),
        );

        // Set AVOCADO_DISTRO_VERSION if configured
        if let Some(distro_version) = config.get_distro_version() {
            env_vars.insert("AVOCADO_DISTRO_VERSION".to_string(), distro_version.clone());
        }

        // Determine state file path and container location if a provision profile is set
        let state_file_info = if let Some(profile) = &self.config.provision_profile {
            let state_file_path = self
                .config
                .state_file
                .clone()
                .unwrap_or_else(|| config.get_provision_state_file(profile));
            let container_state_path = format!(
                "/opt/_avocado/{}/output/runtimes/{}/provision-state.state",
                target_arch, self.config.runtime_name
            );
            env_vars.insert(
                "AVOCADO_PROVISION_STATE".to_string(),
                container_state_path.clone(),
            );
            Some((state_file_path, container_state_path))
        } else {
            None
        };

        let env_vars = if env_vars.is_empty() {
            None
        } else {
            Some(env_vars)
        };

        // Copy state file to container volume if it exists
        let src_dir = std::env::current_dir()?;
        let state_file_existed =
            if let Some((ref state_file_path, ref container_state_path)) = state_file_info {
                self.copy_state_to_container(
                    &src_dir,
                    state_file_path,
                    container_state_path,
                    &target_arch,
                    container_image,
                )
                .await?
            } else {
                false
            };

        // Check if runtime has signing configured
        let signing_config = self.setup_signing_service(config).await?;

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Create provision script
        let provision_script = self.create_provision_script(&target_arch)?;

        if self.config.verbose {
            print_info("Executing provision script.", OutputLevel::Normal);
        }

        let mut run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: provision_script,
            verbose: self.config.verbose,
            source_environment: true,
            interactive: !self.config.force,
            env_vars,
            container_args: config.merge_provision_container_args(
                self.config.provision_profile.as_deref(),
                self.config.container_args.as_ref(),
            ),
            dnf_args: self.config.dnf_args.clone(),
            runs_on: self.config.runs_on.clone(),
            nfs_port: self.config.nfs_port,
            sdk_arch: self.config.sdk_arch.clone(),
            ..Default::default()
        };

        // Add signing configuration to run_config if available
        if let Some((socket_path, helper_script_path, key_name, checksum_algo)) = &signing_config {
            run_config.signing_socket_path = Some(socket_path.clone());
            run_config.signing_helper_script_path = Some(helper_script_path.clone());
            run_config.signing_key_name = Some(key_name.clone());
            run_config.signing_checksum_algorithm = Some(checksum_algo.clone());
        }

        let provision_result = container_helper
            .run_in_container(run_config)
            .await
            .context("Failed to provision runtime")?;

        // Shutdown signing service if it was started
        if signing_config.is_some() {
            self.cleanup_signing_service().await?;
        }

        if !provision_result {
            return Err(anyhow::anyhow!("Failed to provision runtime"));
        }

        // Note: File ownership is automatically handled by bindfs permission translation,
        // so no explicit chown is needed for the output directory.

        // Copy state file back from container if it exists
        if let Some((ref state_file_path, ref container_state_path)) = state_file_info {
            self.copy_state_from_container(
                &src_dir,
                state_file_path,
                container_state_path,
                &target_arch,
                state_file_existed,
                container_image,
            )
            .await?;
        }

        print_success(
            &format!(
                "Successfully provisioned runtime '{}'",
                self.config.runtime_name
            ),
            OutputLevel::Normal,
        );

        // Write provision stamp (unless --no-stamps)
        if !self.config.no_stamps {
            let container_helper = SdkContainer::from_config(&self.config.config_path, config)?
                .verbose(self.config.verbose);

            let inputs = StampInputs::new("provision".to_string());
            let outputs = StampOutputs::default();
            let stamp =
                Stamp::runtime_provision(&self.config.runtime_name, &target_arch, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: stamp_script,
                verbose: self.config.verbose,
                source_environment: true,
                interactive: false,
                runs_on: self.config.runs_on.clone(),
                nfs_port: self.config.nfs_port,
                sdk_arch: self.config.sdk_arch.clone(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

            if self.config.verbose {
                print_info(
                    &format!(
                        "Wrote provision stamp for runtime '{}'.",
                        self.config.runtime_name
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(())
    }

    /// Setup signing service if signing is configured for the runtime
    ///
    /// Returns Some((socket_path, helper_script_path, key_name, checksum_algorithm)) if signing is enabled
    #[cfg(unix)]
    async fn setup_signing_service(
        &mut self,
        config: &crate::utils::config::Config,
    ) -> Result<Option<(PathBuf, PathBuf, String, String)>> {
        // Check if runtime has signing configuration
        let signing_key_name = match config.get_runtime_signing_key(&self.config.runtime_name) {
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
                if self.config.verbose {
                    print_info(
                        "No signing key configured for runtime. Signing service will not be started.",
                        OutputLevel::Verbose,
                    );
                }
                return Ok(None);
            }
        };

        let keyid = config
            .get_runtime_signing_key(&self.config.runtime_name)
            .context("Failed to get signing key ID")?;

        // Get checksum algorithm (defaults to sha256)
        let checksum_str = config
            .runtimes
            .as_ref()
            .and_then(|r| r.get(&self.config.runtime_name))
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

        if self.config.verbose {
            print_info(
                &format!(
                    "Starting signing service with key '{signing_key_name}' using {checksum_str} checksums"
                ),
                OutputLevel::Verbose,
            );
        }

        // Start signing service
        // Note: Hash computation happens in the container, so we don't need volume access
        let service_config = SigningServiceConfig {
            socket_path: socket_path.clone(),
            runtime_name: self.config.runtime_name.clone(),
            key_name: signing_key_name.clone(),
            keyid,
            verbose: self.config.verbose,
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
    /// Signing service requires Unix domain sockets and is not available on Windows
    #[cfg(not(unix))]
    async fn setup_signing_service(
        &mut self,
        _config: &crate::utils::config::Config,
    ) -> Result<Option<(PathBuf, PathBuf, String, String)>> {
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

    fn create_provision_script(&self, target_arch: &str) -> Result<String> {
        let script = format!(
            r#"
echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-provision' for '{}'."
avocado-provision-{} {}
"#,
            self.config.runtime_name, target_arch, self.config.runtime_name
        );

        Ok(script)
    }

    /// Copy state file from src_dir to container volume before provisioning.
    /// Returns true if the state file existed and was copied, false otherwise.
    async fn copy_state_to_container(
        &self,
        src_dir: &std::path::Path,
        state_file_path: &str,
        container_state_path: &str,
        _target_arch: &str,
        container_image: &str,
    ) -> Result<bool> {
        let host_state_file = src_dir.join(state_file_path);

        // Check if the state file exists on the host
        if !host_state_file.exists() {
            if self.config.verbose {
                print_info(
                    &format!(
                        "No existing state file at {}, starting fresh",
                        host_state_file.display()
                    ),
                    OutputLevel::Verbose,
                );
            }
            return Ok(false);
        }

        if self.config.verbose {
            print_info(
                &format!(
                    "Copying state file from {} to container at {}",
                    host_state_file.display(),
                    container_state_path
                ),
                OutputLevel::Verbose,
            );
        }

        let container_tool = "docker";
        let volume_manager = VolumeManager::new(container_tool.to_string(), self.config.verbose);
        let volume_state = volume_manager.get_or_create_volume(src_dir).await?;

        // Ensure parent directory exists and copy file to container
        let copy_script = format!(
            "mkdir -p \"$(dirname '{container_state_path}')\" && cp '/opt/src/{state_file_path}' '{container_state_path}'"
        );

        let mut copy_cmd = vec![
            container_tool.to_string(),
            "run".to_string(),
            "--rm".to_string(),
        ];

        // Mount the source directory
        copy_cmd.push("-v".to_string());
        copy_cmd.push(format!("{}:/opt/src:ro", src_dir.display()));

        // Mount the volume
        copy_cmd.push("-v".to_string());
        copy_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Add the container image
        copy_cmd.push(container_image.to_string());

        // Add the command
        copy_cmd.push("bash".to_string());
        copy_cmd.push("-c".to_string());
        copy_cmd.push(copy_script);

        if self.config.verbose {
            print_info(
                &format!("Running: {}", copy_cmd.join(" ")),
                OutputLevel::Verbose,
            );
        }

        let mut cmd = tokio::process::Command::new(&copy_cmd[0]);
        cmd.args(&copy_cmd[1..]);
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let status = cmd
            .status()
            .await
            .context("Failed to copy state file to container")?;

        if !status.success() {
            print_info(
                "Warning: Failed to copy state file to container",
                OutputLevel::Normal,
            );
            return Ok(false);
        }

        if self.config.verbose {
            print_info(
                "Successfully copied state file to container",
                OutputLevel::Verbose,
            );
        }

        Ok(true)
    }

    /// Copy state file from container volume back to src_dir after provisioning.
    /// Only copies if the file exists in the container. If the file is empty and
    /// the original didn't exist, no file is copied.
    async fn copy_state_from_container(
        &self,
        src_dir: &std::path::Path,
        state_file_path: &str,
        container_state_path: &str,
        _target_arch: &str,
        _original_existed: bool,
        container_image: &str,
    ) -> Result<()> {
        if self.config.verbose {
            print_info(
                &format!("Checking for state file at {container_state_path} in container"),
                OutputLevel::Verbose,
            );
        }

        let container_tool = "docker";
        let volume_manager = VolumeManager::new(container_tool.to_string(), self.config.verbose);
        let volume_state = volume_manager.get_or_create_volume(src_dir).await?;

        // Check if the state file exists in the container
        let check_script = format!("test -f '{container_state_path}'");

        let mut check_cmd = vec![
            container_tool.to_string(),
            "run".to_string(),
            "--rm".to_string(),
        ];

        check_cmd.push("-v".to_string());
        check_cmd.push(format!("{}:/opt/_avocado:ro", volume_state.volume_name));

        check_cmd.push(container_image.to_string());
        check_cmd.push("bash".to_string());
        check_cmd.push("-c".to_string());
        check_cmd.push(check_script);

        let mut cmd = tokio::process::Command::new(&check_cmd[0]);
        cmd.args(&check_cmd[1..]);
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let status = cmd
            .status()
            .await
            .context("Failed to check state file existence")?;

        if !status.success() {
            // State file doesn't exist in container
            if self.config.verbose {
                print_info(
                    "No state file found in container, nothing to copy back",
                    OutputLevel::Verbose,
                );
            }
            return Ok(());
        }

        // State file exists - copy it back to host
        let host_state_file = src_dir.join(state_file_path);

        if self.config.verbose {
            print_info(
                &format!(
                    "Copying state file from container to {}",
                    host_state_file.display()
                ),
                OutputLevel::Verbose,
            );
        }

        // Ensure parent directory exists on host
        if let Some(parent) = host_state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Copy from container to host src_dir
        // Note: File ownership is automatically handled by bindfs permission translation
        let copy_script = format!("cp '{container_state_path}' '/opt/src/{state_file_path}'");

        // Use shared SdkContainer for running the command
        let container_helper = SdkContainer::new()
            .with_src_dir(Some(src_dir.to_path_buf()))
            .verbose(self.config.verbose);

        let success = container_helper
            .run_simple_command(container_image, &copy_script, true)
            .await?;

        if !success {
            print_info(
                "Warning: Failed to copy state file from container",
                OutputLevel::Normal,
            );
        } else if self.config.verbose {
            print_info(
                &format!(
                    "Successfully copied state file to {}",
                    host_state_file.display()
                ),
                OutputLevel::Verbose,
            );
        }

        Ok(())
    }

    async fn collect_runtime_extensions(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        runtime_name: &str,
        target_arch: &str,
        config_path: &str,
        container_image: &str,
    ) -> Result<Vec<String>> {
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target_arch, config_path)?;

        let runtime_dep_table = merged_runtime
            .as_ref()
            .and_then(|value| value.get("packages").and_then(|d| d.as_mapping()))
            .or_else(|| {
                parsed
                    .get("runtimes")
                    .and_then(|r| r.get(runtime_name))
                    .and_then(|runtime_value| runtime_value.get("packages"))
                    .and_then(|d| d.as_mapping())
            });

        let mut extensions = Vec::new();

        if let Some(deps) = runtime_dep_table {
            for dep_spec in deps.values() {
                if let Some(ext_name) = dep_spec.get("extensions").and_then(|v| v.as_str()) {
                    let version = self
                        .resolve_extension_version(
                            parsed,
                            config,
                            config_path,
                            ext_name,
                            dep_spec,
                            container_image,
                            target_arch,
                        )
                        .await?;
                    extensions.push(format!("{ext_name}-{version}"));
                }
            }
        }

        extensions.sort();
        extensions.dedup();

        Ok(extensions)
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_extension_version(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        config_path: &str,
        ext_name: &str,
        dep_spec: &serde_yaml::Value,
        container_image: &str,
        target_arch: &str,
    ) -> Result<String> {
        // If version is explicitly specified with vsn field, use it (unless it's a wildcard)
        if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If vsn is "*", fall through to query RPM for the actual installed version
        }

        // If external config is specified, try to get version from it
        if let Some(external_config_path) = dep_spec.get("config").and_then(|v| v.as_str()) {
            let external_extensions =
                config.load_external_extensions(config_path, external_config_path)?;
            if let Some(ext_config) = external_extensions.get(ext_name) {
                if let Some(version) = ext_config.get("version").and_then(|v| v.as_str()) {
                    if version != "*" {
                        return Ok(version.to_string());
                    }
                    // If version is "*", fall through to query RPM
                }
            }
            // External config but no version found or version is "*" - query RPM database
            return self
                .query_rpm_version(ext_name, container_image, target_arch)
                .await;
        }

        // Try to get version from local [ext] section
        if let Some(version) = parsed
            .get("extensions")
            .and_then(|ext_section| ext_section.as_mapping())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
        {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If version is "*", fall through to query RPM
        }

        // No version found in config - this is likely a package repository extension
        // Query RPM database for the installed version
        self.query_rpm_version(ext_name, container_image, target_arch)
            .await
    }

    /// Query RPM database for the actual installed version of an extension
    ///
    /// This queries the RPM database in the extension's sysroot at $AVOCADO_EXT_SYSROOTS/{ext_name}
    /// to get the actual installed version. This ensures AVOCADO_EXT_LIST contains
    /// precise version information.
    async fn query_rpm_version(
        &self,
        ext_name: &str,
        container_image: &str,
        target: &str,
    ) -> Result<String> {
        let container_helper = SdkContainer::new();

        let version_query_script = format!(
            r#"
set -e
# Query RPM version for extension from RPM database using the same config as installation
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
rpm --root="$AVOCADO_EXT_SYSROOTS/{ext_name}" --dbpath=/var/lib/extension.d/rpm -q {ext_name} --queryformat '%{{VERSION}}'
"#
        );

        let version_query_config = crate::utils::container::RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: version_query_script,
            verbose: self.config.verbose,
            source_environment: true,
            interactive: false,
            runs_on: self.config.runs_on.clone(),
            nfs_port: self.config.nfs_port,
            ..Default::default()
        };

        match container_helper
            .run_in_container_with_output(version_query_config)
            .await
        {
            Ok(Some(actual_version)) => {
                let trimmed_version = actual_version.trim();
                if self.config.verbose {
                    print_info(
                        &format!(
                            "Resolved extension '{ext_name}' to version '{trimmed_version}' from RPM database"
                        ),
                        OutputLevel::Normal,
                    );
                }
                Ok(trimmed_version.to_string())
            }
            Ok(None) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
            Err(e) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database: {e}. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: None,
            dnf_args: None,
            state_file: None,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.out, None);
    }

    #[test]
    fn test_create_provision_script() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: None,
            dnf_args: None,
            state_file: None,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        let script = cmd.create_provision_script("x86_64").unwrap();

        assert!(script.contains("avocado-provision-x86_64 test-runtime"));
        assert!(script.contains("Running SDK lifecycle hook 'avocado-provision'"));
    }

    // NOTE: test_collect_runtime_extensions was removed as it tested the deprecated
    // ext:/vsn: format inside runtime packages. The new format uses an extensions array.

    #[test]
    fn test_new_with_container_args() {
        let container_args = Some(vec![
            "--privileged".to_string(),
            "--network=host".to_string(),
        ]);
        let dnf_args = Some(vec!["--nogpgcheck".to_string()]);

        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: container_args.clone(),
            dnf_args: dnf_args.clone(),
            state_file: None,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.out, None);
        assert_eq!(cmd.config.container_args, container_args);
        assert_eq!(cmd.config.dnf_args, dnf_args);
    }

    #[test]
    fn test_new_with_env_vars() {
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_DEVICE_ID".to_string(), "device123".to_string());
        env_vars.insert(
            "AVOCADO_PROVISION_PROFILE".to_string(),
            "production".to_string(),
        );

        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: Some(env_vars.clone()),
            out: None,
            container_args: None,
            dnf_args: None,
            state_file: None,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert_eq!(cmd.config.env_vars, Some(env_vars));
    }
}
