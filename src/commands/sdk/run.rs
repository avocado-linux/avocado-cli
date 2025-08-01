//! SDK run command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::SdkContainer,
    output::{print_error, print_success, OutputLevel},
    target::resolve_target,
};

/// Implementation of the 'sdk run' command.
pub struct SdkRunCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Assign a name to the container
    pub name: Option<String>,
    /// Run container in background and print container ID
    pub detach: bool,
    /// Automatically remove the container when it exits
    pub rm: bool,
    /// Drop into interactive shell in container
    pub interactive: bool,
    /// Enable verbose output
    pub verbose: bool,
    /// Command and arguments to run in container
    pub command: Vec<String>,
    /// Global target architecture
    pub target: Option<String>,
}

impl SdkRunCommand {
    /// Create a new SdkRunCommand instance
    pub fn new(
        config_path: String,
        name: Option<String>,
        detach: bool,
        rm: bool,
        interactive: bool,
        verbose: bool,
        command: Vec<String>,
        target: Option<String>,
    ) -> Self {
        Self {
            config_path,
            name,
            detach,
            rm,
            interactive,
            verbose,
            command,
            target,
        }
    }

    /// Execute the sdk run command
    pub async fn execute(&self) -> Result<()> {
        // Validate arguments
        if self.interactive && self.detach {
            return Err(anyhow::anyhow!(
                "Cannot specify both --interactive (-i) and --detach (-d) simultaneously."
            ));
        }

        // Require either a command or --interactive flag
        if !self.interactive && self.command.is_empty() {
            return Err(anyhow::anyhow!(
                "You must either provide a command or use --interactive (-i)."
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

        if let Some(ref name) = self.name {
            println!("Container name: {}", name);
        }

        // Build the command to execute
        let command = if self.command.is_empty() {
            "bash".to_string()
        } else {
            self.command.join(" ")
        };

        // Use the container helper to run the command
        let container_helper = SdkContainer::new().verbose(self.verbose);

        let success = if self.detach {
            self.run_detached_container(&container_helper, container_image, &target, &command)
                .await?
        } else if self.interactive {
            self.run_interactive_container(&container_helper, container_image, &target)
                .await?
        } else {
            container_helper
                .run_in_container(
                    container_image,
                    &target,
                    &command,
                    self.verbose,
                    false,
                    false,
                )
                .await?
        };

        if success {
            print_success("SDK command completed successfully.", OutputLevel::Normal);
        }

        Ok(())
    }

    /// Run container in detached mode
    async fn run_detached_container(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        command: &str,
    ) -> Result<bool> {
        // Build container command for detached mode
        let mut container_cmd = vec![
            container_helper.container_tool.clone(),
            "run".to_string(),
            "-d".to_string(),
        ];

        if self.rm {
            container_cmd.push("--rm".to_string());
        }

        if let Some(ref name) = self.name {
            container_cmd.push("--name".to_string());
            container_cmd.push(name.clone());
        }

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

        // Add the command
        container_cmd.push("bash".to_string());
        container_cmd.push("-c".to_string());
        container_cmd.push(command.to_string());

        // Execute using tokio Command
        let output = tokio::process::Command::new(&container_cmd[0])
            .args(&container_cmd[1..])
            .output()
            .await
            .with_context(|| "Failed to execute detached container command")?;

        if output.status.success() {
            let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!(
                "Container started in detached mode with ID: {}",
                container_id
            );
            Ok(true)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            print_error(
                &format!("Container execution failed: {}", stderr),
                OutputLevel::Normal,
            );
            Ok(false)
        }
    }

    /// Run container in interactive mode
    async fn run_interactive_container(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
    ) -> Result<bool> {
        container_helper
            .run_in_container(container_image, target, "bash", self.verbose, false, true)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            Some("test-container".to_string()),
            false,
            true,
            false,
            true,
            vec!["echo".to_string(), "test".to_string()],
            Some("test-target".to_string()),
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert_eq!(cmd.name, Some("test-container".to_string()));
        assert!(!cmd.detach);
        assert!(cmd.rm);
        assert!(!cmd.interactive);
        assert!(cmd.verbose);
        assert_eq!(cmd.command, vec!["echo", "test"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[tokio::test]
    async fn test_invalid_arguments() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            true, // detach
            false,
            true, // interactive
            false,
            vec![],
            None,
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Cannot specify both"));
    }

    #[tokio::test]
    async fn test_no_command_or_interactive() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false, // not interactive
            false,
            vec![], // no command
            None,
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("You must either provide a command"));
    }
}
