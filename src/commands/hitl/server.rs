use crate::utils::config::{ComposedConfig, Config};
use crate::utils::container::{is_docker_desktop, RunConfig, SdkContainer};
use crate::utils::nfs_server::{NfsExport, HITL_DEFAULT_PORT};
use crate::utils::output::{print_debug, print_info, OutputLevel};
use crate::utils::stamps::{
    generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement,
};
use crate::utils::target::validate_and_log_target;
use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Args, Debug)]
pub struct HitlServerCommand {
    /// Path to the avocado.yaml configuration file
    #[arg(short, long, default_value = "avocado.yaml")]
    pub config_path: String,

    /// Extensions to create NFS exports for
    #[arg(short, long = "extension")]
    pub extensions: Vec<String>,

    /// Additional container arguments
    #[arg(long)]
    pub container_args: Option<Vec<String>>,

    /// Additional DNF arguments
    #[arg(long)]
    pub dnf_args: Option<Vec<String>>,

    /// Target to build for
    #[arg(short, long)]
    pub target: Option<String>,

    /// Enable verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// NFS port number to use
    pub port: Option<u16>,

    /// Disable stamp validation
    #[arg(long)]
    pub no_stamps: bool,

    /// SDK container architecture for cross-arch emulation
    #[arg(skip)]
    pub sdk_arch: Option<String>,

    /// Pre-composed configuration to avoid reloading
    #[arg(skip)]
    pub composed_config: Option<Arc<ComposedConfig>>,
}

impl HitlServerCommand {
    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Use shared target resolution logic with early validation and logging
        let target = validate_and_log_target(self.target.as_deref(), config)?;

        // Get SDK configuration
        let (container_image, repo_url, repo_release) = if let Some(sdk_config) = &config.sdk {
            let repo_url = sdk_config.repo_url.as_ref();
            let repo_release = sdk_config.repo_release.as_ref();
            let image = sdk_config
                .image
                .as_ref()
                .unwrap_or(&"docker.io/avocadolinux/sdk:apollo-edge".to_string())
                .clone();
            (image, repo_url, repo_release)
        } else {
            return Err(anyhow::anyhow!("No SDK configuration found in config file"));
        };

        if self.verbose {
            print_debug(
                &format!("Using SDK image: {container_image}"),
                OutputLevel::Normal,
            );
        }

