//! SDK DNF command implementation.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use crate::utils::{
    config::Config,
    container::SdkContainer,
    output::{print_error, print_success},
    target::resolve_target,
};

/// Implementation of the 'sdk dnf' command.
pub struct SdkDnfCommand {
    /// Path to configuration file
    pub config_path: String,
    /// DNF command and arguments to execute
    pub dnf_args: Vec<String>,
    /// Global target architecture
    pub target: Option<String>,
}

impl SdkDnfCommand {
    /// Create a new SdkDnfCommand instance
    pub fn new(config_path: String, dnf_args: Vec<String>, target: Option<String>) -> Self {
        Self {
            config_path,
            dnf_args,
            target,
        }
    }

    /// Execute the sdk dnf command
    pub async fn execute(&self) -> Result<()> {
        if self.dnf_args.is_empty() {
            return Err(anyhow::anyhow!(
                "No DNF command specified. Use '--' to separate DNF arguments."
            ));
        }

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

        let container_helper = SdkContainer::new();

        // Build DNF command
        let command = format!(
            "RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF {}",
            self.dnf_args.join(" ")
        );

        // Run the DNF command using the container helper
        let success = self
            .run_dnf_command(&container_helper, container_image, &target, &command)
            .await?;

        // Log the result
        if success {
            print_success("DNF command completed successfully.");
        } else {
            print_error("DNF command failed.");
            return Err(anyhow::anyhow!("DNF command failed"));
        }

        Ok(())
    }

    /// Run DNF command using container with entrypoint
    async fn run_dnf_command(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        command: &str,
    ) -> Result<bool> {
        // Build container command with entrypoint
        let mut container_cmd = vec![
            container_helper.container_tool.clone(),
            "run".to_string(),
            "--rm".to_string(),
        ];

        // Add volume mounts
        container_cmd.push("-v".to_string());
        container_cmd.push(format!(
            "{}:/opt/_avocado/src:ro",
            container_helper.cwd.display()
        ));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!(
            "{}/_avocado:/opt/_avocado:rw",
            container_helper.cwd.display()
        ));

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={}", target));

        // Add the container image
        container_cmd.push(container_image.to_string());

        // Use entrypoint to set up environment, then run DNF command
        let full_command = format!(
            "{}\n{}",
            container_helper.create_entrypoint_script(),
            command
        );

        container_cmd.push("bash".to_string());
        container_cmd.push("-c".to_string());
        container_cmd.push(full_command);

        // Execute the command
        let status = AsyncCommand::new(&container_cmd[0])
            .args(&container_cmd[1..])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| "Failed to execute DNF container command")?;

        Ok(status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = SdkDnfCommand::new(
            "config.toml".to_string(),
            vec!["install".to_string(), "gcc".to_string()],
            Some("test-target".to_string()),
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert_eq!(cmd.dnf_args, vec!["install", "gcc"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[tokio::test]
    async fn test_empty_dnf_args() {
        let cmd = SdkDnfCommand::new("config.toml".to_string(), vec![], None);

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No DNF command specified"));
    }
}
