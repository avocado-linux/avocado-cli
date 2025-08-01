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
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if runtime section exists
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        // Check if runtime exists
        let _runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        // List dependencies for the runtime
        let dependencies = self.list_runtime_dependencies(&parsed, &self.runtime_name)?;

        for (dep_type, dep_name, dep_version) in &dependencies {
            println!("({}) {} ({})", dep_type, dep_name, dep_version);
        }

        // Print success message with count
        print_success(
            &format!("Listed {} dependency(s).", dependencies.len()),
            OutputLevel::Normal,
        );

        Ok(())
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
            .with_context(|| format!("Runtime '{}' not found", runtime_name))?;

        let binding = toml::map::Map::new();
        let runtime_deps = runtime_spec
            .get("dependencies")
            .and_then(|v| v.as_table())
            .unwrap_or(&binding);

        let mut dependencies = Vec::new();

        for (dep_name, dep_spec) in runtime_deps {
            if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                // This is an extension reference
                let mut version = "*".to_string();

                // Get version from extension config if available
                if let Some(ext_config) = parsed.get("ext").and_then(|v| v.as_table()) {
                    if let Some(ext_spec) = ext_config.get(ext_name) {
                        if let Some(ext_version) = ext_spec.get("version").and_then(|v| v.as_str())
                        {
                            version = ext_version.to_string();
                        }
                    }
                }

                dependencies.push(("ext".to_string(), ext_name.to_string(), version));
            } else {
                // This is a package
                let version = dep_spec
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("*")
                    .to_string();
                dependencies.push(("pkg".to_string(), dep_name.clone(), version));
            }
        }

        // Sort: extensions first, then packages, both alphabetically
        dependencies.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
            ("ext", "pkg") => std::cmp::Ordering::Less,
            ("pkg", "ext") => std::cmp::Ordering::Greater,
            _ => a.1.cmp(&b.1),
        });

        Ok(dependencies)
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
sysext = true
confext = false
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
