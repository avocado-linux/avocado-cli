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
    pub out: Option<String>,
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
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
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
        // This includes local extensions, external extensions, and versioned extensions from ext repos
        // For package repository extensions, we query the RPM database to get actual installed versions
        let resolved_extensions = self
            .collect_runtime_extensions(
                &parsed,
                &config,
                &self.config.runtime_name,
                target_arch.as_str(),
                &self.config.config_path,
                container_image,
            )
            .await?;

        // Merge CLI env vars with AVOCADO_EXT_LIST if any extensions exist
        let mut env_vars = self.config.env_vars.clone().unwrap_or_default();
        if !resolved_extensions.is_empty() {
            env_vars.insert(
                "AVOCADO_EXT_LIST".to_string(),
                resolved_extensions.join(" "),
            );
        }

        // Set AVOCADO_PROVISION_OUT if --out is specified
        if let Some(out_path) = &self.config.out {
            // Construct the absolute path from the container's perspective
            // The src_dir is mounted at /opt/src in the container
            let container_out_path = format!("/opt/src/{out_path}");
            env_vars.insert("AVOCADO_PROVISION_OUT".to_string(), container_out_path);
        }

        // Set AVOCADO_STONE_INCLUDE_PATHS if configured
        if let Some(stone_paths) = config.get_stone_include_paths_for_runtime(
            &self.config.runtime_name,
            &target_arch,
            &self.config.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_INCLUDE_PATHS".to_string(), stone_paths);
        }

        // Set AVOCADO_STONE_MANIFEST if configured
        if let Some(stone_manifest) = config.get_stone_manifest_for_runtime(
            &self.config.runtime_name,
            &target_arch,
            &self.config.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_MANIFEST".to_string(), stone_manifest);
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

    async fn collect_runtime_extensions(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        runtime_name: &str,
        target_arch: &str,
        config_path: &str,
        container_image: &str,
    ) -> Result<Vec<String>> {
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target_arch, config_path)?;

        let runtime_dep_table = merged_runtime
            .as_ref()
            .and_then(|value| value.get("dependencies").and_then(|d| d.as_mapping()))
            .or_else(|| {
                parsed
                    .get("runtime")
                    .and_then(|r| r.get(runtime_name))
                    .and_then(|runtime_value| runtime_value.get("dependencies"))
                    .and_then(|d| d.as_mapping())
            });

        let mut extensions = Vec::new();

        if let Some(deps) = runtime_dep_table {
            for dep_spec in deps.values() {
                if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                    let version = self
                        .resolve_extension_version(
                            parsed,
                            config,
                            config_path,
                            ext_name,
                            dep_spec,
                            container_image,
                            target_arch,
                        )
                        .await?;
                    extensions.push(format!("{ext_name}-{version}"));
                }
            }
        }

        extensions.sort();
        extensions.dedup();

        Ok(extensions)
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_extension_version(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        config_path: &str,
        ext_name: &str,
        dep_spec: &serde_yaml::Value,
        container_image: &str,
        target_arch: &str,
    ) -> Result<String> {
        // If version is explicitly specified with vsn field, use it (unless it's a wildcard)
        if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If vsn is "*", fall through to query RPM for the actual installed version
        }

        // If external config is specified, try to get version from it
        if let Some(external_config_path) = dep_spec.get("config").and_then(|v| v.as_str()) {
            let external_extensions =
                config.load_external_extensions(config_path, external_config_path)?;
            if let Some(ext_config) = external_extensions.get(ext_name) {
                if let Some(version) = ext_config.get("version").and_then(|v| v.as_str()) {
                    if version != "*" {
                        return Ok(version.to_string());
                    }
                    // If version is "*", fall through to query RPM
                }
            }
            // External config but no version found or version is "*" - query RPM database
            return self
                .query_rpm_version(ext_name, container_image, target_arch)
                .await;
        }

        // Try to get version from local [ext] section
        if let Some(version) = parsed
            .get("ext")
            .and_then(|ext_section| ext_section.as_mapping())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
        {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If version is "*", fall through to query RPM
        }

        // No version found in config - this is likely a package repository extension
        // Query RPM database for the installed version
        self.query_rpm_version(ext_name, container_image, target_arch)
            .await
    }

    /// Query RPM database for the actual installed version of an extension
    ///
    /// This queries the RPM database in the extension's sysroot at $AVOCADO_EXT_SYSROOTS/{ext_name}
    /// to get the actual installed version. This ensures AVOCADO_EXT_LIST contains
    /// precise version information.
    async fn query_rpm_version(
        &self,
        ext_name: &str,
        container_image: &str,
        target: &str,
    ) -> Result<String> {
        let container_helper = SdkContainer::new();

        let version_query_script = format!(
            r#"
set -e
# Query RPM version for extension from RPM database using the same config as installation
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
rpm --root="$AVOCADO_EXT_SYSROOTS/{ext_name}" --dbpath=/var/lib/extension.d/rpm -q {ext_name} --queryformat '%{{VERSION}}'
"#
        );

        let version_query_config = crate::utils::container::RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: version_query_script,
            verbose: self.config.verbose,
            source_environment: true,
            interactive: false,
            ..Default::default()
        };

        match container_helper
            .run_in_container_with_output(version_query_config)
            .await
        {
            Ok(Some(actual_version)) => {
                let trimmed_version = actual_version.trim();
                if self.config.verbose {
                    print_info(
                        &format!(
                            "Resolved extension '{ext_name}' to version '{trimmed_version}' from RPM database"
                        ),
                        OutputLevel::Normal,
                    );
                }
                Ok(trimmed_version.to_string())
            }
            Ok(None) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
            Err(e) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database: {e}. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.out, None);
    }

    #[test]
    fn test_create_provision_script() {
        let config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        let script = cmd.create_provision_script("x86_64").unwrap();

        assert!(script.contains("avocado-provision-x86_64 test-runtime"));
        assert!(script.contains("Running SDK lifecycle hook 'avocado-provision'"));
    }

    #[tokio::test]
    async fn test_collect_runtime_extensions() {
        use std::fs;
        use tempfile::TempDir;

        let config_content = r#"
sdk:
  image: "docker.io/avocado/sdk:latest"

runtime:
  test-runtime:
    dependencies:
      ext_one:
        ext: alpha-ext
        vsn: "1.0.0"
      ext_two:
        ext: beta-ext
        vsn: "2.0.0"
        "#;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("avocado.yaml");
        fs::write(&config_path, config_content).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let config = crate::utils::config::Config::load(&config_path).unwrap();

        let provision_config = RuntimeProvisionConfig {
            runtime_name: "test-runtime".to_string(),
            config_path: config_path.to_str().unwrap().to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: None,
            dnf_args: None,
        };

        let command = RuntimeProvisionCommand::new(provision_config);

        let extensions = command
            .collect_runtime_extensions(
                &parsed,
                &config,
                "test-runtime",
                "x86_64",
                config_path.to_str().unwrap(),
                "docker.io/avocado/sdk:latest",
            )
            .await
            .unwrap();

        assert_eq!(
            extensions,
            vec!["alpha-ext-1.0.0".to_string(), "beta-ext-2.0.0".to_string()]
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
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: None,
            out: None,
            container_args: container_args.clone(),
            dnf_args: dnf_args.clone(),
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert!(!cmd.config.verbose);
        assert!(!cmd.config.force);
        assert_eq!(cmd.config.target, Some("x86_64".to_string()));
        assert_eq!(cmd.config.env_vars, None);
        assert_eq!(cmd.config.out, None);
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
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            force: false,
            target: Some("x86_64".to_string()),
            provision_profile: None,
            env_vars: Some(env_vars.clone()),
            out: None,
            container_args: None,
            dnf_args: None,
        };
        let cmd = RuntimeProvisionCommand::new(config);

        assert_eq!(cmd.config.runtime_name, "test-runtime");
        assert_eq!(cmd.config.config_path, "avocado.yaml");
        assert_eq!(cmd.config.env_vars, Some(env_vars));
    }
}