        // Validate extension stamps before starting server (unless --no-stamps)
        if !self.no_stamps && !self.extensions.is_empty() {
            print_info("Validating extension stamps...", OutputLevel::Normal);

            // Build stamp requirements for all requested extensions
            // Each extension needs both install AND build stamps
            let mut requirements = vec![StampRequirement::sdk_install()];
            for ext_name in &self.extensions {
                requirements.push(StampRequirement::ext_install(ext_name));
                requirements.push(StampRequirement::ext_build(ext_name));
            }

            // Batch read all stamps in a single container invocation
            let batch_script = generate_batch_read_stamps_script(&requirements);
            let validation_config = RunConfig {
                container_image: container_image.clone(),
                target: target.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(validation_config)
                .await?;

            // Validate all stamps from batch output
            let validation =
                validate_stamps_batch(&requirements, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error = validation.into_error("Cannot start HITL server");
                return Err(error.into());
            }

            if self.verbose {
                print_debug(
                    &format!(
                        "All {} extension(s) have valid install and build stamps.",
                        self.extensions.len()
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        // Get the NFS port (used for both Ganesha and port publishing)
        let nfs_port = self.port.unwrap_or(HITL_DEFAULT_PORT);

        // Build container arguments with HITL-specific defaults
        // On Docker Desktop (macOS/Windows), --network=host doesn't expose ports to the
        // actual host network (only to the Linux VM), so we use explicit port publishing.
        let mut container_args = if is_docker_desktop() {
            if self.verbose {
                print_debug(
                    "Docker Desktop detected: using port publishing instead of host networking",
                    OutputLevel::Normal,
                );
            }
            vec![
                "-p".to_string(),
                format!("0.0.0.0:{}:{}", nfs_port, nfs_port),
                "--cap-add".to_string(),
                "DAC_READ_SEARCH".to_string(),
                "--init".to_string(),
            ]
        } else {
            vec![
                "--net=host".to_string(),
                "--cap-add".to_string(),
                "DAC_READ_SEARCH".to_string(),
                "--init".to_string(),
            ]
        };

        // Add any additional container arguments with environment variable expansion
        if let Some(ref additional_args) = self.container_args {
            let processed_args = Config::process_container_args(Some(additional_args));
            if let Some(processed) = processed_args {
                container_args.extend(processed);
            }
        }

        // Generate NFS export setup commands
        let export_setup = self.generate_export_setup_commands();

        // Create the command to set up netconfig symlink, ganesha symlink, exports, and start HITL server
        let setup_command = format!(
            "if [ -f \"${{AVOCADO_SDK_PREFIX}}/environment-setup\" ]; then \
             source \"${{AVOCADO_SDK_PREFIX}}/environment-setup\"; \
             fi && \
             ln -sf ${{AVOCADO_SDK_PREFIX}}/etc/netconfig /etc/netconfig && \
             mkdir -p /tmp/hitl && \
             ln -sf ${{AVOCADO_SDK_PREFIX}}/usr/var/lib/nfs/ganesha /tmp/hitl && \
             {export_setup} \
             exec avocado-hitl-server -c ${{AVOCADO_SDK_PREFIX}}/etc/avocado/hitl-nfs.conf"
        );

        if self.verbose {
            print_debug("Starting HITL server container...", OutputLevel::Normal);
            print_debug(
                &format!("Container args: {container_args:?}"),
                OutputLevel::Normal,
            );
            print_debug(
                &format!("Setup command: {setup_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image,
            target,
            command: setup_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: true,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: Some(container_args),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        container_helper.run_in_container(config).await?;

        Ok(())
    }

    /// Generate shell commands to create NFS export configuration files
    fn generate_export_setup_commands(&self) -> String {
        let mut commands = vec![
            "mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d".to_string(),
            "mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado".to_string(),
        ];

        // Add/update the hitl-nfs.conf file with the exports.d directory directive
        let config_file = "${AVOCADO_SDK_PREFIX}/etc/avocado/hitl-nfs.conf";

        // Remove any existing %dir line (may have old unexpanded variable) and add correct one
        // Use double quotes so shell expands ${AVOCADO_SDK_PREFIX} when writing
        let update_config_cmd = format!(
            "touch {config_file} && \
             sed -i '/^%dir .*\\/etc\\/avocado\\/exports\\.d$/d' {config_file} && \
             echo \"%dir ${{AVOCADO_SDK_PREFIX}}/etc/avocado/exports.d\" >> {config_file}"
        );
        commands.push(update_config_cmd);

        // Update NFS_Port if a port is specified (it's nested inside NFS_Core_Param block)
        let port = self.port.unwrap_or(HITL_DEFAULT_PORT);
        // Always update the port to ensure consistency, especially on Docker Desktop
        let port_update_cmd = format!(
            "sed -i '/NFS_Core_Param {{/,/}}/s/NFS_Port = [0-9]\\+;/NFS_Port = {port};/' {config_file}"
        );
        commands.push(port_update_cmd);

        if self.verbose && self.port.is_some() {
            commands.push(format!(
                "echo \"[DEBUG] Updated NFS_Port to {port} in NFS_Core_Param block in hitl-nfs.conf\""
            ));
        }

        if self.extensions.is_empty() {
            return format!("{} &&", commands.join(" && "));
        }

        // Use shared NfsExport to generate export configurations
        for (index, extension) in self.extensions.iter().enumerate() {
            let export_id = (index + 1) as u32;
            let extensions_path = format!("${{AVOCADO_EXT_SYSROOTS}}/{extension}");
            let pseudo_path = format!("/{extension}");

            // Create NfsExport using the shared type
            let export = NfsExport::new(export_id, PathBuf::from(&extensions_path), pseudo_path);

            // Generate the export config content using the shared method
            let export_content = Self::generate_ganesha_export_block(&export);

            let export_file =
                format!("${{AVOCADO_SDK_PREFIX}}/etc/avocado/exports.d/{extension}.conf");

            // Create a command that writes the export content to the file
            let escaped_content = export_content.replace('\\', "\\\\").replace('"', "\\\"");
            let write_command = format!("echo -e \"{escaped_content}\" > {export_file}");

            commands.push(write_command);

            if self.verbose {
                commands.push(format!(
                    "echo \"[DEBUG] Created NFS export for extension '{extension}' with Export_Id {export_id} at {extensions_path}\""
                ));
            }
        }

        format!("{} &&", commands.join(" && "))
    }

    /// Generate a Ganesha EXPORT block for the given export config
    fn generate_ganesha_export_block(export: &NfsExport) -> String {
        format!(
            "EXPORT {{\n\
            \x20\x20Export_Id = {};\n\
            \x20\x20Path = {};\n\
            \x20\x20Pseudo = {};\n\
            \x20\x20FSAL {{\n\
            \x20\x20\x20\x20name = VFS;\n\
            \x20\x20}}\n\
            }}",
            export.export_id,
            export.local_path.display(),
            export.pseudo_path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_export_setup_commands_without_port() {
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec![],
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: false,
            port: None,
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        };

        let commands = cmd.generate_export_setup_commands();

        // Should create directories and exports.d directive
        assert!(commands.contains("mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d"));
        assert!(commands.contains("echo \"%dir ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d\""));
        // Port is now always set to ensure consistency (uses default 12049)
        assert!(commands.contains("NFS_Port = 12049"));
    }

    #[test]
    fn test_generate_export_setup_commands_with_port() {
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec![],
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: false,
            port: Some(2049),
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        };

        let commands = cmd.generate_export_setup_commands();

        // Should include port update commands that search within NFS_Core_Param block
        assert!(commands.contains("NFS_Port = 2049"));
        assert!(commands.contains("/NFS_Core_Param {/,/}/s/NFS_Port = [0-9]\\+;/NFS_Port = 2049;/"));
    }

    #[test]
    fn test_generate_export_setup_commands_with_port_and_verbose() {
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec![],
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: true,
            port: Some(3049),
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        };

        let commands = cmd.generate_export_setup_commands();

        // Should include port update commands and debug message
        assert!(commands.contains("NFS_Port = 3049"));
        assert!(commands
            .contains("[DEBUG] Updated NFS_Port to 3049 in NFS_Core_Param block in hitl-nfs.conf"));
    }

    #[test]
    fn test_generate_export_setup_commands_with_extensions_and_port() {
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec!["ext1".to_string(), "ext2".to_string()],
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: false,
            port: Some(4049),
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        };

        let commands = cmd.generate_export_setup_commands();

        // Should include both port update and extension configurations
        assert!(commands.contains("NFS_Port = 4049"));
        assert!(commands.contains("Export_Id = 1"));
        assert!(commands.contains("Export_Id = 2"));
        assert!(commands.contains("${AVOCADO_EXT_SYSROOTS}/ext1"));
        assert!(commands.contains("${AVOCADO_EXT_SYSROOTS}/ext2"));
    }

    #[test]
    fn test_stamp_requirements_for_extensions() {
        // Verify we require both install and build stamps for each extension
        let extensions = vec!["ext1".to_string(), "ext2".to_string()];

        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext_name in &extensions {
            requirements.push(StampRequirement::ext_install(ext_name));
            requirements.push(StampRequirement::ext_build(ext_name));
        }

        // Should have: SDK install + (install + build) for each extension
        // 1 + (2 * 2) = 5
        assert_eq!(requirements.len(), 5);
        assert!(requirements.contains(&StampRequirement::sdk_install()));
        assert!(requirements.contains(&StampRequirement::ext_install("ext1")));
        assert!(requirements.contains(&StampRequirement::ext_build("ext1")));
        assert!(requirements.contains(&StampRequirement::ext_install("ext2")));
        assert!(requirements.contains(&StampRequirement::ext_build("ext2")));
    }

    // ========================================================================
    // HITL Server Stamp Validation Tests
    // ========================================================================

    #[test]
    fn test_hitl_server_no_stamps_flag() {
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec!["ext1".to_string()],
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: false,
            port: None,
            no_stamps: true,
            sdk_arch: None,
            composed_config: None,
        };

        // With no_stamps, validation should be skipped
        assert!(cmd.no_stamps);
    }

    #[test]
    fn test_hitl_server_no_extensions_no_stamp_requirements() {
        // When no extensions specified, only SDK install is needed (implicit)
        let cmd = HitlServerCommand {
            config_path: "test.yaml".to_string(),
            extensions: vec![], // No extensions
            container_args: None,
            dnf_args: None,
            target: None,
            verbose: false,
            port: None,
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        };

        // With no extensions, the stamp validation loop is skipped entirely
        assert!(cmd.extensions.is_empty());
    }

    #[test]
    fn test_hitl_server_stamp_validation_all_present() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
        };

        let extensions = vec!["gpu-driver".to_string()];

        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext in &extensions {
            requirements.push(StampRequirement::ext_install(ext));
            requirements.push(StampRequirement::ext_build(ext));
        }

        // All stamps present
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_install = Stamp::ext_install(
            "gpu-driver",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );
        let ext_build = Stamp::ext_build(
            "gpu-driver",
            "qemux86-64",
            StampInputs::new("hash3".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let install_json = serde_json::to_string(&ext_install).unwrap();
        let build_json = serde_json::to_string(&ext_build).unwrap();

        let output = format!(
            "sdk/{}/install.stamp:::{}\next/gpu-driver/install.stamp:::{}\next/gpu-driver/build.stamp:::{}",
            get_local_arch(),
            sdk_json,
            install_json,
            build_json
        );

        let result = validate_stamps_batch(&requirements, &output, None);
        assert!(result.is_satisfied());
    }

    #[test]
    fn test_hitl_server_stamp_validation_missing_build() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
        };

        let extensions = vec!["app".to_string()];

        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext in &extensions {
            requirements.push(StampRequirement::ext_install(ext));
            requirements.push(StampRequirement::ext_build(ext));
        }

        // SDK and install present, build missing
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_install = Stamp::ext_install(
            "app",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let install_json = serde_json::to_string(&ext_install).unwrap();

        let output = format!(
            "sdk/{}/install.stamp:::{}\next/app/install.stamp:::{}\next/app/build.stamp:::null",
            get_local_arch(),
            sdk_json,
            install_json
        );

        let result = validate_stamps_batch(&requirements, &output, None);
        assert!(!result.is_satisfied());
        assert_eq!(result.missing.len(), 1);
        assert_eq!(result.missing[0].relative_path(), "ext/app/build.stamp");
    }

    #[test]
    fn test_hitl_server_clean_lifecycle() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
        };

        let extensions = vec!["network-driver".to_string()];

        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext in &extensions {
            requirements.push(StampRequirement::ext_install(ext));
            requirements.push(StampRequirement::ext_build(ext));
        }

        // All stamps present before clean
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_install = Stamp::ext_install(
            "network-driver",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );
        let ext_build = Stamp::ext_build(
            "network-driver",
            "qemux86-64",
            StampInputs::new("hash3".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let install_json = serde_json::to_string(&ext_install).unwrap();
        let build_json = serde_json::to_string(&ext_build).unwrap();

        let output_before = format!(
            "sdk/{}/install.stamp:::{}\next/network-driver/install.stamp:::{}\next/network-driver/build.stamp:::{}",
            get_local_arch(),
            sdk_json,
            install_json,
            build_json
        );

        let result_before = validate_stamps_batch(&requirements, &output_before, None);
        assert!(result_before.is_satisfied(), "Should pass before clean");

        // After ext clean network-driver: SDK still there, ext stamps gone
        let output_after = format!(
            "sdk/{}/install.stamp:::{}\next/network-driver/install.stamp:::null\next/network-driver/build.stamp:::null",
            get_local_arch(),
            sdk_json
        );

        let result_after = validate_stamps_batch(&requirements, &output_after, None);
        assert!(!result_after.is_satisfied(), "Should fail after ext clean");
        assert_eq!(
            result_after.missing.len(),
            2,
            "Both ext stamps should be missing"
        );
    }

    #[test]
    fn test_hitl_server_multiple_extensions_partial_clean() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
        };

        let extensions = vec!["ext-a".to_string(), "ext-b".to_string()];

        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext in &extensions {
            requirements.push(StampRequirement::ext_install(ext));
            requirements.push(StampRequirement::ext_build(ext));
        }

        // All stamps present
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_a_install = Stamp::ext_install(
            "ext-a",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );
        let ext_a_build = Stamp::ext_build(
            "ext-a",
            "qemux86-64",
            StampInputs::new("hash3".to_string()),
            StampOutputs::default(),
        );
        let ext_b_install = Stamp::ext_install(
            "ext-b",
            "qemux86-64",
            StampInputs::new("hash4".to_string()),
            StampOutputs::default(),
        );
        let ext_b_build = Stamp::ext_build(
            "ext-b",
            "qemux86-64",
            StampInputs::new("hash5".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        // ext-a stamps are intentionally not used - simulating they were cleaned (return null)
        let _ext_a_install_json = serde_json::to_string(&ext_a_install).unwrap();
        let _ext_a_build_json = serde_json::to_string(&ext_a_build).unwrap();
        let ext_b_install_json = serde_json::to_string(&ext_b_install).unwrap();
        let ext_b_build_json = serde_json::to_string(&ext_b_build).unwrap();

        // After cleaning only ext-a: ext-a stamps return null, ext-b stamps still present
        let output_partial = format!(
            "sdk/{}/install.stamp:::{}\next/ext-a/install.stamp:::null\next/ext-a/build.stamp:::null\next/ext-b/install.stamp:::{}\next/ext-b/build.stamp:::{}",
            get_local_arch(),
            sdk_json,
            ext_b_install_json,
            ext_b_build_json
        );

        let result = validate_stamps_batch(&requirements, &output_partial, None);
        assert!(
            !result.is_satisfied(),
            "Should fail when one extension is cleaned"
        );
        assert_eq!(
            result.missing.len(),
            2,
            "ext-a install and build should be missing"
        );
        assert_eq!(
            result.satisfied.len(),
            3,
            "SDK and ext-b stamps should be satisfied"
        );
    }
}
