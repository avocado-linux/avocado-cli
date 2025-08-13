use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_debug, OutputLevel};
use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct HitlServerCommand {
    /// Path to the avocado.toml configuration file
    #[arg(short, long, default_value = "avocado.toml")]
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
}

impl HitlServerCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Determine target
        let target = if let Some(ref target) = self.target {
            target.clone()
        } else if let Some(ref runtime_map) = config.runtime {
            if let Some(first_runtime) = runtime_map.keys().next() {
                if let Some(runtime_config) = runtime_map.get(first_runtime) {
                    if let Some(ref target) = runtime_config.target {
                        target.clone()
                    } else {
                        return Err(anyhow::anyhow!(
                            "No target specified for runtime '{}'",
                            first_runtime
                        ));
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "No target configuration found for runtime '{}'",
                        first_runtime
                    ));
                }
            } else {
                return Err(anyhow::anyhow!("No runtime configurations found"));
            }
        } else {
            return Err(anyhow::anyhow!(
                "No target specified and no runtime configuration found"
            ));
        };

        if self.verbose {
            print_debug(&format!("Using target: {target}"), OutputLevel::Normal);
        }

        // Get SDK configuration
        let (container_image, repo_url, repo_release) = if let Some(sdk_config) = &config.sdk {
            let repo_url = sdk_config.repo_url.as_ref();
            let repo_release = sdk_config.repo_release.as_ref();
            let image = sdk_config
                .image
                .as_ref()
                .unwrap_or(&"avocadolinux/sdk:latest".to_string())
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

        // Build container arguments with HITL-specific defaults
        let mut container_args = vec![
            "--net=host".to_string(),
            "--cap-add".to_string(),
            "DAC_READ_SEARCH".to_string(),
            "--init".to_string(),
        ];

        // Add any additional container arguments
        if let Some(ref additional_args) = self.container_args {
            container_args.extend(additional_args.clone());
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
