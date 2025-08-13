//! Provision command implementation that acts as a shortcut to runtime provision.

use anyhow::Result;

use crate::commands::runtime::RuntimeProvisionCommand;

/// Implementation of the 'provision' command that calls through to runtime provision.
pub struct ProvisionCommand {
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
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

impl ProvisionCommand {
    /// Create a new ProvisionCommand instance
    pub fn new(
        runtime: String,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime,
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
        }
    }

    /// Execute the provision command by calling runtime provision
    pub async fn execute(&self) -> Result<()> {
        let runtime_provision_cmd = RuntimeProvisionCommand::new(
            self.runtime.clone(),
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            crate::utils::config::Config::process_container_args(self.container_args.as_ref()),
            self.dnf_args.clone(),
        );

        runtime_provision_cmd.execute().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = ProvisionCommand::new(
            "my-runtime".to_string(),
            "avocado.toml".to_string(),
            true,
            false,
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.runtime, "my-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = ProvisionCommand::new(
            "test-runtime".to_string(),
            "config.toml".to_string(),
            false,
            true,
            None,
            None,
            None,
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert!(cmd.force);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
