use crate::utils::{
    config::{ComposedConfig, Config},
    output::{print_success, OutputLevel},
};
use anyhow::{Context, Result};
use std::sync::Arc;

pub struct RuntimeDepsCommand {
    config_path: String,
    runtime_name: String,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RuntimeDepsCommand {
    pub fn new(config_path: String, runtime_name: String) -> Self {
        Self {
            config_path,
            runtime_name,
            composed_config: None,
        }
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(&self.config_path, None)?),
        };
        let parsed = &composed.merged_value;

        self.validate_runtime_exists(parsed)?;
        let dependencies = self.list_runtime_dependencies(parsed, &self.runtime_name)?;

        self.display_dependencies(&dependencies);
        print_success(
            &format!("Listed {} dependency(s).", dependencies.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn validate_runtime_exists(&self, parsed: &serde_yaml::Value) -> Result<()> {
        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        Ok(())
    }

    fn display_dependencies(&self, dependencies: &[(String, String, String)]) {
        for (dep_type, dep_name, dep_version) in dependencies {
            println!("({dep_type}) {dep_name} ({dep_version})");
        }
    }

    fn list_runtime_dependencies(
        &self,
        parsed: &serde_yaml::Value,
        runtime_name: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        let runtime_spec = runtime_config
            .get(runtime_name)
            .with_context(|| format!("Runtime '{runtime_name}' not found"))?;

        let mut dependencies = Vec::new();

        // New way: Read extensions from the `extensions` array
        if let Some(extensions) = runtime_spec.get("extensions").and_then(|e| e.as_sequence()) {
            for ext in extensions {
                if let Some(ext_name) = ext.as_str() {
                    dependencies.push(self.resolve_extension_dependency(parsed, ext_name));
                }
            }
        }

        // Read package dependencies from the `dependencies` section
        if let Some(deps_table) = runtime_spec.get("packages").and_then(|v| v.as_mapping()) {
            for (dep_name_val, dep_spec) in deps_table {
                if let Some(dep_name) = dep_name_val.as_str() {
                    dependencies.push(self.resolve_package_dependency(dep_name, dep_spec));
                }
            }
        }

        self.sort_dependencies(&mut dependencies);
        Ok(dependencies)
    }

    fn resolve_extension_dependency(
        &self,
        parsed: &serde_yaml::Value,
        ext_name: &str,
    ) -> (String, String, String) {
        let version = parsed
            .get("extensions")
            .and_then(|ext_config| ext_config.as_mapping())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_spec| ext_spec.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        ("ext".to_string(), ext_name.to_string(), version.to_string())
    }

    fn resolve_package_dependency(
        &self,
        dep_name: &str,
        dep_spec: &serde_yaml::Value,
    ) -> (String, String, String) {
        // Version can be a string directly or in a mapping with 'version' key
        let version = if let Some(v) = dep_spec.as_str() {
            v.to_string()
        } else {
            dep_spec
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("*")
                .to_string()
        };

        ("pkg".to_string(), dep_name.to_string(), version)
    }

    fn sort_dependencies(&self, dependencies: &mut [(String, String, String)]) {
        dependencies.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
            ("ext", "pkg") => std::cmp::Ordering::Less,
            ("pkg", "ext") => std::cmp::Ordering::Greater,
            _ => a.1.cmp(&b.1),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config_file(temp_dir: &TempDir, content: &str) -> String {
        let config_path = temp_dir.path().join("avocado.yaml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    fn create_test_config_content() -> &'static str {
        r#"
sdk:
  image: "test-image"

runtimes:
  test-runtime:
    target: "x86_64"
    extensions:
      - my-extension
    packages:
      gcc: "11.0"

extensions:
  my-extension:
    version: "2.0.0"
    types:
      - sysext
"#
    }

    #[test]
    fn test_new() {
        let cmd = RuntimeDepsCommand::new("avocado.yaml".to_string(), "test-runtime".to_string());

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert_eq!(cmd.runtime_name, "test-runtime");
    }

    #[test]
    fn test_list_runtime_dependencies() {
        let config_content = create_test_config_content();
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeDepsCommand::new("avocado.yaml".to_string(), "test-runtime".to_string());

        let deps = cmd
            .list_runtime_dependencies(&parsed, "test-runtime")
            .unwrap();

        assert_eq!(deps.len(), 2);

        // Extensions should come first
        assert_eq!(deps[0].0, "ext");
        assert_eq!(deps[0].1, "my-extension");
        assert_eq!(deps[0].2, "2.0.0");

        // Then packages
        assert_eq!(deps[1].0, "pkg");
        assert_eq!(deps[1].1, "gcc");
        assert_eq!(deps[1].2, "11.0");
    }

    #[test]
    fn test_execute_with_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = create_test_config_content();
        let config_path = create_test_config_file(&temp_dir, config_content);
        let cmd = RuntimeDepsCommand::new(config_path, "test-runtime".to_string());

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_execute_runtime_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = create_test_config_content();
        let config_path = create_test_config_file(&temp_dir, config_content);
        let cmd = RuntimeDepsCommand::new(config_path, "nonexistent".to_string());

        let result = cmd.execute();
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_no_runtime_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let cmd = RuntimeDepsCommand::new(config_path, "app".to_string());

        let result = cmd.execute();
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_invalid_config() {
        let cmd =
            RuntimeDepsCommand::new("nonexistent.toml".to_string(), "test-runtime".to_string());
        let result = cmd.execute();
        assert!(result.is_err());
    }
}
