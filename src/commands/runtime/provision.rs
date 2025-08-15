use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target_required,
};
use anyhow::{Context, Result};
use std::collections::HashMap;

pub struct RuntimeProvisionConfig {
    pub runtime_name: String,
    pub config_path: String,
    pub verbose: bool,
    pub force: bool,
    pub target: Option<String>,
    pub provision_profile: Option<String>,
    pub env_vars: Option<HashMap<String, String>>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
}

pub struct RuntimeProvisionCommand {
    config: RuntimeProvisionConfig,
}

impl RuntimeProvisionCommand {
    pub fn new(config: RuntimeProvisionConfig) -> Self {
        Self { config }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = load_config(&self.config.config_path)?;
        let content = std::fs::read_to_string(&self.config.config_path)?;
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
        let runtime_spec = runtime_config
            .get(&self.config.runtime_name)
            .with_context(|| {
                format!(
                    "Runtime '{}' not found in configuration",
                    self.config.runtime_name
                )
            })?;

        // Get target from runtime config
        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.config.target.as_deref(), &config)?;

        print_info(
            &format!("Provisioning runtime '{}'", self.config.runtime_name),
            OutputLevel::Normal,
        );

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Create provision script
        let provision_script = self.create_provision_script(&target_arch)?;

        if self.config.verbose {
            print_info("Executing provision script.", OutputLevel::Normal);
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: provision_script,
            verbose: self.config.verbose,
            source_environment: true,
            interactive: !self.config.force,
            env_vars: self.config.env_vars.clone(),
            container_args: config.merge_provision_container_args(
                self.config.provision_profile.as_deref(),
                self.config.container_args.as_ref(),
            ),
            dnf_args: self.config.dnf_args.clone(),
            ..Default::default()
        };
        let provision_result = container_helper
            .run_in_container(run_config)
            .await
            .context("Failed to provision runtime")?;

        if !provision_result {
            return Err(anyhow::anyhow!("Failed to provision runtime"));
        }

        print_success(
            &format!(
                "Successfully provisioned runtime '{}'",
                self.config.runtime_name
            ),
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
            self.config.runtime_name, target_arch, self.config.runtime_name
        );

        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.toml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
    }

    #[test]
    fn test_create_provision_script() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

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

        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            container_args: container_args.clone(),
            dnf_args: dnf_args.clone(),
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.toml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.container_args, container_args);
        assert_eq!(cmd.config.dnf_args, dnf_args);
    }

    #[test]
    fn test_new_with_env_vars() {
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_DEVICE_ID".to_string(), "device123".to_string());
        env_vars.insert(
            "AVOCADO_PROVISION_PROFILE".to_string(),
            "production".to_string(),
        );

        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: Some(env_vars.clone()),
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.toml");
        assert_eq!(cmd.config.env_vars, Some(env_vars));
    }
}
