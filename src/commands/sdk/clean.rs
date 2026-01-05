//! SDK clean command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    target::resolve_target_required,
};

/// Implementation of the 'sdk clean' command.
pub struct SdkCleanCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
}

impl SdkCleanCommand {
    /// Create a new SdkCleanCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            sdk_arch: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Execute the sdk clean command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), &config)?;

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
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: remove_command.to_string(),
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url,
            repo_release,
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(run_config).await?;

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
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = SdkCleanCommand::new("config.toml".to_string(), false, None, None, None);

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, None);
    }
}
