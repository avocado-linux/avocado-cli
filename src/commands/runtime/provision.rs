use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target,
};
use anyhow::{Context, Result};

pub struct RuntimeProvisionCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeProvisionCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Get SDK configuration
        let sdk_config = parsed.get("sdk").context("No SDK configuration found")?;

        let container_image = sdk_config
            .get("image")
            .and_then(|v| v.as_str())
            .context("No SDK container image specified in configuration")?;

        // Get runtime configuration
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        // Check if runtime exists
        let runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        // Get target from runtime config
        let config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target(self.target.as_deref(), config_target.as_deref())
            .with_context(|| {
                format!(
                    "No target architecture specified for runtime '{}'. Use --target, AVOCADO_TARGET env var, or config under 'runtime.{}.target'",
                    self.runtime_name,
                    self.runtime_name
                )
            })?;

        print_info(
            &format!("Provisioning runtime '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Create provision script
        let provision_script = self.create_provision_script(&target_arch)?;

        if self.verbose {
            print_info("Executing provision script.", OutputLevel::Normal);
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: provision_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: !self.force,
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let provision_result = container_helper
            .run_in_container(config)
            .await
            .context("Failed to provision runtime")?;

        if !provision_result {
            return Err(anyhow::anyhow!("Failed to provision runtime"));
        }

        print_success(
            &format!("Successfully provisioned runtime '{}'", self.runtime_name),
            OutputLevel::Normal,
        );
        Ok(())
    }

    fn create_provision_script(&self, target_arch: &str) -> Result<String> {
        let script = format!(
            r#"
echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-provision' for '{}'."
avocado-provision-{} {}
"#,
            self.runtime_name, target_arch, self.runtime_name
        );

        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeProvisionCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_create_provision_script() {
        let cmd = RuntimeProvisionCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let script = cmd.create_provision_script("x86_64").unwrap();

        assert!(script.contains("avocado-provision-x86_64 test-runtime"));
        assert!(script.contains("Running SDK lifecycle hook 'avocado-provision'"));
    }

    #[test]
    fn test_new_with_container_args() {
        let container_args = Some(vec![
            "--privileged".to_string(),
            "--network=host".to_string(),
        ]);
        let dnf_args = Some(vec!["--nogpgcheck".to_string()]);

        let cmd = RuntimeProvisionCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
            container_args.clone(),
            dnf_args.clone(),
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, container_args);
        assert_eq!(cmd.dnf_args, dnf_args);
    }
}
