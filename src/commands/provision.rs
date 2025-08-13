//! Provision command implementation that acts as a shortcut to runtime provision.

use anyhow::Result;
use std::collections::HashMap;

use crate::commands::runtime::RuntimeProvisionCommand;

/// Configuration for provision command
pub struct ProvisionConfig {
    /// Runtime name to provision
    pub runtime: String,
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Global target architecture
    pub target: Option<String>,
    /// Environment variables to pass to the provision process
    pub env_vars: Option<HashMap<String, String>>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

/// Implementation of the 'provision' command that calls through to runtime provision.
pub struct ProvisionCommand {
    config: ProvisionConfig,
}

impl ProvisionCommand {
    /// Create a new ProvisionCommand instance
    pub fn new(config: ProvisionConfig) -> Self {
        Self { config }
    }

    /// Execute the provision command by calling runtime provision
    pub async fn execute(&self) -> Result<()> {
        let processed_container_args = crate::utils::config::Config::process_container_args(
            self.config.container_args.as_ref(),
        );

        let runtime_provision_cmd = RuntimeProvisionCommand::new(
            crate::commands::runtime::provision::RuntimeProvisionConfig {
                runtime_name: self.config.runtime.clone(),
                config_path: self.config.config_path.clone(),
                verbose: self.config.verbose,
                force: self.config.force,
                target: self.config.target.clone(),
                env_vars: self.config.env_vars.clone(),
                container_args: processed_container_args,
                dnf_args: self.config.dnf_args.clone(),
            },
        );

        runtime_provision_cmd.execute().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let mut env_vars = HashMap::new();
        env_vars.insert("TEST_VAR".to_string(), "test_value".to_string());

        let config = ProvisionConfig {
            runtime: "my-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: true,
            force: false,
            target: Some("x86_64".to_string()),
            env_vars: Some(env_vars.clone()),
            container_args: Some(vec!["--privileged".to_string()]),
            dnf_args: Some(vec!["--nogpgcheck".to_string()]),
        };
        let cmd = ProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime, "my-runtime");
        assert_eq!(cmd.config.config_path, "avocado.toml");
        assert!(cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, Some(env_vars));
        assert_eq!(
            cmd.config.container_args,
            Some(vec!["--privileged".to_string()])
        );
        assert_eq!(cmd.config.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_minimal() {
        let config = ProvisionConfig {
            runtime: "test-runtime".to_string(),
            config_path: "config.toml".to_string(),
            verbose: false,
            force: true,
            target: None,
            env_vars: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = ProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime, "test-runtime");
        assert_eq!(cmd.config.config_path, "config.toml");
        assert!(!cmd.config.verbose);
        assert!(cmd.config.force);
        assert_eq!(cmd.config.target, None);
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.container_args, None);
        assert_eq!(cmd.config.dnf_args, None);
    }

    #[test]
    fn test_new_with_provision_profile() {
        let mut expected_env = HashMap::new();
        expected_env.insert(
            "AVOCADO_PROVISION_PROFILE".to_string(),
            "production".to_string(),
        );

        let config = ProvisionConfig {
            runtime: "my-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            force: false,
            target: None,
            env_vars: Some(expected_env.clone()),
            container_args: None,
            dnf_args: None,
        };
        let cmd = ProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime, "my-runtime");
        assert_eq!(cmd.config.env_vars, Some(expected_env));
    }
}
