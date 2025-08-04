use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtCleanCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
}

impl ExtCleanCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_extension_exists(&parsed)?;
        let container_image = self.get_container_image(&parsed)?;
        let target = self.resolve_target_architecture(&parsed)?;

        self.clean_extension(&container_image, &target).await
    }

    fn validate_extension_exists(&self, parsed: &toml::Value) -> Result<()> {
        let ext_section = parsed.get("ext").ok_or_else(|| {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No ext section found")
        })?;

        let ext_table = ext_section
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("Invalid ext section format"))?;

        if !ext_table.contains_key(&self.extension) {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Extension not found"));
        }

        Ok(())
    }

    fn get_container_image(&self, parsed: &toml::Value) -> Result<String> {
        parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, parsed: &toml::Value) -> Result<String> {
        let config_target = self.extract_config_target(parsed);
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());

        resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })
    }

    fn extract_config_target(&self, parsed: &toml::Value) -> Option<String> {
        parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_table())
            .and_then(|runtime_table| {
                if runtime_table.len() == 1 {
                    runtime_table.values().next()
                } else {
                    None
                }
            })
            .and_then(|runtime_config| runtime_config.get("target"))
            .and_then(|target| target.as_str())
            .map(|s| s.to_string())
    }

    async fn clean_extension(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning extension '{}'...", self.extension),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();
        let clean_command = format!("rm -rf ${{AVOCADO_EXT_SYSROOTS}}/{}", self.extension);

        if self.verbose {
            print_info(
                &format!("Running command: {}", clean_command),
                OutputLevel::Normal,
            );
        }

        let success = container_helper
            .run_in_container(
                &container_image,
                target,
                &clean_command,
                self.verbose,
                false, // don't source environment
                false, // not interactive
            )
            .await?;

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
