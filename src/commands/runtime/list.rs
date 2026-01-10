use crate::utils::{
    config::{ComposedConfig, Config},
    output::{print_success, OutputLevel},
};
use anyhow::Result;
use std::sync::Arc;

pub struct RuntimeListCommand {
    config_path: String,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RuntimeListCommand {
    pub fn new(config_path: String) -> Self {
        Self {
            config_path,
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

        // Check if runtime section exists
        if let Some(runtime_config) = parsed.get("runtimes").and_then(|v| v.as_mapping()) {
            // List all runtime names
            let mut runtimes: Vec<String> = runtime_config
                .keys()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect();
            runtimes.sort();

            for runtime_name in &runtimes {
                println!("{runtime_name}");
            }

            print_success(
                &format!("Listed {} runtime(s).", runtimes.len()),
                OutputLevel::Normal,
            );
        } else {
            print_success("Listed 0 runtime(s).", OutputLevel::Normal);
        }

        Ok(())
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

    #[test]
    fn test_new() {
        let cmd = RuntimeListCommand::new("avocado.yaml".to_string());
        assert_eq!(cmd.config_path, "avocado.yaml");
    }

    #[test]
    fn test_execute_with_runtimes() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

runtimes:
  app:
    target: "x86_64"
  server:
    target: "aarch64"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let cmd = RuntimeListCommand::new(config_path);

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_execute_without_runtimes() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let cmd = RuntimeListCommand::new(config_path);

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_execute_invalid_config() {
        let cmd = RuntimeListCommand::new("nonexistent.toml".to_string());
        let result = cmd.execute();
        assert!(result.is_err());
    }
}
