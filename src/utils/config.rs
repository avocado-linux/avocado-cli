//! Configuration utilities for Avocado CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Configuration error type
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Configuration file '{0}' not found")]
    FileNotFound(String),
    #[error("Failed to parse configuration: {0}")]
    #[allow(dead_code)]
    ParseError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Runtime configuration section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeConfig {
    pub target: Option<String>,
    pub dependencies: Option<HashMap<String, toml::Value>>,
}

/// SDK configuration section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SdkConfig {
    pub image: Option<String>,
    pub dependencies: Option<HashMap<String, toml::Value>>,
    pub compile: Option<HashMap<String, CompileConfig>>,
    pub repo_url: Option<String>,
    pub repo_release: Option<String>,
    pub container_args: Option<Vec<String>>,
}

/// Compile configuration for SDK
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompileConfig {
    pub compile: Option<String>,
    pub dependencies: Option<HashMap<String, toml::Value>>,
}

/// Main configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub runtime: Option<HashMap<String, RuntimeConfig>>,
    pub sdk: Option<SdkConfig>,
}

impl Config {
    /// Load configuration from a TOML file
    pub fn load<P: AsRef<Path>>(config_path: P) -> Result<Self> {
        let path = config_path.as_ref();

        if !path.exists() {
            return Err(ConfigError::FileNotFound(path.display().to_string()).into());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        Self::load_from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    /// Load configuration from a TOML string
    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: Config =
            toml::from_str(content).with_context(|| "Failed to parse TOML configuration")?;

        Ok(config)
    }

    /// Get the SDK image from configuration
    pub fn get_sdk_image(&self) -> Option<&String> {
        self.sdk.as_ref()?.image.as_ref()
    }

    /// Get SDK dependencies
    pub fn get_sdk_dependencies(&self) -> Option<&HashMap<String, toml::Value>> {
        self.sdk.as_ref()?.dependencies.as_ref()
    }

    /// Get the SDK repo URL from configuration
    pub fn get_sdk_repo_url(&self) -> Option<&String> {
        self.sdk.as_ref()?.repo_url.as_ref()
    }

    /// Get the SDK repo release from configuration
    pub fn get_sdk_repo_release(&self) -> Option<&String> {
        self.sdk.as_ref()?.repo_release.as_ref()
    }

    /// Get the SDK container args from configuration
    pub fn get_sdk_container_args(&self) -> Option<&Vec<String>> {
        self.sdk.as_ref()?.container_args.as_ref()
    }

    /// Expand environment variables in a string
    pub fn expand_env_vars(input: &str) -> String {
        let mut result = input.to_string();

        // Find and replace $VAR and ${VAR} patterns
        while let Some(start) = result.find('$') {
            let after_dollar = &result[start + 1..];

            let (var_name, end_pos) = if after_dollar.starts_with('{') {
                // Handle ${VAR} format
                if let Some(close_brace) = after_dollar.find('}') {
                    let var_name = &after_dollar[1..close_brace];
                    (var_name, start + close_brace + 2)
                } else {
                    // Malformed ${VAR without closing brace, skip this $
                    result.replace_range(start..start + 1, "\\$");
                    continue;
                }
            } else {
                // Handle $VAR format (alphanumeric + underscore)
                let var_end = after_dollar
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(after_dollar.len());

                if var_end == 0 {
                    // Just a lone $, skip it
                    result.replace_range(start..start + 1, "\\$");
                    continue;
                }

                let var_name = &after_dollar[..var_end];
                (var_name, start + var_end + 1)
            };

            // Get the environment variable value
            let var_value = std::env::var(var_name).unwrap_or_default();

            // Replace the variable reference with its value
            let pattern_start = start;
            result.replace_range(pattern_start..end_pos, &var_value);
        }

        // Unescape any literal $ that were escaped
        result.replace("\\$", "$")
    }

    /// Process container args by expanding environment variables
    /// This is a universal function that can be used by any command
    pub fn process_container_args(args: Option<&Vec<String>>) -> Option<Vec<String>> {
        args.map(|args_vec| {
            args_vec
                .iter()
                .map(|arg| Self::expand_env_vars(arg))
                .collect()
        })
    }

    /// Merge SDK container args from config with CLI args, expanding environment variables
    /// Returns a new Vec containing config args first, then CLI args
    pub fn merge_sdk_container_args(&self, cli_args: Option<&Vec<String>>) -> Option<Vec<String>> {
        let config_args = self.get_sdk_container_args();

        match (config_args, cli_args) {
            (Some(config), Some(cli)) => {
                let mut merged = Self::process_container_args(Some(config)).unwrap_or_default();
                merged.extend(Self::process_container_args(Some(cli)).unwrap_or_default());
                Some(merged)
            }
            (Some(config), None) => Self::process_container_args(Some(config)),
            (None, Some(cli)) => Self::process_container_args(Some(cli)),
            (None, None) => None,
        }
    }

    /// Get compile section dependencies
    pub fn get_compile_dependencies(&self) -> HashMap<String, &HashMap<String, toml::Value>> {
        let mut compile_deps = HashMap::new();

        if let Some(sdk) = &self.sdk {
            if let Some(compile) = &sdk.compile {
                for (section_name, compile_config) in compile {
                    if let Some(dependencies) = &compile_config.dependencies {
                        compile_deps.insert(section_name.clone(), dependencies);
                    }
                }
            }
        }

        compile_deps
    }

    /// Get extension SDK dependencies from configuration
    /// Returns a HashMap where keys are extension names and values are their SDK dependencies
    pub fn get_extension_sdk_dependencies(
        &self,
        config_content: &str,
    ) -> Result<HashMap<String, HashMap<String, toml::Value>>> {
        let parsed: toml::Value =
            toml::from_str(config_content).with_context(|| "Failed to parse TOML configuration")?;

        let mut extension_sdk_deps = HashMap::new();

        if let Some(ext_section) = parsed.get("ext") {
            if let Some(ext_table) = ext_section.as_table() {
                for (ext_name, ext_config) in ext_table {
                    if let Some(ext_config_table) = ext_config.as_table() {
                        if let Some(sdk_section) = ext_config_table.get("sdk") {
                            if let Some(sdk_table) = sdk_section.as_table() {
                                if let Some(dependencies) = sdk_table.get("dependencies") {
                                    if let Some(deps_table) = dependencies.as_table() {
                                        let deps_map: HashMap<String, toml::Value> = deps_table
                                            .iter()
                                            .map(|(k, v)| (k.clone(), v.clone()))
                                            .collect();
                                        extension_sdk_deps.insert(ext_name.clone(), deps_map);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(extension_sdk_deps)
    }

    /// Get target from configuration
    /// Returns the target if there's exactly one runtime configuration
    pub fn get_target(&self) -> Option<String> {
        let runtime = self.runtime.as_ref()?;

        // Find all runtime configurations (nested dictionaries)
        let runtime_configs: Vec<&RuntimeConfig> = runtime.values().collect();

        // If exactly one runtime configuration, use its target
        if runtime_configs.len() == 1 {
            runtime_configs[0].target.clone()
        } else {
            // If multiple or no runtime configurations, return None
            None
        }
    }
}

/// Convenience function to load a config file
#[allow(dead_code)]
pub fn load_config<P: AsRef<Path>>(config_path: P) -> Result<Config> {
    Config::load(config_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_config() {
        let config_content = r#"
[runtime.default]
target = "qemux86-64"

[runtime.default.dependencies]
nativesdk-avocado-images = "*"

[sdk]
image = "avocadolinux/sdk:apollo-edge"

[sdk.dependencies]
cmake = "*"

[sdk.compile.app]
dependencies = { gcc = "*" }
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();

        assert_eq!(config.get_target(), Some("qemux86-64".to_string()));
        assert_eq!(
            config.get_sdk_image(),
            Some(&"avocadolinux/sdk:apollo-edge".to_string())
        );
        assert!(config.get_sdk_dependencies().is_some());
        assert!(!config.get_compile_dependencies().is_empty());
    }

    #[test]
    fn test_load_nonexistent_config() {
        let result = Config::load("nonexistent.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_extension_sdk_dependencies() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"

[ext.avocado-dev]
types = ["sysext", "confext"]

[ext.avocado-dev.sdk.dependencies]
nativesdk-avocado-hitl = "*"
nativesdk-something-else = "1.2.3"

[ext.another-ext]
types = ["sysext"]

[ext.another-ext.sdk.dependencies]
nativesdk-tool = "*"
"#;

        let config = Config::load_from_str(config_content).unwrap();
        let extension_deps = config
            .get_extension_sdk_dependencies(config_content)
            .unwrap();

        assert_eq!(extension_deps.len(), 2);

        // Check avocado-dev extension dependencies
        let avocado_dev_deps = extension_deps.get("avocado-dev").unwrap();
        assert_eq!(avocado_dev_deps.len(), 2);
        assert_eq!(
            avocado_dev_deps
                .get("nativesdk-avocado-hitl")
                .unwrap()
                .as_str(),
            Some("*")
        );
        assert_eq!(
            avocado_dev_deps
                .get("nativesdk-something-else")
                .unwrap()
                .as_str(),
            Some("1.2.3")
        );

        // Check another-ext extension dependencies
        let another_ext_deps = extension_deps.get("another-ext").unwrap();
        assert_eq!(another_ext_deps.len(), 1);
        assert_eq!(
            another_ext_deps.get("nativesdk-tool").unwrap().as_str(),
            Some("*")
        );
    }

    #[test]
    fn test_invalid_toml() {
        let invalid_content = "invalid toml content [[[";
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{invalid_content}").unwrap();

        let result = Config::load(temp_file.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_sdk_container_args() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
container_args = ["--network=$USER-avocado", "--privileged"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test getting container args
        let container_args = config.get_sdk_container_args();
        assert!(container_args.is_some());
        let args = container_args.unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--network=$USER-avocado");
        assert_eq!(args[1], "--privileged");
    }

    #[test]
    fn test_merge_sdk_container_args() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
container_args = ["--network=host", "--privileged"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test merging with CLI args
        let cli_args = vec!["--cap-add=SYS_ADMIN".to_string(), "--rm".to_string()];
        let merged = config.merge_sdk_container_args(Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 4);
        assert_eq!(merged_args[0], "--network=host");
        assert_eq!(merged_args[1], "--privileged");
        assert_eq!(merged_args[2], "--cap-add=SYS_ADMIN");
        assert_eq!(merged_args[3], "--rm");
    }

    #[test]
    fn test_merge_sdk_container_args_config_only() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
container_args = ["--network=host"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with no CLI args
        let merged = config.merge_sdk_container_args(None);

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 1);
        assert_eq!(merged_args[0], "--network=host");
    }

    #[test]
    fn test_merge_sdk_container_args_cli_only() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with only CLI args
        let cli_args = vec!["--cap-add=SYS_ADMIN".to_string()];
        let merged = config.merge_sdk_container_args(Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 1);
        assert_eq!(merged_args[0], "--cap-add=SYS_ADMIN");
    }

    #[test]
    fn test_merge_sdk_container_args_none() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with no config args and no CLI args
        let merged = config.merge_sdk_container_args(None);

        assert!(merged.is_none());
    }

    #[test]
    fn test_expand_env_vars() {
        // Set up test environment variables
        std::env::set_var("TEST_USER", "testuser");
        std::env::set_var("TEST_NETWORK", "mynetwork");

        // Test $VAR format
        let result = Config::expand_env_vars("--network=$TEST_USER-avocado");
        assert_eq!(result, "--network=testuser-avocado");

        // Test ${VAR} format
        let result = Config::expand_env_vars("--network=${TEST_NETWORK}");
        assert_eq!(result, "--network=mynetwork");

        // Test mixed formats
        let result = Config::expand_env_vars("--arg=$TEST_USER-${TEST_NETWORK}");
        assert_eq!(result, "--arg=testuser-mynetwork");

        // Test undefined variable (should become empty string)
        let result = Config::expand_env_vars("--network=$UNDEFINED_VAR-test");
        assert_eq!(result, "--network=-test");

        // Test no variables
        let result = Config::expand_env_vars("--privileged");
        assert_eq!(result, "--privileged");

        // Clean up
        std::env::remove_var("TEST_USER");
        std::env::remove_var("TEST_NETWORK");
    }

    #[test]
    fn test_merge_sdk_container_args_with_env_expansion() {
        // Set up test environment variable
        std::env::set_var("TEST_USER", "myuser");

        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"
container_args = ["--network=$TEST_USER-avocado", "--privileged"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test merging with environment variable expansion
        let cli_args = vec!["--cap-add=$TEST_USER".to_string()];
        let merged = config.merge_sdk_container_args(Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 3);
        assert_eq!(merged_args[0], "--network=myuser-avocado");
        assert_eq!(merged_args[1], "--privileged");
        assert_eq!(merged_args[2], "--cap-add=myuser");

        // Clean up
        std::env::remove_var("TEST_USER");
    }

    #[test]
    fn test_process_container_args() {
        // Set up test environment variable
        std::env::set_var("TEST_VAR", "testvalue");

        let args = vec![
            "--network=$TEST_VAR-net".to_string(),
            "--env=HOME=${HOME}".to_string(),
            "--privileged".to_string(),
        ];

        let processed = Config::process_container_args(Some(&args));

        assert!(processed.is_some());
        let processed_args = processed.unwrap();
        assert_eq!(processed_args.len(), 3);
        assert_eq!(processed_args[0], "--network=testvalue-net");
        assert!(processed_args[1].starts_with("--env=HOME="));
        assert_eq!(processed_args[2], "--privileged");

        // Test with None
        let no_args = Config::process_container_args(None);
        assert!(no_args.is_none());

        // Clean up
        std::env::remove_var("TEST_VAR");
    }
}
