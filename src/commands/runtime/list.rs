use crate::utils::{
    config::load_config,
    output::{print_success, OutputLevel},
};
use anyhow::Result;

pub struct RuntimeListCommand {
    config_path: String,
}

impl RuntimeListCommand {
    pub fn new(config_path: String) -> Self {
        Self { config_path }
    }

    pub fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if runtime section exists
        if let Some(runtime_config) = parsed.get("runtime").and_then(|v| v.as_table()) {
            // List all runtime names
            let mut runtimes: Vec<&String> = runtime_config.keys().collect();
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
        let config_path = temp_dir.path().join("avocado.toml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_new() {
        let cmd = RuntimeListCommand::new("avocado.toml".to_string());
        assert_eq!(cmd.config_path, "avocado.toml");
    }

    #[test]
    fn test_execute_with_runtimes() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.app]
target = "x86_64"

[runtime.server]
target = "aarch64"
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
[sdk]
image = "test-image"
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
