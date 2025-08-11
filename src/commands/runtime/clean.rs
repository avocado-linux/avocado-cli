use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

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
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_runtime_exists(&parsed)?;
        let container_image = self.get_container_image(&parsed)?;
        let target = self.resolve_target_architecture(&parsed)?;

        self.clean_runtime(&container_image, &target).await
    }

    fn validate_runtime_exists(&self, parsed: &toml::Value) -> Result<()> {
        let runtime_section = parsed.get("runtime").ok_or_else(|| {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No runtime section found")
        })?;

        let runtime_table = runtime_section
            .as_table()
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

    fn get_container_image(&self, parsed: &toml::Value) -> Result<String> {
        parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, parsed: &toml::Value) -> Result<String> {
        let config_target = self.extract_config_target(parsed);
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());

        resolved_target.ok_or_else(|| {
            anyhow::anyhow!(
                "No target architecture specified for runtime '{}'. Use --target, AVOCADO_TARGET env var, or config under 'runtime.{}.target'.",
                self.runtime, self.runtime
            )
        })
    }

    fn extract_config_target(&self, parsed: &toml::Value) -> Option<String> {
        parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_table())
            .and_then(|runtime_table| runtime_table.get(&self.runtime))
            .and_then(|runtime_config| runtime_config.get("target"))
            .and_then(|target| target.as_str())
            .map(|s| s.to_string())
    }

    async fn clean_runtime(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning runtime '{}'...", self.runtime),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();
        let clean_command = format!("rm -rf $AVOCADO_PREFIX/runtimes/{}", self.runtime);

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
            container_args: self.container_args.clone(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, None);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }
}
