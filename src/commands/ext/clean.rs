use anyhow::Result;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtCleanCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl ExtCleanCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let _parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        let target = resolve_target_required(self.target.as_deref(), &config)?;
        let _extension_location = self.find_extension_in_dependency_tree(&config, &target)?;
        let container_image = self.get_container_image(&config)?;

        self.clean_extension(&container_image, &target).await
    }

    fn find_extension_in_dependency_tree(
        &self,
        config: &Config,
        target: &str,
    ) -> Result<ExtensionLocation> {
        match config.find_extension_in_dependency_tree(
            &self.config_path,
            &self.extension,
            target,
        )? {
            Some(location) => {
                if self.verbose {
                    match &location {
                        ExtensionLocation::Local { name, config_path } => {
                            print_info(
                                &format!(
                                    "Found local extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        ExtensionLocation::External { name, config_path } => {
                            print_info(
                                &format!(
                                    "Found external extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                    }
                }
                Ok(location)
            }
            None => {
                print_error(
                    &format!("Extension '{}' not found in configuration.", self.extension),
                    OutputLevel::Normal,
                );
                Err(anyhow::anyhow!("Extension not found"))
            }
        }
    }

    fn get_container_image(&self, config: &Config) -> Result<String> {
        config
            .get_sdk_image()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    async fn clean_extension(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning extension '{}'...", self.extension),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();

        // Clean sysroot, output files, and stamps
        let clean_command = format!(
            r#"
# Clean extension sysroot
rm -rf "$AVOCADO_EXT_SYSROOTS/{ext}"

# Clean extension output files (built .raw images)
rm -f "$AVOCADO_PREFIX/output/extensions/{ext}"-*.raw

# Clean extension stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/ext/{ext}"

echo "Cleaned extension '{ext}': sysroot, outputs, and stamps"
"#,
            ext = self.extension
        );

        if self.verbose {
            print_info(
                &format!("Running command: {clean_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            container_args: crate::utils::config::Config::process_container_args(
                self.container_args.as_ref(),
            ),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success(
                &format!("Successfully cleaned extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            print_error(
                &format!("Failed to clean extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!("Clean command failed"))
        }
    }

    /// Generate the clean command script for testing
    #[cfg(test)]
    fn generate_clean_script(&self) -> String {
        format!(
            r#"
# Clean extension sysroot
rm -rf "$AVOCADO_EXT_SYSROOTS/{ext}"

# Clean extension output files (built .raw images)
rm -f "$AVOCADO_PREFIX/output/extensions/{ext}"-*.raw

# Clean extension stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/ext/{ext}"

echo "Cleaned extension '{ext}': sysroot, outputs, and stamps"
"#,
            ext = self.extension
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.extension, "test-ext");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = ExtCleanCommand::new(
            "my-extension".to_string(),
            "config.yaml".to_string(),
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.extension, "my-extension");
        assert!(cmd.verbose);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_clean_script_cleans_sysroot() {
        let cmd = ExtCleanCommand::new(
            "gpu-driver".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean extension sysroot
        assert!(script.contains(r#"rm -rf "$AVOCADO_EXT_SYSROOTS/gpu-driver""#));
    }

    #[test]
    fn test_clean_script_cleans_output_files() {
        let cmd = ExtCleanCommand::new(
            "network-driver".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean built extension images
        assert!(
            script.contains(r#"rm -f "$AVOCADO_PREFIX/output/extensions/network-driver"-*.raw"#)
        );
    }

    #[test]
    fn test_clean_script_cleans_stamps() {
        let cmd = ExtCleanCommand::new(
            "app-bundle".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean extension stamps (install and build)
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/.stamps/ext/app-bundle""#));
    }

    #[test]
    fn test_clean_script_includes_all_cleanup_targets() {
        let cmd = ExtCleanCommand::new(
            "my-ext".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Verify all three cleanup targets are present
        assert!(
            script.contains("AVOCADO_EXT_SYSROOTS"),
            "Should clean sysroot"
        );
        assert!(
            script.contains("output/extensions"),
            "Should clean output files"
        );
        assert!(script.contains(".stamps/ext"), "Should clean stamps");
    }
}
