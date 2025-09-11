//! SDK run command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_success, OutputLevel},
    target::validate_and_log_target,
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
    /// Mount extension sysroot and change working directory to it
    pub extension: Option<String>,
    /// Mount runtime sysroot and change working directory to it
    pub runtime: Option<String>,
    /// Command and arguments to run in container
    pub command: Option<Vec<String>>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Skip SDK bootstrap initialization
    pub no_bootstrap: bool,
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
        extension: Option<String>,
        runtime: Option<String>,
        command: Option<Vec<String>>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
        no_bootstrap: bool,
    ) -> Self {
        Self {
            config_path,
            name,
            detach,
            rm,
            interactive,
            verbose,
            env,
            extension,
            runtime,
            command,
            target,
            container_args,
            dnf_args,
            no_bootstrap,
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

        // Validate that extension and runtime are not both specified
        if self.extension.is_some() && self.runtime.is_some() {
            return Err(anyhow::anyhow!(
                "Cannot specify both --extension (-e) and --runtime (-r) simultaneously."
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

        // Early target validation and logging - fail fast if target is unsupported
        let target = validate_and_log_target(self.target.as_deref(), &config)?;

        // Get merged SDK configuration for the target
        let merged_sdk_config = config.get_merged_sdk_config(&target, &self.config_path)?;

        // Get repo_url and repo_release from merged config
        let repo_url = merged_sdk_config.repo_url.as_ref();
        let repo_release = merged_sdk_config.repo_release.as_ref();

        // Merge container args from merged config with CLI args
        let config_container_args = merged_sdk_config.container_args.as_ref();
        let merged_container_args = match (config_container_args, self.container_args.as_ref()) {
            (Some(config_args), Some(cli_args)) => {
                let mut processed_args =
                    Config::process_container_args(Some(config_args)).unwrap_or_default();
                processed_args.extend_from_slice(cli_args);
                Some(processed_args)
            }
            (Some(config_args), None) => Config::process_container_args(Some(config_args)),
            (None, Some(cli_args)) => Some(cli_args.clone()),
            (None, None) => None,
        };

        // Get the SDK image from merged configuration
        let container_image = merged_sdk_config.image.ok_or_else(|| {
            anyhow::anyhow!(
                "No container image specified in config under 'sdk.image' or 'sdk.{}.image'",
                target
            )
        })?;

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
        let container_helper =
            SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

        // Create shared RunConfig for all execution modes
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: command.clone(),
            verbose: self.verbose,
            source_environment: self.env,
            interactive: self.interactive,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            extension_sysroot: self.extension.clone(),
            runtime_sysroot: self.runtime.clone(),
            no_bootstrap: self.no_bootstrap,
            ..Default::default()
        };

        let success = if self.detach {
            self.run_detached_container(&container_helper, &run_config)
                .await?
        } else {
            container_helper.run_in_container(run_config).await?
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
        config: &RunConfig,
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
        let src_path = container_helper
            .src_dir
            .as_ref()
            .unwrap_or(&container_helper.cwd);
        container_cmd.push(format!("{}:/opt/src:rw", src_path.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_TARGET={}", config.target));
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={}", config.target));

        // Add merged container args
        if let Some(args) = &config.container_args {
            container_cmd.extend_from_slice(args);
        }

        // Add the container image
        container_cmd.push(config.container_image.clone());

        // Add the command
        container_cmd.push("bash".to_string());
        container_cmd.push("-c".to_string());
        container_cmd.push(config.command.clone());

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
            None,  // extension
            None,  // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
            false, // no_bootstrap
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
            None,  // extension
            None,  // runtime
            Some(vec!["ls".to_string(), "-la".to_string()]),
            Some("test-target".to_string()),
            None,
            None,
            false, // no_bootstrap
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
            None,  // extension
            None,  // runtime
            None,
            None,
            None,
            None,
            false, // no_bootstrap
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
            None,  // extension
            None,  // runtime
            None,  // no command
            None,
            None,
            None,
            false, // no_bootstrap
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
            None, // extension
            None, // runtime
            Some(vec![
                "vm".to_string(),
                "--mem".to_string(),
                "512".to_string(),
            ]),
            None,
            None,
            None,
            false, // no_bootstrap
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
            None,  // extension
            None,  // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert!(!cmd_no_env.env);
    }

    #[tokio::test]
    async fn test_extension_and_runtime_conflict() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            Some("test-ext".to_string()),     // extension
            Some("test-runtime".to_string()), // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Cannot specify both --extension (-e) and --runtime (-r)"));
    }

    #[test]
    fn test_extension_sysroot_creation() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            Some("test-ext".to_string()), // extension
            None,                         // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert_eq!(cmd.extension, Some("test-ext".to_string()));
        assert_eq!(cmd.runtime, None);
    }

    #[test]
    fn test_runtime_sysroot_creation() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            None,                             // extension
            Some("test-runtime".to_string()), // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            false, // no_bootstrap
        );

        assert_eq!(cmd.extension, None);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
    }

    #[test]
    fn test_no_bootstrap_flag() {
        let cmd = SdkRunCommand::new(
            "config.toml".to_string(),
            None,
            false,
            false,
            false,
            false,
            false,
            None, // extension
            None, // runtime
            Some(vec!["echo".to_string(), "test".to_string()]),
            None,
            None,
            None,
            true, // no_bootstrap = true
        );

        assert!(cmd.no_bootstrap);
        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.env);
    }
}
