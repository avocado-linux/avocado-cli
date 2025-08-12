use crate::utils::{
    config::load_config,
    output::{print_success, OutputLevel},
};
use anyhow::{Context, Result};

pub struct RuntimeDepsCommand {
    config_path: String,
    runtime_name: String,
}

impl RuntimeDepsCommand {
    pub fn new(config_path: String, runtime_name: String) -> Self {
        Self {
            config_path,
            runtime_name,
        }
    }

    pub fn execute(&self) -> Result<()> {
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_runtime_exists(&parsed)?;
        let dependencies = self.list_runtime_dependencies(&parsed, &self.runtime_name)?;

        self.display_dependencies(&dependencies);
        print_success(
            &format!("Listed {} dependency(s).", dependencies.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn validate_runtime_exists(&self, parsed: &toml::Value) -> Result<()> {
        let runtime_config = parsed
            .get("runtime")
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
        parsed: &toml::Value,
        runtime_name: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        let runtime_spec = runtime_config
            .get(runtime_name)
            .with_context(|| format!("Runtime '{runtime_name}' not found"))?;

        let runtime_deps = runtime_spec.get("dependencies").and_then(|v| v.as_table());

        let mut dependencies = Vec::new();

        if let Some(deps_table) = runtime_deps {
            for (dep_name, dep_spec) in deps_table {
                if let Some(dependency) = self.resolve_dependency(parsed, dep_name, dep_spec) {
                    dependencies.push(dependency);
                }
            }
        }

        self.sort_dependencies(&mut dependencies);
        Ok(dependencies)
    }

    fn resolve_dependency(
        &self,
        parsed: &toml::Value,
        dep_name: &str,
        dep_spec: &toml::Value,
    ) -> Option<(String, String, String)> {
        // Try to resolve as extension reference first
        if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
            return Some(self.resolve_extension_dependency(parsed, ext_name));
        }

        // Otherwise treat as package dependency
        Some(self.resolve_package_dependency(dep_name, dep_spec))
    }

    fn resolve_extension_dependency(
        &self,
        parsed: &toml::Value,
        ext_name: &str,
    ) -> (String, String, String) {
        let version = parsed
            .get("ext")
            .and_then(|ext_config| ext_config.as_table())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_spec| ext_spec.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        ("ext".to_string(), ext_name.to_string(), version.to_string())
    }

    fn resolve_package_dependency(
        &self,
        dep_name: &str,
        dep_spec: &toml::Value,
    ) -> (String, String, String) {
        let version = dep_spec
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        ("pkg".to_string(), dep_name.to_string(), version.to_string())
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
        let config_path = temp_dir.path().join("avocado.toml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    fn create_test_config_content() -> &'static str {
        r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = { version = "11.0" }
app-ext = { ext = "my-extension" }

[ext.my-extension]
version = "2.0"
types = ["sysext"]
"#
    }

    #[test]
    fn test_new() {
        let cmd = RuntimeDepsCommand::new("avocado.toml".to_string(), "test-runtime".to_string());

        assert_eq!(cmd.config_path, "avocado.toml");
        assert_eq!(cmd.runtime_name, "test-runtime");
    }

    #[test]
    fn test_list_runtime_dependencies() {
        let config_content = create_test_config_content();
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeDepsCommand::new("avocado.toml".to_string(), "test-runtime".to_string());

        let deps = cmd
            .list_runtime_dependencies(&parsed, "test-runtime")
            .unwrap();

        assert_eq!(deps.len(), 2);

        // Extensions should come first
        assert_eq!(deps[0].0, "ext");
        assert_eq!(deps[0].1, "my-extension");
        assert_eq!(deps[0].2, "2.0");

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
