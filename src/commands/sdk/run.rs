//! SDK run command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_success, OutputLevel},
    target::resolve_target_required,
    volume::VolumeManager,
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
    /// Source the avocado SDK environment before running command
    pub env: bool,
    /// Command and arguments to run in container
    pub command: Option<Vec<String>>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

impl SdkRunCommand {
    /// Create a new SdkRunCommand instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_path: String,
        name: Option<String>,
        detach: bool,
        rm: bool,
        interactive: bool,
        verbose: bool,
        env: bool,
        command: Option<Vec<String>>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            name,
            detach,
            rm,
            interactive,
            verbose,
            env,
            command,
            target,
            container_args,
            dnf_args,
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
        if !self.interactive && self.command.is_none() {
            return Err(anyhow::anyhow!(
                "You must either provide a --command (-c) or use --interactive (-i)."
            ));
        }

        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        if let Some(ref name) = self.name {
            println!("Container name: {name}");
        }

        // Build the command to execute
        let command = if let Some(ref cmd) = self.command {
            let user_command = cmd.join(" ");
            if self.env {
                format!(". avocado-env && {user_command}")
            } else {
                user_command
            }
        } else if self.env {
            ". avocado-env && bash".to_string()
        } else {
            "bash".to_string()
        };

        // Use the container helper to run the command
        let container_helper = SdkContainer::new().verbose(self.verbose);

        let success = if self.detach {
            self.run_detached_container(&container_helper, container_image, &target, &command)
                .await?
        } else if self.interactive {
            self.run_interactive_container(
                &container_helper,
                container_image,
                &target,
                &command,
                repo_url,
                repo_release,
            )
            .await?
        } else {
            let config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: command.clone(),
                verbose: self.verbose,
                source_environment: false, // don't source environment
                interactive: false,        // not interactive
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                ..Default::default()
            };
            container_helper.run_in_container(config).await?
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
        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(container_helper.container_tool.clone(), false);
        let volume_state = volume_manager
            .get_or_create_volume(&container_helper.cwd)
            .await?;
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

        // Volume mounts: docker volume for persistent state, bind mount for source
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/src:rw", container_helper.cwd.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={target}"));

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
            println!("Container started in detached mode with ID: {container_id}");
            Ok(true)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            print_error(
                &format!("Container execution failed: {stderr}"),
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
        command: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
    ) -> Result<bool> {
        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: command.to_string(),
            verbose: self.verbose,
            source_environment: self.env,
            interactive: true,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
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
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            Some("test-container".to_string()),
            false,
            true,
            false,
            true,
            false, // env
            Some(vec!["echo".to_string(), "test".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert_eq!(cmd.name, Some("test-container".to_string()));
        assert!(!cmd.detach);
        assert!(cmd.rm);
        assert!(!cmd.interactive);
        assert!(cmd.verbose);
        assert_eq!(
            cmd.command,
            Some(vec!["echo".to_string(), "test".to_string()])
        );
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_interactive_with_env_and_command() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false, // detach
            false, // rm
            true,  // interactive
            false, // verbose
            true,  // env
            Some(vec!["ls".to_string(), "-la".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
        );

        // Verify that the command struct stores the values correctly
        assert!(cmd.interactive);
        assert!(cmd.env);
        assert_eq!(cmd.command, Some(vec!["ls".to_string(), "-la".to_string()]));
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
            false, // env
            None,
            None,
            None,
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
            false, // env
            None,  // no command
            None,
            None,
            None,
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("You must either provide a --command"));
    }

    #[test]
    fn test_env_flag_command_building() {
        // Test with env flag and command
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            true, // env = true
            Some(vec![
                "vm".to_string(),
                "--mem".to_string(),
                "512".to_string(),
            ]),
            None,
            None,
            None,
        );

        assert!(cmd.env);
        assert_eq!(
            cmd.command,
            Some(vec![
                "vm".to_string(),
                "--mem".to_string(),
                "512".to_string()
            ])
        );

        // Test without env flag
        let cmd_no_env = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false, // env = false
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
        );

        assert!(!cmd_no_env.env);
    }
}
