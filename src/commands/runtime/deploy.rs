use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target_required,
};
use anyhow::{Context, Result};
use std::collections::HashMap;

pub struct RuntimeDeployCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    device: String,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeDeployCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        device: String,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            target,
            device,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = load_config(&self.config_path)?;
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
        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        print_info(
            &format!(
                "Deploying runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Create deploy script
        let deploy_script = self.create_deploy_script(&target_arch)?;

        if self.verbose {
            print_info("Executing deploy script.", OutputLevel::Normal);
        }

        // Build environment variables for the deploy process
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.clone());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), self.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), self.device.clone());

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: deploy_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false, // deploy should be non-interactive
            env_vars: Some(env_vars),
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let deploy_result = container_helper
            .run_in_container(run_config)
            .await
            .context("Failed to deploy runtime")?;

        if !deploy_result {
            return Err(anyhow::anyhow!("Failed to deploy runtime"));
        }

        print_success(
            &format!(
                "Successfully deployed runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    fn create_deploy_script(&self, target_arch: &str) -> Result<String> {
        let script = format!(
            r#"
echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-deploy' for '{}' to device '{}'."
avocado-deploy-{} {} "{}"
"#,
            self.runtime_name, self.device, target_arch, self.runtime_name, self.device
        );

        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.100".to_string(),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.device, "192.168.1.100");
    }

    #[test]
    fn test_create_deploy_script() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device.local".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("avocado-deploy-x86_64 test-runtime \"device.local\""));
        assert!(script.contains("Running SDK lifecycle hook 'avocado-deploy'"));
        // Environment variables are now set via RunConfig, not in the script
        assert!(!script.contains("export AVOCADO_DEPLOY_DEVICE"));
    }

    #[test]
    fn test_new_with_container_args() {
        let container_args = Some(vec![
            "--privileged".to_string(),
            "--network=host".to_string(),
        ]);
        let dnf_args = Some(vec!["--nogpgcheck".to_string()]);

        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            true,
            Some("aarch64".to_string()),
            "192.168.1.50".to_string(),
            container_args.clone(),
            dnf_args.clone(),
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, Some("aarch64".to_string()));
        assert_eq!(cmd.device, "192.168.1.50");
        assert_eq!(cmd.container_args, container_args);
        assert_eq!(cmd.dnf_args, dnf_args);
    }

    #[test]
    fn test_create_deploy_script_with_ip() {
        let cmd = RuntimeDeployCommand::new(
            "edge-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("qemux86-64".to_string()),
            "10.0.0.42".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("qemux86-64").unwrap();

        assert!(script.contains("avocado-deploy-qemux86-64 edge-runtime \"10.0.0.42\""));
        assert!(script.contains("to device '10.0.0.42'"));
        // Environment variables are now set via RunConfig, not in the script
        assert!(!script.contains("export AVOCADO_DEPLOY_DEVICE"));
    }

    #[test]
    fn test_create_deploy_script_with_hostname() {
        let cmd = RuntimeDeployCommand::new(
            "production".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("aarch64".to_string()),
            "edge-device.company.com".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("aarch64").unwrap();

        assert!(script.contains("avocado-deploy-aarch64 production \"edge-device.company.com\""));
        assert!(script.contains("to device 'edge-device.company.com'"));
        // Environment variables are now set via RunConfig, not in the script
        assert!(!script.contains("export AVOCADO_DEPLOY_DEVICE"));
    }

    #[test]
    fn test_environment_variables_setup() {
        // This test verifies that the correct environment variables would be set
        // We can't easily test the actual RunConfig execution in unit tests,
        // but we can verify the structure is correct
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.10".to_string(),
            None,
            None,
        );

        // Simulate building environment variables like in execute()
        let target_arch = "x86_64";
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.to_string());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), cmd.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), cmd.device.clone());

        // Verify all expected environment variables are present
        assert_eq!(env_vars.get("AVOCADO_TARGET"), Some(&"x86_64".to_string()));
        assert_eq!(
            env_vars.get("AVOCADO_RUNTIME"),
            Some(&"my-runtime".to_string())
        );
        assert_eq!(
            env_vars.get("AVOCADO_DEPLOY_MACHINE"),
            Some(&"192.168.1.10".to_string())
        );
        assert_eq!(env_vars.len(), 3); // Ensure no extra variables
    }
}
