//! SDK clean command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::SdkContainer,
    output::{print_error, print_info, print_success, OutputLevel},
    target::resolve_target,
};

/// Implementation of the 'sdk clean' command.
pub struct SdkCleanCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Global target architecture
    pub target: Option<String>,
}

impl SdkCleanCommand {
    /// Create a new SdkCleanCommand instance
    pub fn new(config_path: String, verbose: bool, target: Option<String>) -> Self {
        Self {
            config_path,
            verbose,
            target,
        }
    }

    /// Execute the sdk clean command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Resolve target with proper precedence
        let config_target = config.get_target();
        let target = resolve_target(self.target.as_deref(), config_target.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'."
                )
            })?;

        // Create container helper
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Remove the directory using container helper
        if self.verbose {
            print_info(
                "Removing SDK directory: $AVOCADO_SDK_PREFIX",
                OutputLevel::Normal,
            );
        }

        let remove_command = "rm -rf $AVOCADO_SDK_PREFIX";
        let success = container_helper
            .run_in_container(
                container_image,
                &target,
                remove_command,
                self.verbose,
                false, // don't source environment
                false,
            )
            .await?;

        if success {
            print_success("Successfully removed SDK directory.", OutputLevel::Normal);
        } else {
            print_error("Failed to remove SDK directory.", OutputLevel::Normal);
            return Err(anyhow::anyhow!("Failed to remove SDK directory"));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = SdkCleanCommand::new(
            "config.toml".to_string(),
            true,
            Some("test-target".to_string()),
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = SdkCleanCommand::new("config.toml".to_string(), false, None);

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, None);
    }
}
