use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_debug, print_info, OutputLevel};
use crate::utils::stamps::{
    generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement,
};
use crate::utils::target::validate_and_log_target;
use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct HitlServerCommand {
    /// Path to the avocado.toml configuration file
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
}

impl HitlServerCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Use shared target resolution logic with early validation and logging
        let target = validate_and_log_target(self.target.as_deref(), &config)?;

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

        // Build container arguments with HITL-specific defaults
        let mut container_args = vec![
            "--net=host".to_string(),
            "--cap-add".to_string(),
            "DAC_READ_SEARCH".to_string(),
            "--init".to_string(),
        ];

        // Add any additional container arguments with environment variable expansion
        if let Some(ref additional_args) = self.container_args {
            let processed_args = Config::process_container_args(Some(additional_args));
            if let Some(processed) = processed_args {
                container_args.extend(processed);
            }
        }

        // Generate NFS export setup commands
        let export_setup = self.generate_export_setup_commands(&target);

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
            ..Default::default()
        };

        container_helper.run_in_container(config).await?;

        Ok(())
    }

    /// Generate shell commands to create NFS export configuration files
    fn generate_export_setup_commands(&self, target: &str) -> String {
        let mut commands = vec![
            "mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d".to_string(),
            "mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado".to_string(),
        ];

        // Add/update the hitl-nfs.conf file with the exports.d directory directive
        let exports_dir_line = format!("%dir /opt/_avocado/{target}/sdk/etc/avocado/exports.d");
        let config_file = "${AVOCADO_SDK_PREFIX}/etc/avocado/hitl-nfs.conf".to_string();

        // Check if the line exists, if not add it
        let update_config_cmd = format!(
            "touch {config_file} && \
             if ! grep -q '^%dir /opt/_avocado/{target}/sdk/etc/avocado/exports.d$' {config_file}; then \
               echo '{exports_dir_line}' >> {config_file}; \
             fi"
        );
        commands.push(update_config_cmd);

        // Update NFS_Port if a port is specified (it's nested inside NFS_Core_Param block)
        if let Some(port) = self.port {
            let port_update_cmd = format!(
                "sed -i '/NFS_Core_Param {{/,/}}/s/NFS_Port = [0-9]\\+;/NFS_Port = {port};/' {config_file}"
            );
            commands.push(port_update_cmd);

            if self.verbose {
                commands.push(format!(
                    "echo \"[DEBUG] Updated NFS_Port to {port} in NFS_Core_Param block in hitl-nfs.conf\""
                ));
            }
        }

        if self.extensions.is_empty() {
            return format!("{} &&", commands.join(" && "));
        }

        for (index, extension) in self.extensions.iter().enumerate() {
            let export_id = index + 1;

            // Expand the AVOCADO_PREFIX variable to its actual path
            let extensions_path = format!("/opt/_avocado/{target}/extensions/{extension}");

            let export_content = format!(
                "EXPORT {{\n\
                \x20\x20Export_Id = {export_id};\n\
                \x20\x20Path = {extensions_path};\n\
                \x20\x20Pseudo = /{extension};\n\
                \x20\x20FSAL {{\n\
                \x20\x20\x20\x20name = VFS;\n\
                \x20\x20}}\n\
                }}"
            );

            let export_file =
                format!("${{AVOCADO_SDK_PREFIX}}/etc/avocado/exports.d/{extension}.conf");

            // Create a command that writes the export content to the file using echo -e to avoid here-doc issues
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
        };

        let commands = cmd.generate_export_setup_commands("x86_64");

        // Should create directories and exports.d directive but no port update
        assert!(commands.contains("mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d"));
        assert!(commands.contains("%dir /opt/_avocado/x86_64/sdk/etc/avocado/exports.d"));
        assert!(!commands.contains("NFS_Port ="));
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
        };

        let commands = cmd.generate_export_setup_commands("x86_64");

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
        };

        let commands = cmd.generate_export_setup_commands("x86_64");

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
        };

        let commands = cmd.generate_export_setup_commands("aarch64");

        // Should include both port update and extension configurations
        assert!(commands.contains("NFS_Port = 4049"));
        assert!(commands.contains("Export_Id = 1"));
        assert!(commands.contains("Export_Id = 2"));
        assert!(commands.contains("/opt/_avocado/aarch64/extensions/ext1"));
        assert!(commands.contains("/opt/_avocado/aarch64/extensions/ext2"));
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
}
