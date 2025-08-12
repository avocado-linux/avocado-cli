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
sysext = true
confext = true

[ext.avocado-dev.sdk.dependencies]
nativesdk-avocado-hitl = "*"
nativesdk-something-else = "1.2.3"

[ext.another-ext]
sysext = true

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
}
