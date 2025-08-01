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
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if ext section exists
        let ext_section = match parsed.get("ext") {
            Some(ext) => ext,
            None => {
                print_error(
                    &format!("Extension '{}' not found in configuration.", self.extension),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        };

        // Check if the specific extension exists
        if !ext_section
            .as_table()
            .unwrap()
            .contains_key(&self.extension)
        {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Get the SDK image from configuration
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })?;

        // Resolve target architecture
        let config_target = parsed
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
            .map(|s| s.to_string());
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());
        let target = resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })?;

        print_info(
            &format!("Cleaning extension '{}'...", self.extension),
            OutputLevel::Normal,
        );

        // Use the container helper to run the clean command
        let container_helper = SdkContainer::new();

        // Command to remove the extension sysroot directory
        let clean_command = format!("rm -rf ${{AVOCADO_EXT_SYSROOTS}}/{}", self.extension);

        if self.verbose {
            print_info(
                &format!("Running command: {}", clean_command),
                OutputLevel::Normal,
            );
        }

        // Execute the clean command
        let success = container_helper
            .run_in_container(
                container_image,
                &target,
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
        } else {
            print_error(
                &format!("Failed to clean extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Clean command failed"));
        }

        Ok(())
    }
}
