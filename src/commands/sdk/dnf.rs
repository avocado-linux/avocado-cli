//! SDK DNF command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_success, OutputLevel},
    target::resolve_target_required,
};

/// Implementation of the 'sdk dnf' command.
pub struct SdkDnfCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// DNF command and arguments to execute
    pub command: Vec<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
}

impl SdkDnfCommand {
    /// Create a new SdkDnfCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        command: Vec<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            command,
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

    /// Execute the sdk dnf command
    pub async fn execute(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(anyhow::anyhow!(
                "No DNF command specified. Use --command (-c) to provide DNF arguments."
            ));
        }

        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Get repo_url and repo_release from config, if they exist
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        let container_helper = SdkContainer::new();

        // Build DNF command
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };
        let command = format!(
            "RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF --disablerepo=${{AVOCADO_TARGET}}-target-ext {} {}",
            dnf_args_str,
            self.command.join(" ")
        );

        // Run the DNF command using the container helper
        let success = self
            .run_dnf_command(
                &container_helper,
                container_image,
                &target,
                &command,
                repo_url.as_ref(),
                repo_release.as_ref(),
                merged_container_args.as_ref(),
            )
            .await?;

        // Log the result
        if success {
            print_success("DNF command completed successfully.", OutputLevel::Normal);
        } else {
            print_error("DNF command failed.", OutputLevel::Normal);
            return Err(anyhow::anyhow!("DNF command failed"));
        }

        Ok(())
    }

    /// Run DNF command using container with entrypoint
    #[allow(clippy::too_many_arguments)]
    async fn run_dnf_command(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        command: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_args: Option<&Vec<String>>,
    ) -> Result<bool> {
        // Use the container helper's method with repo URL and release support
        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: command.to_string(),
            verbose: self.verbose,
            source_environment: true, // need environment for DNF
            interactive: true,        // allow user input for DNF prompts
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        container_helper.run_in_container(config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = SdkDnfCommand::new(
            "config.toml".to_string(),
            false,
            vec!["install".to_string(), "gcc".to_string()],
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.command, vec!["install", "gcc"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[tokio::test]
    async fn test_empty_command() {
        let cmd = SdkDnfCommand::new("config.toml".to_string(), false, vec![], None, None, None);

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No DNF command specified"));
    }
}
