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
        let clean_command = format!("rm -rf $AVOCADO_EXT_SYSROOTS/{}", self.extension);

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
}
