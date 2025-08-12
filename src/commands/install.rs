//! Install command implementation that runs SDK, extension, and runtime installs.

use anyhow::{Context, Result};

use crate::commands::{
    ext::ExtInstallCommand, runtime::RuntimeInstallCommand, sdk::SdkInstallCommand,
};
use crate::utils::{
    config::Config,
    output::{print_info, print_success, OutputLevel},
};

/// Implementation of the 'install' command that runs all install subcommands.
pub struct InstallCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Runtime name to install dependencies for (if not provided, installs for all runtimes)
    pub runtime: Option<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

impl InstallCommand {
    /// Create a new InstallCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        runtime: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            runtime,
            target,
            container_args,
            dnf_args,
        }
    }

    /// Execute the install command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration to check what components exist
        let _config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        print_info(
            "Starting comprehensive install process...",
            OutputLevel::Normal,
        );

        // 1. Install SDK dependencies
        print_info("Step 1/3: Installing SDK dependencies", OutputLevel::Normal);
        let sdk_install_cmd = SdkInstallCommand::new(
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        );
        sdk_install_cmd
            .execute()
            .await
            .with_context(|| "Failed to install SDK dependencies")?;

        // 2. Install extension dependencies
        print_info(
            "Step 2/3: Installing extension dependencies",
            OutputLevel::Normal,
        );
        let ext_install_cmd = ExtInstallCommand::new(
            None, // Install all extensions
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        );
        ext_install_cmd
            .execute()
            .await
            .with_context(|| "Failed to install extension dependencies")?;

        // 3. Install runtime dependencies
        if let Some(ref runtime_name) = self.runtime {
            print_info(
                &format!("Step 3/3: Installing runtime dependencies for '{runtime_name}'"),
                OutputLevel::Normal,
            );
        } else {
            print_info(
                "Step 3/3: Installing runtime dependencies for all runtimes",
                OutputLevel::Normal,
            );
        }
        let runtime_install_cmd = RuntimeInstallCommand::new(
            self.runtime.clone(), // Use the specified runtime or None for all runtimes
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        );
        runtime_install_cmd
            .execute()
            .await
            .with_context(|| "Failed to install runtime dependencies")?;

        print_success(
            "All components installed successfully!",
            OutputLevel::Normal,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = InstallCommand::new(
            "avocado.toml".to_string(),
            true,
            false,
            Some("my-runtime".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.runtime, Some("my-runtime".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = InstallCommand::new(
            "config.toml".to_string(),
            false,
            false,
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_new_with_runtime() {
        let cmd = InstallCommand::new(
            "avocado.toml".to_string(),
            false,
            true,
            Some("test-runtime".to_string()),
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert!(cmd.force);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
