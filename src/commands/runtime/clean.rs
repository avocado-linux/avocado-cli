use anyhow::Result;

use crate::utils::config::{load_config, Config};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct RuntimeCleanCommand {
    runtime: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeCleanCommand {
    pub fn new(
        runtime: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        self.validate_runtime_exists(&parsed)?;
        let container_image = self.get_container_image(&config)?;
        let target = self.resolve_target_architecture(&config)?;

        self.clean_runtime(&container_image, &target).await
    }

    fn validate_runtime_exists(&self, parsed: &serde_yaml::Value) -> Result<()> {
        let runtime_section = parsed.get("runtime").ok_or_else(|| {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No runtime section found")
        })?;

        let runtime_table = runtime_section
            .as_mapping()
            .ok_or_else(|| anyhow::anyhow!("Invalid runtime section format"))?;

        if !runtime_table.contains_key(&self.runtime) {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Runtime not found"));
        }

        Ok(())
    }

    fn get_container_image(&self, config: &Config) -> Result<String> {
        config
            .get_sdk_image()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, config: &crate::utils::config::Config) -> Result<String> {
        resolve_target_required(self.target.as_deref(), config)
    }

    async fn clean_runtime(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning runtime '{}'...", self.runtime),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();

        // Clean runtime directory and stamps
        let clean_command = format!(
            r#"
# Clean runtime build directory
rm -rf "$AVOCADO_PREFIX/runtimes/{runtime}"

# Clean runtime stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/runtime/{runtime}"
"#,
            runtime = self.runtime
        );

        if self.verbose {
            print_info(
                &format!("Running command: {clean_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: crate::utils::config::Config::process_container_args(
                self.container_args.as_ref(),
            ),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success(
                &format!("Successfully cleaned runtime '{}'.", self.runtime),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            print_error(
                &format!("Failed to clean runtime '{}'.", self.runtime),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!("Clean command failed"))
        }
    }

    /// Generate the clean command script for testing
    #[cfg(test)]
    fn generate_clean_script(&self) -> String {
        format!(
            r#"
# Clean runtime build directory
rm -rf "$AVOCADO_PREFIX/runtimes/{runtime}"

# Clean runtime stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/runtime/{runtime}"
"#,
            runtime = self.runtime
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, None);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_clean_script_cleans_runtime_directory() {
        let cmd = RuntimeCleanCommand::new(
            "production".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean runtime build directory
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/runtimes/production""#));
    }

    #[test]
    fn test_clean_script_cleans_stamps() {
        let cmd = RuntimeCleanCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean runtime stamps (install and build)
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/.stamps/runtime/dev""#));
    }

    #[test]
    fn test_clean_script_includes_all_cleanup_targets() {
        let cmd = RuntimeCleanCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Verify both cleanup targets are present
        assert!(
            script.contains("runtimes/my-runtime"),
            "Should clean runtime directory"
        );
        assert!(
            script.contains(".stamps/runtime/my-runtime"),
            "Should clean stamps"
        );
    }
}
