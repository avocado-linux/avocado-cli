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

        // Determine extensions required for this runtime/target combination
        let runtime_extensions = Self::collect_runtime_extensions(
            &parsed,
            &config,
            &self.config.runtime_name,
            target_arch.as_str(),
            &self.config.config_path,
        )?;

        // Merge CLI env vars with AVOCADO_EXT_LIST if any extensions exist
        let mut env_vars = self.config.env_vars.clone().unwrap_or_default();
        if !runtime_extensions.is_empty() {
            env_vars.insert("AVOCADO_EXT_LIST".to_string(), runtime_extensions.join(" "));
        }
        let env_vars = if env_vars.is_empty() {
            None
        } else {
            Some(env_vars)
        };

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
            env_vars,
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

    fn collect_runtime_extensions(
        parsed: &toml::Value,
        config: &crate::utils::config::Config,
        runtime_name: &str,
        target_arch: &str,
        config_path: &str,
    ) -> Result<Vec<String>> {
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target_arch, config_path)?;

        let runtime_dep_table = merged_runtime
            .as_ref()
            .and_then(|value| value.get("dependencies").and_then(|d| d.as_table()))
            .or_else(|| {
                parsed
                    .get("runtime")
                    .and_then(|r| r.get(runtime_name))
                    .and_then(|runtime_value| runtime_value.get("dependencies"))
                    .and_then(|d| d.as_table())
            });

        let mut extensions = Vec::new();

        if let Some(deps) = runtime_dep_table {
            for dep_spec in deps.values() {
                if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                    let version = Self::resolve_extension_version(
                        parsed,
                        config,
                        config_path,
                        ext_name,
                        dep_spec,
                    )?;
                    extensions.push(format!("{ext_name}-{version}"));
                }
            }
        }

        extensions.sort();
        extensions.dedup();

        Ok(extensions)
    }

    fn resolve_extension_version(
        parsed: &toml::Value,
        config: &crate::utils::config::Config,
        config_path: &str,
        ext_name: &str,
        dep_spec: &toml::Value,
    ) -> Result<String> {
        if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
            return Ok(version.to_string());
        }

        if let Some(external_config_path) = dep_spec.get("config").and_then(|v| v.as_str()) {
            let external_extensions =
                config.load_external_extensions(config_path, external_config_path)?;
            if let Some(ext_config) = external_extensions.get(ext_name) {
                if let Some(version) = ext_config.get("version").and_then(|v| v.as_str()) {
                    return Ok(version.to_string());
                }
            }
            return Ok("*".to_string());
        }

        let version = parsed
            .get("ext")
            .and_then(|ext_section| ext_section.as_table())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .to_string();

        Ok(version)
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
    fn test_collect_runtime_extensions() {
        use std::fs;
        use tempfile::TempDir;

        let config_content = r#"
[sdk]
image = "docker.io/avocado/sdk:latest"

[runtime.test-runtime]
[runtime.test-runtime.dependencies]
ext_one = { ext = "alpha-ext" }
ext_two = { ext = "beta-ext" }
        "#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("avocado.toml");
        fs::write(&config_path, config_content).unwrap();

        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let config = crate::utils::config::Config::load(&config_path).unwrap();

        let extensions = RuntimeProvisionCommand::collect_runtime_extensions(
            &parsed,
            &config,
            "test-runtime",
            "x86_64",
            config_path.to_str().unwrap(),
        )
        .unwrap();

        assert_eq!(
            extensions,
            vec!["alpha-ext-*".to_string(), "beta-ext-*".to_string()]
        );
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
