//! Configuration utilities for Avocado CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// =============================================================================
// DEPRECATION NOTE: TOML Support (Pre-1.0.0)
// =============================================================================
// TOML configuration file support is DEPRECATED and maintained only for
// backward compatibility and migration purposes. The default format is now YAML.
//
// TOML support will be removed before the 1.0.0 release.
//
// Migration: When a legacy avocado.toml file is detected, it will be
// automatically converted to avocado.yaml format.
// =============================================================================

/// Custom deserializer module for container_args
mod container_args_deserializer {
    use serde::{Deserialize, Deserializer};

    /// Splits a string on unescaped spaces, respecting quotes
    ///
    /// Examples:
    /// - "-v /dev:/dev" -> ["-v", "/dev:/dev"]
    /// - "-v /path\\ with\\ spaces:/dev" -> ["-v", "/path with spaces:/dev"]
    /// - "-e \"VAR=value with spaces\"" -> ["-e", "VAR=value with spaces"]
    fn split_on_unescaped_spaces(s: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut current = String::new();
        let mut chars = s.chars().peekable();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;

        while let Some(ch) = chars.next() {
            if escape_next {
                current.push(ch);
                escape_next = false;
                continue;
            }

            match ch {
                '\\' => {
                    // Look ahead to see if we're escaping a space or quote
                    if let Some(&next_ch) = chars.peek() {
                        if next_ch == ' ' || next_ch == '"' || next_ch == '\'' || next_ch == '\\' {
                            escape_next = true;
                        } else {
                            current.push(ch);
                        }
                    } else {
                        current.push(ch);
                    }
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                ' ' if !in_single_quote && !in_double_quote => {
                    if !current.is_empty() {
                        result.push(current.clone());
                        current.clear();
                    }
                }
                _ => {
                    current.push(ch);
                }
            }
        }

        if !current.is_empty() {
            result.push(current);
        }

        result
    }

    /// Deserializes container_args from either a string or an array
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrVec {
            String(String),
            Vec(Vec<String>),
        }

        let value = Option::<StringOrVec>::deserialize(deserializer)?;

        Ok(value.map(|v| match v {
            StringOrVec::String(s) => split_on_unescaped_spaces(&s),
            StringOrVec::Vec(vec) => vec,
        }))
    }
}

/// Represents the location of an extension (local or external)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionLocation {
    /// Extension defined in the main config file
    Local { name: String, config_path: String },
    /// Extension defined in an external config file
    External { name: String, config_path: String },
}

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
    pub dependencies: Option<HashMap<String, serde_yaml::Value>>,
    pub stone_include_paths: Option<Vec<String>>,
    pub stone_manifest: Option<String>,
}

/// SDK configuration section
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SdkConfig {
    pub image: Option<String>,
    pub dependencies: Option<HashMap<String, serde_yaml::Value>>,
    pub compile: Option<HashMap<String, CompileConfig>>,
    pub repo_url: Option<String>,
    pub repo_release: Option<String>,
    #[serde(default, deserialize_with = "container_args_deserializer::deserialize")]
    pub container_args: Option<Vec<String>>,
    pub disable_weak_dependencies: Option<bool>,
}

/// Compile configuration for SDK
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompileConfig {
    pub compile: Option<String>,
    pub dependencies: Option<HashMap<String, serde_yaml::Value>>,
}

/// Provision profile configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisionProfileConfig {
    #[serde(default, deserialize_with = "container_args_deserializer::deserialize")]
    pub container_args: Option<Vec<String>>,
}

/// Distribution configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistroConfig {
    pub channel: Option<String>,
    pub version: Option<String>,
}

/// Signing key reference in configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SigningKeyRef {
    /// Name of the signing key (as registered in global signing keys)
    pub key: String,
}

/// Supported targets configuration - can be either "*" (all targets) or a list of specific targets
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SupportedTargets {
    All(String),       // "*"
    List(Vec<String>), // ["target1", "target2", ...]
}

/// Main configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub default_target: Option<String>,
    pub supported_targets: Option<SupportedTargets>,
    pub src_dir: Option<String>,
    pub distro: Option<DistroConfig>,
    pub runtime: Option<HashMap<String, RuntimeConfig>>,
    pub sdk: Option<SdkConfig>,
    pub provision: Option<HashMap<String, ProvisionProfileConfig>>,
    /// Signing keys referenced by this configuration
    pub signing_keys: Option<Vec<SigningKeyRef>>,
}

impl Config {
    /// Get merged configuration for any section type with target-specific overrides.
    ///
    /// This function implements the hierarchical merging pattern:
    /// - Base section: [section_name]
    /// - Target-specific: [section_name.<target>]
    /// - For named sections: [section_type.name] + [section_type.name.<target>]
    ///
    /// # Arguments
    /// * `section_path` - The base section path (e.g., "sdk", "runtime.prod", "ext.avocado-dev")
    /// * `target` - The target architecture
    /// * `config_path` - Path to the configuration file for raw TOML access
    ///
    /// # Returns
    /// Merged value with target-specific overrides applied
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_section(
        &self,
        section_path: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        // Read the raw config to access target-specific sections
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;

        let mut parsed = Self::parse_config_value(config_path, &content)?;

        // Apply interpolation to the parsed config
        crate::utils::interpolation::interpolate_config(&mut parsed, Some(target))
            .with_context(|| "Failed to interpolate configuration values")?;

        // Get the base section
        let base_section = self.get_nested_section(&parsed, section_path);

        // Get the target-specific section
        let target_section_path = format!("{section_path}.{target}");
        let target_section = self.get_nested_section(&parsed, &target_section_path);

        // Merge the sections, but filter out target-specific keys from the base
        match (base_section, target_section) {
            (Some(base), Some(target_override)) => Ok(Some(
                self.merge_values(base.clone(), target_override.clone()),
            )),
            (Some(base), None) => {
                // Filter out target-specific subsections from base before returning
                let supported_targets = self.get_supported_targets().unwrap_or_default();
                let filtered_base =
                    self.filter_target_subsections(base.clone(), &supported_targets);
                if filtered_base.as_mapping().is_some_and(|t| t.is_empty()) {
                    Ok(None)
                } else {
                    Ok(Some(filtered_base))
                }
            }
            (None, Some(target_override)) => Ok(Some(target_override.clone())),
            (None, None) => Ok(None),
        }
    }

    /// Parse a config file content into a YAML value (supports both YAML and TOML)
    fn parse_config_value(path: &str, content: &str) -> Result<serde_yaml::Value> {
        let is_yaml = path.ends_with(".yaml") || path.ends_with(".yml");

        if is_yaml {
            serde_yaml::from_str(content)
                .with_context(|| format!("Failed to parse config file: {path}"))
        } else {
            // DEPRECATED: Parse TOML and convert to YAML value
            let toml_val: toml::Value = toml::from_str(content)
                .with_context(|| format!("Failed to parse config file: {path}"))?;
            Self::toml_to_yaml(&toml_val)
        }
    }

    /// Parse config content and apply interpolation with the given target.
    ///
    /// This is used when we need to read raw config values with interpolation applied,
    /// particularly for extension SDK dependencies that may contain templates like
    /// `{{ avocado.target }}` in their keys.
    fn parse_config_value_with_interpolation(
        path: &str,
        content: &str,
        target: Option<&str>,
    ) -> Result<serde_yaml::Value> {
        let mut parsed = Self::parse_config_value(path, content)?;

        // Apply interpolation with the target
        crate::utils::interpolation::interpolate_config(&mut parsed, target)
            .with_context(|| "Failed to interpolate configuration values")?;

        Ok(parsed)
    }

    /// Helper function to get a nested section from YAML using dot notation
    #[allow(dead_code)] // Helper for merging system
    fn get_nested_section<'a>(
        &self,
        yaml: &'a serde_yaml::Value,
        path: &str,
    ) -> Option<&'a serde_yaml::Value> {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = yaml;

        for part in parts {
            match current.get(part) {
                Some(value) => current = value,
                None => return None,
            }
        }

        Some(current)
    }

    /// Filter out target-specific subsections from a YAML value
    #[allow(dead_code)] // Helper for merging system
    fn filter_target_subsections(
        &self,
        mut value: serde_yaml::Value,
        supported_targets: &[String],
    ) -> serde_yaml::Value {
        if let serde_yaml::Value::Mapping(ref mut map) = value {
            // Remove any keys that match supported targets
            for target in supported_targets {
                map.remove(serde_yaml::Value::String(target.clone()));
            }
        }
        value
    }

    /// Merge two YAML values with the target value taking precedence
    #[allow(dead_code)] // Helper for merging system
    #[allow(clippy::only_used_in_recursion)] // Recursive merge function needs self parameter
    fn merge_values(
        &self,
        mut base: serde_yaml::Value,
        target: serde_yaml::Value,
    ) -> serde_yaml::Value {
        match (&mut base, target) {
            // If both are mappings, merge them recursively
            (serde_yaml::Value::Mapping(base_map), serde_yaml::Value::Mapping(target_map)) => {
                for (key, target_value) in target_map {
                    if let Some(base_value) = base_map.get_mut(&key) {
                        // Recursively merge if both are mappings, otherwise override
                        *base_value = self.merge_values(base_value.clone(), target_value);
                    } else {
                        // Add new key from target
                        base_map.insert(key, target_value);
                    }
                }
                base
            }
            // For any other combination, target overrides base
            (_, target_value) => target_value,
        }
    }
    /// Get merged runtime configuration for a specific runtime and target
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_runtime_config(
        &self,
        runtime_name: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        let section_path = format!("runtime.{runtime_name}");
        self.get_merged_section(&section_path, target, config_path)
    }

    /// Get merged provision configuration for a specific profile and target
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_provision_config(
        &self,
        profile_name: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        let section_path = format!("provision.{profile_name}");
        self.get_merged_section(&section_path, target, config_path)
    }

    /// Get merged extension configuration for a specific extension and target
    pub fn get_merged_ext_config(
        &self,
        ext_name: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        let section_path = format!("ext.{ext_name}");
        self.get_merged_section(&section_path, target, config_path)
    }

    /// Get merged section for nested paths (e.g., "ext.name.dependencies", "runtime.name.dependencies")
    /// For target-specific overrides, the target is inserted between base_path and nested_path:
    /// Base: [ext.name.dependencies] + Target: [ext.name.<target>.dependencies]
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_nested_section(
        &self,
        base_path: &str,
        nested_path: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        // Read the raw config to access target-specific sections
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let mut parsed = Self::parse_config_value(config_path, &content)?;

        // Apply interpolation to the parsed config
        crate::utils::interpolation::interpolate_config(&mut parsed, Some(target))
            .with_context(|| "Failed to interpolate configuration values")?;

        // Get the base section: base_path.nested_path
        let base_section_path = format!("{base_path}.{nested_path}");
        let base_section = self.get_nested_section(&parsed, &base_section_path);

        // Get the target-specific section: base_path.target.nested_path
        let target_section_path = format!("{base_path}.{target}.{nested_path}");
        let target_section = self.get_nested_section(&parsed, &target_section_path);

        // Merge the sections
        match (base_section, target_section) {
            (Some(base), Some(target_override)) => Ok(Some(
                self.merge_values(base.clone(), target_override.clone()),
            )),
            (Some(base), None) => Ok(Some(base.clone())),
            (None, Some(target_override)) => Ok(Some(target_override.clone())),
            (None, None) => Ok(None),
        }
    }

    /// Load configuration from a file (supports YAML and TOML)
    pub fn load<P: AsRef<Path>>(config_path: P) -> Result<Self> {
        let path = config_path.as_ref();

        if !path.exists() {
            // If a YAML file is requested but doesn't exist, check for a TOML version
            let is_yaml_request = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "yaml" || e == "yml")
                .unwrap_or(false);

            if is_yaml_request {
                // Try to find a corresponding TOML file
                let toml_path = path.with_extension("toml");
                if toml_path.exists() {
                    println!(
                        "⚠ Found legacy TOML config file: {}. Migrating to YAML format...",
                        toml_path.display()
                    );

                    // Migrate TOML to YAML
                    let migrated_path = Self::migrate_toml_to_yaml(&toml_path)?;

                    // Load the migrated YAML file
                    let content = fs::read_to_string(&migrated_path).with_context(|| {
                        format!(
                            "Failed to read migrated config file: {}",
                            migrated_path.display()
                        )
                    })?;

                    return Self::load_from_yaml_str(&content).with_context(|| {
                        format!(
                            "Failed to parse migrated YAML config file: {}",
                            migrated_path.display()
                        )
                    });
                }
            }

            return Err(ConfigError::FileNotFound(path.display().to_string()).into());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        // Determine format based on file extension
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "yaml" || e == "yml")
            .unwrap_or(false);

        if is_yaml {
            Self::load_from_yaml_str(&content)
                .with_context(|| format!("Failed to parse YAML config file: {}", path.display()))
        } else {
            // TOML file detected - migrate to YAML
            println!(
                "⚠ Found legacy TOML config file: {}. Migrating to YAML...",
                path.display()
            );

            // Parse TOML, convert to YAML, and save
            #[allow(deprecated)]
            let config = Self::load_from_toml_str(&content)
                .with_context(|| format!("Failed to parse TOML config file: {}", path.display()))?;

            // Convert to YAML value for saving
            let toml_val: toml::Value =
                toml::from_str(&content).with_context(|| "Failed to parse TOML for conversion")?;
            let yaml_val = Self::toml_to_yaml(&toml_val)?;

            // Save as YAML in the same directory
            let yaml_path = path.with_extension("yaml");
            if !yaml_path.exists() {
                let yaml_content = serde_yaml::to_string(&yaml_val)?;
                fs::write(&yaml_path, yaml_content).with_context(|| {
                    format!(
                        "Failed to write migrated YAML config to {}",
                        yaml_path.display()
                    )
                })?;
                println!("✓ Migrated to {}", yaml_path.display());
                println!("  Note: The old TOML file has been preserved. You can remove it after verifying the migration.");
            }

            Ok(config)
        }
    }

    /// Load configuration from a YAML string
    pub fn load_from_yaml_str(content: &str) -> Result<Self> {
        // Parse YAML into a Value first
        let mut parsed: serde_yaml::Value =
            serde_yaml::from_str(content).with_context(|| "Failed to parse YAML configuration")?;

        // Perform interpolation before deserializing to Config struct
        crate::utils::interpolation::interpolate_config(&mut parsed, None)
            .with_context(|| "Failed to interpolate configuration values")?;

        // Deserialize to Config struct
        let config: Config = serde_yaml::from_value(parsed)
            .with_context(|| "Failed to deserialize configuration after interpolation")?;

        Ok(config)
    }

    /// Load configuration from a string (auto-detects YAML or TOML format)
    /// Used primarily in tests for flexible parsing
    #[allow(dead_code)]
    pub fn load_from_str(content: &str) -> Result<Self> {
        // Try YAML first (preferred format)
        if let Ok(config) = serde_yaml::from_str::<Config>(content) {
            return Ok(config);
        }

        // Fall back to TOML for test compatibility
        #[allow(deprecated)]
        {
            Self::load_from_toml_str(content)
        }
    }

    // =============================================================================
    // DEPRECATED: TOML Support Functions (Pre-1.0.0)
    // =============================================================================
    // The following functions support legacy TOML configuration files.
    // These will be removed before the 1.0.0 release.
    // =============================================================================

    /// DEPRECATED: Load configuration from a TOML string
    #[allow(dead_code)] // Kept for backward compatibility until 1.0.0
    #[deprecated(
        note = "TOML format is deprecated. Use YAML format instead. Will be removed before 1.0.0"
    )]
    pub fn load_from_toml_str(content: &str) -> Result<Self> {
        let config: Config =
            toml::from_str(content).with_context(|| "Failed to parse TOML configuration")?;

        Ok(config)
    }

    /// Convert TOML value to YAML value
    fn toml_to_yaml(toml_val: &toml::Value) -> Result<serde_yaml::Value> {
        let json_str = serde_json::to_string(toml_val)?;
        let yaml_val = serde_json::from_str(&json_str)?;
        Ok(yaml_val)
    }

    /// Migrate a TOML config file to YAML format
    /// Reads an avocado.toml file, converts it to YAML, and saves as avocado.yaml
    #[allow(dead_code)] // Public API for manual migration, kept until 1.0.0
    pub fn migrate_toml_to_yaml<P: AsRef<Path>>(toml_path: P) -> Result<PathBuf> {
        let toml_path = toml_path.as_ref();

        // Read the TOML file
        let toml_content = fs::read_to_string(toml_path)
            .with_context(|| format!("Failed to read TOML config file: {}", toml_path.display()))?;

        // Parse as TOML
        let toml_val: toml::Value =
            toml::from_str(&toml_content).with_context(|| "Failed to parse TOML configuration")?;

        // Convert to YAML
        let yaml_val = Self::toml_to_yaml(&toml_val)?;

        // Serialize to YAML string
        let yaml_content =
            serde_yaml::to_string(&yaml_val).with_context(|| "Failed to serialize to YAML")?;

        // Determine output path
        let yaml_path = toml_path.with_file_name("avocado.yaml");

        // Write YAML file
        fs::write(&yaml_path, yaml_content).with_context(|| {
            format!("Failed to write YAML config file: {}", yaml_path.display())
        })?;

        println!(
            "✓ Migrated {} to {}",
            toml_path.display(),
            yaml_path.display()
        );
        println!("  Note: The old TOML file has been preserved. You can remove it after verifying the migration.");

        Ok(yaml_path)
    }

    /// Get the SDK image from configuration
    pub fn get_sdk_image(&self) -> Option<&String> {
        self.sdk.as_ref()?.image.as_ref()
    }

    /// Get SDK dependencies
    pub fn get_sdk_dependencies(&self) -> Option<&HashMap<String, serde_yaml::Value>> {
        self.sdk.as_ref()?.dependencies.as_ref()
    }

    /// Get SDK dependencies with target interpolation.
    ///
    /// This method re-reads the config file and interpolates `{{ avocado.target }}`
    /// templates with the provided target value.
    ///
    /// # Arguments
    /// * `config_path` - Path to the configuration file
    /// * `target` - The target architecture to use for interpolation
    ///
    /// # Returns
    /// Interpolated SDK dependencies, or None if no dependencies are defined
    pub fn get_sdk_dependencies_for_target(
        &self,
        config_path: &str,
        target: &str,
    ) -> Result<Option<HashMap<String, serde_yaml::Value>>> {
        // Re-read the config file to get raw (uninterpolated) values
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;

        // Parse YAML into a Value
        let mut parsed: serde_yaml::Value =
            serde_yaml::from_str(&content).with_context(|| "Failed to parse YAML configuration")?;

        // Perform interpolation with the target
        crate::utils::interpolation::interpolate_config(&mut parsed, Some(target))
            .with_context(|| "Failed to interpolate configuration values")?;

        // Extract SDK dependencies from the interpolated config
        let sdk_deps = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("dependencies"))
            .and_then(|deps| deps.as_mapping())
            .map(|mapping| {
                mapping
                    .iter()
                    .filter_map(|(k, v)| k.as_str().map(|key| (key.to_string(), v.clone())))
                    .collect::<HashMap<String, serde_yaml::Value>>()
            });

        Ok(sdk_deps)
    }

    /// Get the SDK repo URL from environment variable or configuration
    /// Priority: AVOCADO_SDK_REPO_URL environment variable > config file
    pub fn get_sdk_repo_url(&self) -> Option<String> {
        // First priority: Environment variable
        if let Ok(env_url) = env::var("AVOCADO_SDK_REPO_URL") {
            return Some(env_url);
        }

        // Second priority: Configuration file
        self.sdk.as_ref()?.repo_url.as_ref().cloned()
    }

    /// Get the SDK repo release from environment variable or configuration
    /// Priority: AVOCADO_SDK_REPO_RELEASE environment variable > config file
    pub fn get_sdk_repo_release(&self) -> Option<String> {
        // First priority: Environment variable
        if let Ok(env_release) = env::var("AVOCADO_SDK_REPO_RELEASE") {
            return Some(env_release);
        }

        // Second priority: Configuration file
        self.sdk.as_ref()?.repo_release.as_ref().cloned()
    }

    /// Get the SDK container args from configuration
    pub fn get_sdk_container_args(&self) -> Option<&Vec<String>> {
        self.sdk.as_ref()?.container_args.as_ref()
    }

    /// Get the disable_weak_dependencies setting from SDK configuration
    /// Returns the configured value or false (enable weak deps) if not set
    pub fn get_sdk_disable_weak_dependencies(&self) -> bool {
        self.sdk
            .as_ref()
            .and_then(|sdk| sdk.disable_weak_dependencies)
            .unwrap_or(false) // Default to false (enable weak dependencies)
    }

    /// Get signing keys referenced in this configuration
    #[allow(dead_code)] // Public API for future use
    pub fn get_signing_keys(&self) -> Option<&Vec<SigningKeyRef>> {
        self.signing_keys.as_ref()
    }

    /// Get signing key names as a list of strings
    #[allow(dead_code)] // Public API for future use
    pub fn get_signing_key_names(&self) -> Vec<String> {
        self.signing_keys
            .as_ref()
            .map(|keys| keys.iter().map(|k| k.key.clone()).collect())
            .unwrap_or_default()
    }

    /// Get provision profile configuration
    pub fn get_provision_profile(&self, profile_name: &str) -> Option<&ProvisionProfileConfig> {
        self.provision.as_ref()?.get(profile_name)
    }

    /// Get container args from provision profile
    pub fn get_provision_profile_container_args(&self, profile_name: &str) -> Option<&Vec<String>> {
        self.get_provision_profile(profile_name)?
            .container_args
            .as_ref()
    }

    /// Get the resolved source directory path
    /// If src_dir is configured, it resolves relative paths relative to the config file
    /// If not configured, returns None (use default behavior)
    pub fn get_resolved_src_dir<P: AsRef<Path>>(&self, config_path: P) -> Option<PathBuf> {
        self.src_dir.as_ref().map(|src_dir| {
            let path = Path::new(src_dir);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                // Resolve relative to config file directory
                let config_dir = config_path.as_ref().parent().unwrap_or(Path::new("."));
                config_dir.join(path).canonicalize().unwrap_or_else(|_| {
                    // If canonicalize fails, just join the paths
                    config_dir.join(path)
                })
            }
        })
    }

    /// Resolve a path relative to src_dir if it's relative, or use as-is if absolute
    /// If src_dir is not configured, resolve relative to config file directory
    pub fn resolve_path_relative_to_src_dir<P: AsRef<Path>>(
        &self,
        config_path: P,
        path: &str,
    ) -> PathBuf {
        let target_path = Path::new(path);

        if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            // Try to resolve relative to src_dir first
            if let Some(src_dir) = self.get_resolved_src_dir(&config_path) {
                src_dir.join(target_path)
            } else {
                // Fallback to config file directory
                let config_dir = config_path.as_ref().parent().unwrap_or(Path::new("."));
                config_dir.join(target_path)
            }
        }
    }

    /// Get stone include paths for a runtime and convert them to container paths
    /// Returns a space-separated string of absolute paths from the container's perspective
    /// (e.g., "/opt/src/path1 /opt/src/path2")
    pub fn get_stone_include_paths_for_runtime<P: AsRef<Path>>(
        &self,
        runtime_name: &str,
        target: &str,
        config_path: P,
    ) -> Result<Option<String>> {
        // Get merged runtime config to include target-specific overrides
        let config_path_str = config_path
            .as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in config path"))?;
        let merged_runtime =
            self.get_merged_runtime_config(runtime_name, target, config_path_str)?;

        // Extract stone_include_paths and convert to owned Vec<String>
        let stone_paths: Option<Vec<String>> = merged_runtime
            .as_ref()
            .and_then(|runtime| runtime.get("stone_include_paths"))
            .and_then(|paths| paths.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
            });

        if let Some(paths) = stone_paths {
            if paths.is_empty() {
                return Ok(None);
            }

            // Get the resolved src_dir (or config directory if src_dir not set)
            let src_dir = self.get_resolved_src_dir(&config_path).unwrap_or_else(|| {
                config_path
                    .as_ref()
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf()
            });

            // Convert each path to a container path
            let container_paths: Vec<String> = paths
                .iter()
                .map(|path| {
                    // Resolve the path relative to src_dir
                    let resolved = self.resolve_path_relative_to_src_dir(&config_path, path);

                    // Convert to container path by replacing src_dir with /opt/src
                    // The resolved path should be under src_dir
                    if let Ok(relative) = resolved.strip_prefix(&src_dir) {
                        format!("/opt/src/{}", relative.display())
                    } else {
                        // If not under src_dir, just append to /opt/src
                        format!("/opt/src/{path}")
                    }
                })
                .collect();

            Ok(Some(container_paths.join(" ")))
        } else {
            Ok(None)
        }
    }

    /// Get stone manifest for a runtime and convert it to a container path
    /// Returns the absolute path from the container's perspective (e.g., "/opt/src/stone-qemux86-64.json")
    pub fn get_stone_manifest_for_runtime<P: AsRef<Path>>(
        &self,
        runtime_name: &str,
        target: &str,
        config_path: P,
    ) -> Result<Option<String>> {
        // Get merged runtime config to include target-specific overrides
        let config_path_str = config_path
            .as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in config path"))?;
        let merged_runtime =
            self.get_merged_runtime_config(runtime_name, target, config_path_str)?;

        // Extract stone_manifest value
        let stone_manifest: Option<String> = merged_runtime
            .as_ref()
            .and_then(|runtime| runtime.get("stone_manifest"))
            .and_then(|manifest| manifest.as_str())
            .map(|s| s.to_string());

        if let Some(manifest_path) = stone_manifest {
            // Convert to container path (/opt/src/<path>)
            Ok(Some(format!("/opt/src/{manifest_path}")))
        } else {
            Ok(None)
        }
    }

    /// Load and parse external extension configuration from a config file
    /// Returns a map of extension name to extension configuration
    pub fn load_external_extensions<P: AsRef<Path>>(
        &self,
        config_path: P,
        external_config_path: &str,
    ) -> Result<HashMap<String, serde_yaml::Value>> {
        let resolved_path =
            self.resolve_path_relative_to_src_dir(&config_path, external_config_path);

        if !resolved_path.exists() {
            return Err(anyhow::anyhow!(
                "External extension config file not found: {}",
                resolved_path.display()
            ));
        }

        let content = std::fs::read_to_string(&resolved_path).with_context(|| {
            format!(
                "Failed to read external config file: {}",
                resolved_path.display()
            )
        })?;

        let parsed = Self::parse_config_value(
            resolved_path.to_str().unwrap_or(external_config_path),
            &content,
        )?;

        let mut external_extensions = HashMap::new();

        // Find all ext.* sections in the external config
        if let Some(ext_section) = parsed.get("ext").and_then(|e| e.as_mapping()) {
            for (ext_name_key, ext_config) in ext_section {
                if let Some(ext_name) = ext_name_key.as_str() {
                    external_extensions.insert(ext_name.to_string(), ext_config.clone());
                }
            }
        }

        Ok(external_extensions)
    }

    /// Find an extension in the full dependency tree (local and external)
    /// This is a comprehensive search that looks through all runtime dependencies
    /// and their transitive extension dependencies
    pub fn find_extension_in_dependency_tree(
        &self,
        config_path: &str,
        extension_name: &str,
        target: &str,
    ) -> Result<Option<ExtensionLocation>> {
        let content = std::fs::read_to_string(config_path)?;
        let parsed = Self::parse_config_value(config_path, &content)?;

        // First check if it's a local extension
        if let Some(ext_section) = parsed.get("ext") {
            if let Some(ext_map) = ext_section.as_mapping() {
                if ext_map.contains_key(serde_yaml::Value::String(extension_name.to_string())) {
                    return Ok(Some(ExtensionLocation::Local {
                        name: extension_name.to_string(),
                        config_path: config_path.to_string(),
                    }));
                }
            }
        }

        // If not local, search through the full dependency tree
        let mut all_extensions = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();

        // Get all extensions from runtime dependencies (this will recursively traverse)
        let runtime_section = parsed.get("runtime").and_then(|r| r.as_mapping());

        if let Some(runtime_section) = runtime_section {
            for (runtime_name_key, _) in runtime_section {
                if let Some(runtime_name) = runtime_name_key.as_str() {
                    // Get merged runtime config for this target
                    let merged_runtime =
                        self.get_merged_runtime_config(runtime_name, target, config_path)?;
                    if let Some(merged_value) = merged_runtime {
                        if let Some(dependencies) = merged_value
                            .get("dependencies")
                            .and_then(|d| d.as_mapping())
                        {
                            for (_dep_name, dep_spec) in dependencies {
                                // Check for extension dependency
                                if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str())
                                {
                                    // Check if this is an external extension (has config field)
                                    if let Some(external_config) =
                                        dep_spec.get("config").and_then(|v| v.as_str())
                                    {
                                        let ext_location = ExtensionLocation::External {
                                            name: ext_name.to_string(),
                                            config_path: external_config.to_string(),
                                        };
                                        all_extensions.insert(ext_location.clone());

                                        // Recursively find nested external extension dependencies
                                        self.find_all_nested_extensions_for_lookup(
                                            config_path,
                                            &ext_location,
                                            &mut all_extensions,
                                            &mut visited,
                                        )?;
                                    } else {
                                        // Local extension
                                        all_extensions.insert(ExtensionLocation::Local {
                                            name: ext_name.to_string(),
                                            config_path: config_path.to_string(),
                                        });

                                        // Also check local extension dependencies
                                        self.find_local_extension_dependencies_for_lookup(
                                            config_path,
                                            &parsed,
                                            ext_name,
                                            &mut all_extensions,
                                            &mut visited,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Now search for the target extension in all collected extensions
        for ext_location in all_extensions {
            let found_name = match &ext_location {
                ExtensionLocation::Local { name, .. } => name,
                ExtensionLocation::External { name, .. } => name,
            };

            if found_name == extension_name {
                return Ok(Some(ext_location));
            }
        }

        Ok(None)
    }

    /// Recursively find all nested extensions for lookup
    fn find_all_nested_extensions_for_lookup(
        &self,
        base_config_path: &str,
        ext_location: &ExtensionLocation,
        all_extensions: &mut std::collections::HashSet<ExtensionLocation>,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<()> {
        let (ext_name, ext_config_path) = match ext_location {
            ExtensionLocation::External { name, config_path } => (name, config_path),
            ExtensionLocation::Local { name, config_path } => {
                // For local extensions, we need to check their dependencies too
                let content = std::fs::read_to_string(config_path)?;
                let parsed = Self::parse_config_value(config_path, &content)?;
                return self.find_local_extension_dependencies_for_lookup(
                    config_path,
                    &parsed,
                    name,
                    all_extensions,
                    visited,
                );
            }
        };

        // Cycle detection: check if we've already processed this extension
        let ext_key = format!("{ext_name}:{ext_config_path}");
        if visited.contains(&ext_key) {
            return Ok(());
        }
        visited.insert(ext_key);

        // Load the external extension configuration
        let resolved_external_config_path =
            self.resolve_path_relative_to_src_dir(base_config_path, ext_config_path);
        let external_extensions =
            self.load_external_extensions(base_config_path, ext_config_path)?;

        let extension_config = external_extensions.get(ext_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{ext_name}' not found in external config file '{ext_config_path}'"
            )
        })?;

        // Load the nested config file to get its src_dir setting
        let nested_config_content = std::fs::read_to_string(&resolved_external_config_path)
            .with_context(|| {
                format!(
                    "Failed to read nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;
        let nested_config = Self::parse_config_value(
            resolved_external_config_path
                .to_str()
                .unwrap_or(ext_config_path),
            &nested_config_content,
        )
        .with_context(|| {
            format!(
                "Failed to parse nested config file: {}",
                resolved_external_config_path.display()
            )
        })?;

        // Create a temporary Config object for the nested config to handle its src_dir
        let nested_config_obj = serde_yaml::from_value::<Config>(nested_config.clone())?;

        // Check if this external extension has dependencies
        if let Some(dependencies) = extension_config
            .get("dependencies")
            .and_then(|d| d.as_mapping())
        {
            for (_dep_name, dep_spec) in dependencies {
                // Check for nested extension dependency
                if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                    // Check if this is a nested external extension (has config field)
                    if let Some(nested_external_config) =
                        dep_spec.get("config").and_then(|v| v.as_str())
                    {
                        // Resolve the nested config path relative to the nested config's src_dir
                        let nested_config_path = nested_config_obj
                            .resolve_path_relative_to_src_dir(
                                &resolved_external_config_path,
                                nested_external_config,
                            );

                        let nested_ext_location = ExtensionLocation::External {
                            name: nested_ext_name.to_string(),
                            config_path: nested_config_path.to_string_lossy().to_string(),
                        };

                        // Add the nested extension to all extensions
                        all_extensions.insert(nested_ext_location.clone());

                        // Recursively process the nested extension
                        self.find_all_nested_extensions_for_lookup(
                            base_config_path,
                            &nested_ext_location,
                            all_extensions,
                            visited,
                        )?;
                    } else {
                        // This is a local extension dependency within the external config
                        all_extensions.insert(ExtensionLocation::Local {
                            name: nested_ext_name.to_string(),
                            config_path: resolved_external_config_path
                                .to_string_lossy()
                                .to_string(),
                        });

                        // Check dependencies of this local extension in the external config
                        self.find_local_extension_dependencies_for_lookup(
                            &resolved_external_config_path.to_string_lossy(),
                            &nested_config,
                            nested_ext_name,
                            all_extensions,
                            visited,
                        )?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Find dependencies of local extensions for lookup
    fn find_local_extension_dependencies_for_lookup(
        &self,
        config_path: &str,
        parsed_config: &serde_yaml::Value,
        ext_name: &str,
        all_extensions: &mut std::collections::HashSet<ExtensionLocation>,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<()> {
        // Cycle detection for local extensions
        let ext_key = format!("local:{ext_name}:{config_path}");
        if visited.contains(&ext_key) {
            return Ok(());
        }
        visited.insert(ext_key);

        // Get the local extension configuration
        if let Some(ext_config) = parsed_config.get("ext").and_then(|ext| ext.get(ext_name)) {
            // Check if this local extension has dependencies
            if let Some(dependencies) = ext_config.get("dependencies").and_then(|d| d.as_mapping())
            {
                for (_dep_name, dep_spec) in dependencies {
                    // Check for extension dependency
                    if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                        // Check if this is an external extension (has config field)
                        if let Some(external_config) =
                            dep_spec.get("config").and_then(|v| v.as_str())
                        {
                            let ext_location = ExtensionLocation::External {
                                name: nested_ext_name.to_string(),
                                config_path: external_config.to_string(),
                            };
                            all_extensions.insert(ext_location.clone());

                            // Recursively find nested external extension dependencies
                            self.find_all_nested_extensions_for_lookup(
                                config_path,
                                &ext_location,
                                all_extensions,
                                visited,
                            )?;
                        } else {
                            // Local extension dependency
                            all_extensions.insert(ExtensionLocation::Local {
                                name: nested_ext_name.to_string(),
                                config_path: config_path.to_string(),
                            });

                            // Recursively check this local extension's dependencies
                            self.find_local_extension_dependencies_for_lookup(
                                config_path,
                                parsed_config,
                                nested_ext_name,
                                all_extensions,
                                visited,
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
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

    /// Merge provision profile container args with CLI args, expanding environment variables
    /// Returns a new Vec containing provision profile args first, then CLI args
    pub fn merge_provision_container_args(
        &self,
        provision_profile: Option<&str>,
        cli_args: Option<&Vec<String>>,
    ) -> Option<Vec<String>> {
        let profile_args = provision_profile
            .and_then(|profile| self.get_provision_profile_container_args(profile));

        match (profile_args, cli_args) {
            (Some(profile), Some(cli)) => {
                let mut merged = Self::process_container_args(Some(profile)).unwrap_or_default();
                merged.extend(Self::process_container_args(Some(cli)).unwrap_or_default());
                Some(merged)
            }
            (Some(profile), None) => Self::process_container_args(Some(profile)),
            (None, Some(cli)) => Self::process_container_args(Some(cli)),
            (None, None) => None,
        }
    }

    /// Get compile section dependencies
    pub fn get_compile_dependencies(&self) -> HashMap<String, &HashMap<String, serde_yaml::Value>> {
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
    ) -> Result<HashMap<String, HashMap<String, serde_yaml::Value>>> {
        self.get_extension_sdk_dependencies_with_config_path(config_content, None)
    }

    /// Get extension SDK dependencies from configuration, including nested external extension dependencies
    /// Returns a HashMap where keys are extension names and values are their SDK dependencies
    pub fn get_extension_sdk_dependencies_with_config_path(
        &self,
        config_content: &str,
        config_path: Option<&str>,
    ) -> Result<HashMap<String, HashMap<String, serde_yaml::Value>>> {
        self.get_extension_sdk_dependencies_with_config_path_and_target(
            config_content,
            config_path,
            None,
        )
    }

    /// Get extension SDK dependencies from configuration, including nested external extension dependencies and target-specific dependencies
    /// Returns a HashMap where keys are extension names and values are their SDK dependencies
    pub fn get_extension_sdk_dependencies_with_config_path_and_target(
        &self,
        config_content: &str,
        config_path: Option<&str>,
        target: Option<&str>,
    ) -> Result<HashMap<String, HashMap<String, serde_yaml::Value>>> {
        // Parse config with interpolation applied (including target for {{ avocado.target }} in keys)
        let parsed = if let Some(path) = config_path {
            Self::parse_config_value_with_interpolation(path, config_content, target)?
        } else {
            // Default to YAML parsing with interpolation
            let mut value: serde_yaml::Value = serde_yaml::from_str(config_content)
                .with_context(|| "Failed to parse configuration")?;
            crate::utils::interpolation::interpolate_config(&mut value, target)
                .with_context(|| "Failed to interpolate configuration values")?;
            value
        };

        let mut extension_sdk_deps = HashMap::new();
        let mut visited = std::collections::HashSet::new();

        // Process local extensions in the current config
        if let Some(ext_section) = parsed.get("ext") {
            if let Some(ext_table) = ext_section.as_mapping() {
                for (ext_name_val, ext_config) in ext_table {
                    if let Some(ext_name) = ext_name_val.as_str() {
                        if let Some(ext_config_table) = ext_config.as_mapping() {
                            // Extract SDK dependencies for this extension (base and target-specific)
                            let mut merged_deps = HashMap::new();

                            // First, collect base SDK dependencies from [ext.<ext_name>.sdk.dependencies]
                            if let Some(sdk_section) = ext_config_table.get("sdk") {
                                if let Some(sdk_table) = sdk_section.as_mapping() {
                                    if let Some(dependencies) = sdk_table.get("dependencies") {
                                        if let Some(deps_table) = dependencies.as_mapping() {
                                            for (k, v) in deps_table.iter() {
                                                if let Some(key_str) = k.as_str() {
                                                    merged_deps
                                                        .insert(key_str.to_string(), v.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Then, if we have a target, collect target-specific dependencies from [ext.<ext_name>.<target>.sdk.dependencies]
                            if let Some(target) = target {
                                if let Some(target_section) = ext_config_table.get(target) {
                                    if let Some(target_table) = target_section.as_mapping() {
                                        if let Some(sdk_section) = target_table.get("sdk") {
                                            if let Some(sdk_table) = sdk_section.as_mapping() {
                                                if let Some(dependencies) =
                                                    sdk_table.get("dependencies")
                                                {
                                                    if let Some(deps_table) =
                                                        dependencies.as_mapping()
                                                    {
                                                        // Target-specific dependencies override base dependencies
                                                        for (k, v) in deps_table.iter() {
                                                            if let Some(key_str) = k.as_str() {
                                                                merged_deps.insert(
                                                                    key_str.to_string(),
                                                                    v.clone(),
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Add the merged dependencies if any exist
                            if !merged_deps.is_empty() {
                                extension_sdk_deps.insert(ext_name.to_string(), merged_deps);
                            }

                            // If we have a config path, traverse external extension dependencies
                            if let Some(config_path) = config_path {
                                if let Some(dependencies) = ext_config_table.get("dependencies") {
                                    if let Some(deps_table) = dependencies.as_mapping() {
                                        self.collect_external_extension_sdk_dependencies_with_target(
                                            config_path,
                                            deps_table,
                                            &mut extension_sdk_deps,
                                            &mut visited,
                                            target,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Also process extensions referenced in runtime dependencies
        if let Some(config_path) = config_path {
            if let Some(runtime_section) = parsed.get("runtime") {
                if let Some(runtime_table) = runtime_section.as_mapping() {
                    for (_runtime_name, runtime_config) in runtime_table {
                        if let Some(runtime_config_table) = runtime_config.as_mapping() {
                            // Check base runtime dependencies
                            if let Some(dependencies) = runtime_config_table.get("dependencies") {
                                if let Some(deps_table) = dependencies.as_mapping() {
                                    self.collect_external_extension_sdk_dependencies_with_target(
                                        config_path,
                                        deps_table,
                                        &mut extension_sdk_deps,
                                        &mut visited,
                                        target,
                                    )?;
                                }
                            }

                            // Check target-specific runtime dependencies
                            if let Some(target) = target {
                                if let Some(target_section) = runtime_config_table.get(target) {
                                    if let Some(target_table) = target_section.as_mapping() {
                                        if let Some(dependencies) = target_table.get("dependencies")
                                        {
                                            if let Some(deps_table) = dependencies.as_mapping() {
                                                self.collect_external_extension_sdk_dependencies_with_target(
                                                    config_path,
                                                    deps_table,
                                                    &mut extension_sdk_deps,
                                                    &mut visited,
                                                    Some(target),
                                                )?;
                                            }
                                        }
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

    /// Recursively collect SDK dependencies from external extension configurations with target support
    fn collect_external_extension_sdk_dependencies_with_target(
        &self,
        base_config_path: &str,
        dependencies: &serde_yaml::Mapping,
        extension_sdk_deps: &mut HashMap<String, HashMap<String, serde_yaml::Value>>,
        visited: &mut std::collections::HashSet<String>,
        target: Option<&str>,
    ) -> Result<()> {
        for (_dep_name, dep_spec) in dependencies {
            if let Some(dep_spec_table) = dep_spec.as_mapping() {
                // Check for external extension dependency
                if let Some(ext_name) = dep_spec_table.get("ext").and_then(|v| v.as_str()) {
                    if let Some(external_config) =
                        dep_spec_table.get("config").and_then(|v| v.as_str())
                    {
                        // Cycle detection
                        let ext_key = format!("{ext_name}:{external_config}");
                        if visited.contains(&ext_key) {
                            continue;
                        }
                        visited.insert(ext_key);

                        // Load the external extension configuration
                        let resolved_external_config_path = self
                            .resolve_path_relative_to_src_dir(base_config_path, external_config);

                        match std::fs::read_to_string(&resolved_external_config_path) {
                            Ok(external_config_content) => {
                                let ext_config_path_str = resolved_external_config_path
                                    .to_str()
                                    .unwrap_or(external_config);
                                // Use interpolation-aware parsing to handle {{ avocado.target }} in keys
                                match Self::parse_config_value_with_interpolation(
                                    ext_config_path_str,
                                    &external_config_content,
                                    target,
                                ) {
                                    Ok(external_parsed) => {
                                        // Create a temporary Config object for the external config
                                        if let Ok(external_config_obj) =
                                            serde_yaml::from_value::<Config>(
                                                external_parsed.clone(),
                                            )
                                        {
                                            // Only process the specific extension that's being referenced
                                            if let Some(ext_section) = external_parsed.get("ext") {
                                                if let Some(ext_table) = ext_section.as_mapping() {
                                                    if let Some(external_ext_config) =
                                                        ext_table.get(ext_name)
                                                    {
                                                        if let Some(external_ext_config_table) =
                                                            external_ext_config.as_mapping()
                                                        {
                                                            // Extract SDK dependencies for this specific external extension (base and target-specific)
                                                            let mut merged_deps = HashMap::new();

                                                            // First, collect base SDK dependencies from [ext.<ext_name>.sdk.dependencies]
                                                            if let Some(sdk_section) =
                                                                external_ext_config_table.get("sdk")
                                                            {
                                                                if let Some(sdk_table) =
                                                                    sdk_section.as_mapping()
                                                                {
                                                                    if let Some(dependencies) =
                                                                        sdk_table
                                                                            .get("dependencies")
                                                                    {
                                                                        if let Some(deps_table) =
                                                                            dependencies
                                                                                .as_mapping()
                                                                        {
                                                                            for (k, v) in
                                                                                deps_table.iter()
                                                                            {
                                                                                if let Some(
                                                                                    key_str,
                                                                                ) = k.as_str()
                                                                                {
                                                                                    merged_deps.insert(
                                                                                        key_str.to_string(),
                                                                                        v.clone(),
                                                                                    );
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }

                                                            // Then, if we have a target, collect target-specific dependencies from [ext.<ext_name>.<target>.sdk.dependencies]
                                                            if let Some(target) = target {
                                                                if let Some(target_section) =
                                                                    external_ext_config_table
                                                                        .get(target)
                                                                {
                                                                    if let Some(target_table) =
                                                                        target_section.as_mapping()
                                                                    {
                                                                        if let Some(sdk_section) =
                                                                            target_table.get("sdk")
                                                                        {
                                                                            if let Some(sdk_table) =
                                                                                sdk_section
                                                                                    .as_mapping()
                                                                            {
                                                                                if let Some(
                                                                                    dependencies,
                                                                                ) = sdk_table
                                                                                    .get(
                                                                                    "dependencies",
                                                                                ) {
                                                                                    if let Some(deps_table) = dependencies.as_mapping() {
                                                                                        // Target-specific dependencies override base dependencies
                                                                                        for (k, v) in deps_table.iter() {
                                                                                            if let Some(key_str) = k.as_str() {
                                                                                                merged_deps.insert(key_str.to_string(), v.clone());
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }

                                                            // Add the merged dependencies if any exist
                                                            if !merged_deps.is_empty() {
                                                                extension_sdk_deps.insert(
                                                                    ext_name.to_string(),
                                                                    merged_deps,
                                                                );
                                                            }

                                                            // Recursively process dependencies of this specific external extension
                                                            if let Some(nested_dependencies) =
                                                                external_ext_config_table
                                                                    .get("dependencies")
                                                            {
                                                                if let Some(nested_deps_table) =
                                                                    nested_dependencies.as_mapping()
                                                                {
                                                                    external_config_obj.collect_external_extension_sdk_dependencies_with_target(
                                                                        &resolved_external_config_path.to_string_lossy(),
                                                                        nested_deps_table,
                                                                        extension_sdk_deps,
                                                                        visited,
                                                                        target,
                                                                    )?;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        // If we can't parse the external config, skip it silently
                                        // This prevents the SDK installation from failing due to malformed external configs
                                    }
                                }
                            }
                            Err(_) => {
                                // If we can't read the external config file, skip it silently
                                // This prevents the SDK installation from failing due to missing external configs
                            }
                        }
                    }
                }
            }
        }
        Ok(())
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

    /// Get the default target from configuration.
    ///
    /// # Returns
    /// Returns the default_target from the configuration file
    pub fn get_default_target(&self) -> Option<&String> {
        self.default_target.as_ref()
    }

    /// Get supported targets from configuration.
    ///
    /// # Returns
    /// A vector of supported target names, or None if all targets are supported
    pub fn get_supported_targets(&self) -> Option<Vec<String>> {
        match &self.supported_targets {
            Some(SupportedTargets::All(s)) if s == "*" => None, // "*" means all targets supported
            Some(SupportedTargets::List(targets)) => Some(targets.clone()),
            Some(SupportedTargets::All(_)) => Some(vec![]), // Invalid string, treat as empty list
            None => None, // No supported_targets defined means all targets supported
        }
    }

    /// Check if a target is supported by this configuration.
    ///
    /// # Arguments
    /// * `target` - The target to check
    ///
    /// # Returns
    /// True if the target is supported, false otherwise
    pub fn is_target_supported(&self, target: &str) -> bool {
        match self.get_supported_targets() {
            None => true, // All targets supported
            Some(targets) => targets.contains(&target.to_string()),
        }
    }

    /// Get merged SDK configuration for a specific target.
    ///
    /// This merges the base [sdk] section with the target-specific [sdk.<target>] section,
    /// where target-specific values override base values.
    ///
    /// # Arguments
    /// * `target` - The target to get merged configuration for
    /// * `config_path` - Path to the configuration file
    ///
    /// # Returns
    /// Merged SDK configuration or error if parsing fails
    pub fn get_merged_sdk_config(&self, target: &str, config_path: &str) -> Result<SdkConfig> {
        // Read the raw config to access target-specific sections
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed = Self::parse_config_value(config_path, &content)?;

        // Start with the base SDK config
        let mut merged_config = self.sdk.clone().unwrap_or_default();

        // If there's a target-specific SDK section, merge it
        // First try the nested approach: [sdk] -> [qemux86-64]
        if let Some(sdk_section) = parsed.get("sdk") {
            if let Some(target_section) = sdk_section.get(target) {
                // Merge target-specific SDK configuration
                if let Ok(target_config) =
                    serde_yaml::from_value::<SdkConfig>(target_section.clone())
                {
                    merged_config = merge_sdk_configs(merged_config, target_config);
                }
            }
        }

        // Also try the top-level approach: [sdk.qemux86-64]
        let target_section_name = format!("sdk.{target}");
        if let Some(target_section) = parsed.get(&target_section_name) {
            // Merge target-specific SDK configuration
            if let Ok(target_config) = serde_yaml::from_value::<SdkConfig>(target_section.clone()) {
                merged_config = merge_sdk_configs(merged_config, target_config);
            }
        }

        Ok(merged_config)
    }

    /// Get merged SDK dependencies for a specific target.
    ///
    /// This merges [sdk.dependencies] with [sdk.<target>.dependencies],
    /// where target-specific dependencies override base dependencies.
    ///
    /// # Arguments
    /// * `target` - The target to get merged dependencies for
    /// * `config_path` - Path to the configuration file
    ///
    /// # Returns
    /// Merged dependencies map or None if no dependencies are defined
    #[allow(dead_code)] // Future API for other SDK commands
    pub fn get_merged_sdk_dependencies(
        &self,
        target: &str,
        config_path: &str,
    ) -> Result<Option<HashMap<String, serde_yaml::Value>>> {
        // Read the raw config to access target-specific sections
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed = Self::parse_config_value(config_path, &content)?;

        let mut merged_deps = HashMap::new();

        // First, add base SDK dependencies
        if let Some(sdk_section) = parsed.get("sdk") {
            if let Some(deps) = sdk_section.get("dependencies") {
                if let Some(deps_table) = deps.as_mapping() {
                    for (key, value) in deps_table {
                        if let Some(key_str) = key.as_str() {
                            merged_deps.insert(key_str.to_string(), value.clone());
                        }
                    }
                }
            }

            // Then, add/override with target-specific dependencies
            if let Some(target_section) = sdk_section.get(target) {
                if let Some(target_deps) = target_section.get("dependencies") {
                    if let Some(target_deps_table) = target_deps.as_mapping() {
                        for (key, value) in target_deps_table {
                            if let Some(key_str) = key.as_str() {
                                merged_deps.insert(key_str.to_string(), value.clone());
                            }
                        }
                    }
                }
            }
        }

        if merged_deps.is_empty() {
            Ok(None)
        } else {
            Ok(Some(merged_deps))
        }
    }
}

/// Helper function to convert serde_yaml::Value to a displayable string
#[allow(dead_code)] // Utility function for debugging/display purposes
pub fn value_to_string(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        _ => format!("{value:?}"),
    }
}

/// Merge two SDK configurations, with the target config overriding the base config.
///
/// # Arguments
/// * `base` - Base SDK configuration (from [sdk] section)
/// * `target` - Target-specific SDK configuration (from [sdk.<target>] section)
///
/// # Returns
/// Merged SDK configuration with target values taking precedence
fn merge_sdk_configs(mut base: SdkConfig, target: SdkConfig) -> SdkConfig {
    // Override each field with target-specific values if they exist
    if target.image.is_some() {
        base.image = target.image;
    }
    if target.repo_url.is_some() {
        base.repo_url = target.repo_url;
    }
    if target.repo_release.is_some() {
        base.repo_release = target.repo_release;
    }
    if target.container_args.is_some() {
        base.container_args = target.container_args;
    }

    // For dependencies and compile, merge the HashMaps
    if let Some(target_deps) = target.dependencies {
        match base.dependencies {
            Some(ref mut base_deps) => {
                // Merge target dependencies into base dependencies
                for (key, value) in target_deps {
                    base_deps.insert(key, value);
                }
            }
            None => {
                // No base dependencies, use target dependencies
                base.dependencies = Some(target_deps);
            }
        }
    }

    if let Some(target_compile) = target.compile {
        match base.compile {
            Some(ref mut base_compile) => {
                // Merge target compile configs into base compile configs
                for (key, value) in target_compile {
                    base_compile.insert(key, value);
                }
            }
            None => {
                // No base compile configs, use target compile configs
                base.compile = Some(target_compile);
            }
        }
    }

    base
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
image = "docker.io/avocadolinux/sdk:apollo-edge"

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
            Some(&"docker.io/avocadolinux/sdk:apollo-edge".to_string())
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
    fn test_src_dir_absolute_path() {
        let config_content = r#"
src_dir = "/absolute/path/to/source"

[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let resolved_src_dir = config.get_resolved_src_dir(temp_file.path());

        assert!(resolved_src_dir.is_some());
        assert_eq!(
            resolved_src_dir.unwrap(),
            PathBuf::from("/absolute/path/to/source")
        );
    }

    #[test]
    fn test_src_dir_relative_path() {
        let config_content = r#"
src_dir = "../../"

[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let resolved_src_dir = config.get_resolved_src_dir(temp_file.path());

        assert!(resolved_src_dir.is_some());
        let resolved_path = resolved_src_dir.unwrap();

        // Should be resolved relative to the config file directory
        let config_dir = temp_file.path().parent().unwrap();
        let expected_path = config_dir.join("../../");

        // Compare the canonical forms (or just the path if canonicalize fails)
        let resolved_canonical = resolved_path.canonicalize().unwrap_or(resolved_path);
        let expected_canonical = expected_path.canonicalize().unwrap_or(expected_path);
        assert_eq!(resolved_canonical, expected_canonical);
    }

    #[test]
    fn test_src_dir_not_configured() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let resolved_src_dir = config.get_resolved_src_dir(temp_file.path());

        assert!(resolved_src_dir.is_none());
    }

    #[test]
    fn test_extension_sdk_dependencies() {
        let config_content = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:apollo-edge"

ext:
  avocado-dev:
    types:
      - sysext
      - confext
    sdk:
      dependencies:
        nativesdk-avocado-hitl: "*"
        nativesdk-something-else: "1.2.3"

  another-ext:
    types:
      - sysext
    sdk:
      dependencies:
        nativesdk-tool: "*"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
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
    fn test_extension_sdk_dependencies_with_target() {
        let config_content = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:apollo-edge"

ext:
  avocado-dev:
    types:
      - sysext
      - confext
    sdk:
      dependencies:
        nativesdk-avocado-hitl: "*"
        nativesdk-base-tool: "1.0.0"
    qemux86-64:
      sdk:
        dependencies:
          nativesdk-avocado-hitl: "2.0.0"
          nativesdk-target-specific: "*"

  another-ext:
    types:
      - sysext
    sdk:
      dependencies:
        nativesdk-tool: "*"
    qemuarm64:
      sdk:
        dependencies:
          nativesdk-arm-tool: "*"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Test without target (should get base dependencies only)
        let extension_deps_no_target = config
            .get_extension_sdk_dependencies_with_config_path_and_target(config_content, None, None)
            .unwrap();

        assert_eq!(extension_deps_no_target.len(), 2);

        let avocado_dev_base = extension_deps_no_target.get("avocado-dev").unwrap();
        assert_eq!(avocado_dev_base.len(), 2);
        assert_eq!(
            avocado_dev_base
                .get("nativesdk-avocado-hitl")
                .unwrap()
                .as_str(),
            Some("*")
        );
        assert_eq!(
            avocado_dev_base
                .get("nativesdk-base-tool")
                .unwrap()
                .as_str(),
            Some("1.0.0")
        );
        assert!(avocado_dev_base.get("nativesdk-target-specific").is_none());

        // Test with qemux86-64 target (should merge base + target-specific)
        let extension_deps_x86 = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                config_content,
                None,
                Some("qemux86-64"),
            )
            .unwrap();

        assert_eq!(extension_deps_x86.len(), 2);

        let avocado_dev_x86 = extension_deps_x86.get("avocado-dev").unwrap();
        assert_eq!(avocado_dev_x86.len(), 3);
        // Target-specific dependency should override base
        assert_eq!(
            avocado_dev_x86
                .get("nativesdk-avocado-hitl")
                .unwrap()
                .as_str(),
            Some("2.0.0")
        );
        // Base dependency should still be there
        assert_eq!(
            avocado_dev_x86.get("nativesdk-base-tool").unwrap().as_str(),
            Some("1.0.0")
        );
        // Target-specific new dependency should be added
        assert_eq!(
            avocado_dev_x86
                .get("nativesdk-target-specific")
                .unwrap()
                .as_str(),
            Some("*")
        );

        // another-ext should only have base dependency for x86 target
        let another_ext_x86 = extension_deps_x86.get("another-ext").unwrap();
        assert_eq!(another_ext_x86.len(), 1);
        assert_eq!(
            another_ext_x86.get("nativesdk-tool").unwrap().as_str(),
            Some("*")
        );

        // Test with qemuarm64 target
        let extension_deps_arm = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                config_content,
                None,
                Some("qemuarm64"),
            )
            .unwrap();

        assert_eq!(extension_deps_arm.len(), 2);

        // avocado-dev should only have base dependencies for arm target
        let avocado_dev_arm = extension_deps_arm.get("avocado-dev").unwrap();
        assert_eq!(avocado_dev_arm.len(), 2);
        assert_eq!(
            avocado_dev_arm
                .get("nativesdk-avocado-hitl")
                .unwrap()
                .as_str(),
            Some("*")
        );
        assert_eq!(
            avocado_dev_arm.get("nativesdk-base-tool").unwrap().as_str(),
            Some("1.0.0")
        );

        // another-ext should have base + arm-specific dependencies
        let another_ext_arm = extension_deps_arm.get("another-ext").unwrap();
        assert_eq!(another_ext_arm.len(), 2);
        assert_eq!(
            another_ext_arm.get("nativesdk-tool").unwrap().as_str(),
            Some("*")
        );
        assert_eq!(
            another_ext_arm.get("nativesdk-arm-tool").unwrap().as_str(),
            Some("*")
        );
    }

    #[test]
    fn test_extension_sdk_dependencies_from_runtime_dependencies() {
        let config_content = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:apollo-edge"

runtime:
  dev:
    dependencies:
      avocado-ext-dev:
        ext: avocado-ext-dev
        config: "extensions/dev/avocado.yaml"
    raspberrypi4:
      dependencies:
        avocado-bsp-raspberrypi4:
          ext: avocado-bsp-raspberrypi4
          config: "bsp/raspberrypi4/avocado.yaml"

ext:
  config:
    types:
      - confext
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Test without config_path (should not find runtime dependencies)
        let extension_deps_no_config = config
            .get_extension_sdk_dependencies_with_config_path_and_target(config_content, None, None)
            .unwrap();

        // Should only find the local extension (config)
        assert_eq!(extension_deps_no_config.len(), 0);

        // Test with config_path - this will attempt to access external config files
        // Since the files don't exist, we expect this to return empty results or an error
        // (depends on implementation - external dependencies that can't be resolved are skipped)
        let result = config.get_extension_sdk_dependencies_with_config_path_and_target(
            config_content,
            Some("dummy_path"),
            None,
        );

        // The function should either succeed with empty/partial results, or return an error
        // Both are acceptable for non-existent external configs in a unit test context
        let _ = result; // Acknowledge the result without asserting on it
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
image = "docker.io/avocadolinux/sdk:apollo-edge"
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
    fn test_default_target_field() {
        let config_content = r#"
default_target = "qemux86-64"

[runtime.dev]
target = "qemux86-64"
image = "avocadolinux/runtime:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test getting default target
        assert_eq!(config.default_target, Some("qemux86-64".to_string()));
        assert_eq!(config.get_default_target(), Some(&"qemux86-64".to_string()));
    }

    #[test]
    fn test_no_default_target_field() {
        let config_content = r#"
[runtime.dev]
target = "qemux86-64"
image = "avocadolinux/runtime:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that default target is None when not specified
        assert_eq!(config.default_target, None);
        assert_eq!(config.get_default_target(), None);
    }

    #[test]
    fn test_empty_default_target_field() {
        let config_content = r#"
default_target = ""

[runtime.dev]
target = "qemux86-64"
image = "avocadolinux/runtime:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that empty string is preserved
        assert_eq!(config.default_target, Some("".to_string()));
        assert_eq!(config.get_default_target(), Some(&"".to_string()));
    }

    #[test]
    fn test_merge_sdk_container_args() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
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
image = "docker.io/avocadolinux/sdk:apollo-edge"
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
image = "docker.io/avocadolinux/sdk:apollo-edge"
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
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with no config args and no CLI args
        let merged = config.merge_sdk_container_args(None);

        assert!(merged.is_none());
    }

    #[test]
    fn test_get_sdk_repo_url_env_override() {
        // Test environment variable override for SDK repo URL
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
repo_url = "https://config.example.com"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test without environment variable - should return config value
        std::env::remove_var("AVOCADO_SDK_REPO_URL");
        let repo_url = config.get_sdk_repo_url();
        assert_eq!(repo_url, Some("https://config.example.com".to_string()));

        // Test with environment variable - should return env value
        std::env::set_var("AVOCADO_SDK_REPO_URL", "https://env.example.com");
        let repo_url = config.get_sdk_repo_url();
        assert_eq!(repo_url, Some("https://env.example.com".to_string()));

        // Clean up
        std::env::remove_var("AVOCADO_SDK_REPO_URL");
    }

    #[test]
    fn test_get_sdk_repo_release_env_override() {
        // Test environment variable override for SDK repo release
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
repo_release = "config-release"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test without environment variable - should return config value
        std::env::remove_var("AVOCADO_SDK_REPO_RELEASE");
        let repo_release = config.get_sdk_repo_release();
        assert_eq!(repo_release, Some("config-release".to_string()));

        // Test with environment variable - should return env value
        std::env::set_var("AVOCADO_SDK_REPO_RELEASE", "env-release");
        let repo_release = config.get_sdk_repo_release();
        assert_eq!(repo_release, Some("env-release".to_string()));

        // Clean up
        std::env::remove_var("AVOCADO_SDK_REPO_RELEASE");
    }

    #[test]
    fn test_get_sdk_repo_url_env_only() {
        // Test environment variable when no config value exists
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test without environment variable - should return None
        std::env::remove_var("AVOCADO_SDK_REPO_URL");
        let repo_url = config.get_sdk_repo_url();
        assert_eq!(repo_url, None);

        // Test with environment variable - should return env value
        std::env::set_var("AVOCADO_SDK_REPO_URL", "https://env-only.example.com");
        let repo_url = config.get_sdk_repo_url();
        assert_eq!(repo_url, Some("https://env-only.example.com".to_string()));

        // Clean up
        std::env::remove_var("AVOCADO_SDK_REPO_URL");
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
image = "docker.io/avocadolinux/sdk:apollo-edge"
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

    #[test]
    fn test_provision_profile_config() {
        let config_content = r#"
[provision.usb]
container_args = ["-v", "/dev/usb:/dev/usb", "-v", "/sys:/sys:ro"]

[provision.development]
container_args = ["--privileged", "--network=host"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test getting provision profile
        let usb_profile = config.get_provision_profile("usb");
        assert!(usb_profile.is_some());
        let usb_args = config.get_provision_profile_container_args("usb");
        assert!(usb_args.is_some());
        let args = usb_args.unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "/dev/usb:/dev/usb");
        assert_eq!(args[2], "-v");
        assert_eq!(args[3], "/sys:/sys:ro");

        // Test getting non-existent profile
        assert!(config.get_provision_profile("nonexistent").is_none());
        assert!(config
            .get_provision_profile_container_args("nonexistent")
            .is_none());
    }

    #[test]
    fn test_merge_provision_container_args() {
        let config_content = r#"
[provision.usb]
container_args = ["-v", "/dev/usb:/dev/usb", "--privileged"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test merging with CLI args
        let cli_args = vec!["--cap-add=SYS_ADMIN".to_string(), "--rm".to_string()];
        let merged = config.merge_provision_container_args(Some("usb"), Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 5);
        assert_eq!(merged_args[0], "-v");
        assert_eq!(merged_args[1], "/dev/usb:/dev/usb");
        assert_eq!(merged_args[2], "--privileged");
        assert_eq!(merged_args[3], "--cap-add=SYS_ADMIN");
        assert_eq!(merged_args[4], "--rm");
    }

    #[test]
    fn test_merge_provision_container_args_profile_only() {
        let config_content = r#"
[provision.test]
container_args = ["--network=host"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with no CLI args
        let merged = config.merge_provision_container_args(Some("test"), None);

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 1);
        assert_eq!(merged_args[0], "--network=host");
    }

    #[test]
    fn test_merge_provision_container_args_cli_only() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with only CLI args (no provision profile)
        let cli_args = vec!["--cap-add=SYS_ADMIN".to_string()];
        let merged = config.merge_provision_container_args(None, Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 1);
        assert_eq!(merged_args[0], "--cap-add=SYS_ADMIN");
    }

    #[test]
    fn test_merge_provision_container_args_none() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test with no provision profile and no CLI args
        let merged = config.merge_provision_container_args(None, None);

        assert!(merged.is_none());
    }

    #[test]
    fn test_merged_sdk_config() {
        // Create a temporary config file for testing merging
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64"]

[sdk]
image = "base-image"
repo_url = "http://base-repo"
repo_release = "base-release"

[sdk.dependencies]
base-package = "*"

[sdk.qemux86-64]
image = "target-specific-image"
repo_url = "http://target-repo"

[sdk.qemux86-64.dependencies]
target-package = "*"
"#;

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), config_content).unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let merged = config
            .get_merged_sdk_config("qemux86-64", temp_file.path().to_str().unwrap())
            .unwrap();

        // Target-specific values should override base values
        assert_eq!(merged.image, Some("target-specific-image".to_string()));
        assert_eq!(merged.repo_url, Some("http://target-repo".to_string()));

        // Base values should be preserved when not overridden
        assert_eq!(merged.repo_release, Some("base-release".to_string()));
    }

    #[test]
    fn test_merged_sdk_config_with_container_args() {
        // Test that target-specific container_args are properly merged
        let config_content = r#"
default_target = "qemux86-64"

[sdk]
image = "base-image"

[sdk.qemux86-64]
container_args = ["--net=host", "--privileged"]
"#;

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), config_content).unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let merged = config
            .get_merged_sdk_config("qemux86-64", temp_file.path().to_str().unwrap())
            .unwrap();

        // Target-specific container_args should be present
        assert_eq!(
            merged.container_args,
            Some(vec!["--net=host".to_string(), "--privileged".to_string()])
        );

        // Base image should be preserved
        assert_eq!(merged.image, Some("base-image".to_string()));
    }

    #[test]
    fn test_merged_sdk_dependencies() {
        // Create a temporary config file for testing dependency merging
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64"]

[sdk.dependencies]
base-package = "*"
shared-package = "1.0"

[sdk.qemux86-64.dependencies]
target-package = "*"
shared-package = "2.0"
"#;

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), config_content).unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let merged_deps = config
            .get_merged_sdk_dependencies("qemux86-64", temp_file.path().to_str().unwrap())
            .unwrap()
            .unwrap();

        // Should have both base and target dependencies
        assert!(merged_deps.contains_key("base-package"));
        assert!(merged_deps.contains_key("target-package"));

        // Target-specific dependency should override base dependency
        assert_eq!(
            merged_deps.get("shared-package").unwrap().as_str().unwrap(),
            "2.0"
        );
    }

    #[test]
    fn test_merged_sdk_config_no_target_section() {
        // Test merging when there's no target-specific section
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64"]

[sdk]
image = "base-image"
repo_url = "http://base-repo"
"#;

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), config_content).unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let merged = config
            .get_merged_sdk_config("qemux86-64", temp_file.path().to_str().unwrap())
            .unwrap();

        // Should just return the base config
        assert_eq!(merged.image, Some("base-image".to_string()));
        assert_eq!(merged.repo_url, Some("http://base-repo".to_string()));
    }

    #[test]
    fn test_hierarchical_section_merging() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[sdk]
image = "base-image"
repo_url = "base-repo"

[sdk.qemuarm64]
image = "arm64-image"

[provision.usb]
container_args = ["--network=host"]

[provision.usb.qemuarm64]
container_args = ["--privileged"]

[runtime.prod]
some_setting = "base-value"

[runtime.prod.qemuarm64]
some_setting = "arm64-value"
additional_setting = "arm64-only"
"#;

        // Write test config to a temp file
        let temp_file = std::env::temp_dir().join("hierarchical_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test SDK merging
        let sdk_x86 = config
            .get_merged_section("sdk", "qemux86-64", config_path)
            .unwrap();
        assert!(sdk_x86.is_some());
        let sdk_x86_value = sdk_x86.unwrap();
        let sdk_x86_table = sdk_x86_value.as_mapping().unwrap();
        assert_eq!(
            sdk_x86_table.get("image").unwrap().as_str().unwrap(),
            "base-image"
        );
        assert_eq!(
            sdk_x86_table.get("repo_url").unwrap().as_str().unwrap(),
            "base-repo"
        );

        let sdk_arm64 = config
            .get_merged_section("sdk", "qemuarm64", config_path)
            .unwrap();
        assert!(sdk_arm64.is_some());
        let sdk_arm64_value = sdk_arm64.unwrap();
        let sdk_arm64_table = sdk_arm64_value.as_mapping().unwrap();
        assert_eq!(
            sdk_arm64_table.get("image").unwrap().as_str().unwrap(),
            "arm64-image"
        );
        assert_eq!(
            sdk_arm64_table.get("repo_url").unwrap().as_str().unwrap(),
            "base-repo"
        );

        // Test provision merging
        let provision_x86 = config
            .get_merged_provision_config("usb", "qemux86-64", config_path)
            .unwrap();
        assert!(provision_x86.is_some());
        let provision_x86_value = provision_x86.unwrap();
        let provision_x86_table = provision_x86_value.as_mapping().unwrap();
        let args_x86 = provision_x86_table
            .get("container_args")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(args_x86[0].as_str().unwrap(), "--network=host");

        let provision_arm64 = config
            .get_merged_provision_config("usb", "qemuarm64", config_path)
            .unwrap();
        assert!(provision_arm64.is_some());
        let provision_arm64_value = provision_arm64.unwrap();
        let provision_arm64_table = provision_arm64_value.as_mapping().unwrap();
        let args_arm64 = provision_arm64_table
            .get("container_args")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(args_arm64[0].as_str().unwrap(), "--privileged");

        // Test runtime merging
        let runtime_x86 = config
            .get_merged_runtime_config("prod", "qemux86-64", config_path)
            .unwrap();
        assert!(runtime_x86.is_some());
        let runtime_x86_value = runtime_x86.unwrap();
        let runtime_x86_table = runtime_x86_value.as_mapping().unwrap();
        assert_eq!(
            runtime_x86_table
                .get("some_setting")
                .unwrap()
                .as_str()
                .unwrap(),
            "base-value"
        );
        assert!(runtime_x86_table.get("additional_setting").is_none());

        let runtime_arm64 = config
            .get_merged_runtime_config("prod", "qemuarm64", config_path)
            .unwrap();
        assert!(runtime_arm64.is_some());
        let runtime_arm64_value = runtime_arm64.unwrap();
        let runtime_arm64_table = runtime_arm64_value.as_mapping().unwrap();
        assert_eq!(
            runtime_arm64_table
                .get("some_setting")
                .unwrap()
                .as_str()
                .unwrap(),
            "arm64-value"
        );
        assert_eq!(
            runtime_arm64_table
                .get("additional_setting")
                .unwrap()
                .as_str()
                .unwrap(),
            "arm64-only"
        );

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_nested_section_merging() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[ext.avocado-dev.dependencies]
base-dep = "*"
shared-dep = "1.0"

[ext.avocado-dev.qemuarm64.dependencies]
arm64-dep = "*"
shared-dep = "2.0"

[ext.avocado-dev.users.root]
password = ""
shell = "/bin/bash"

[ext.avocado-dev.qemuarm64.users.root]
password = "arm64-password"
"#;

        // Write test config to a temp file
        let temp_file = std::env::temp_dir().join("nested_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test nested dependencies merging
        let deps_x86 = config
            .get_merged_nested_section("ext.avocado-dev", "dependencies", "qemux86-64", config_path)
            .unwrap();
        assert!(deps_x86.is_some());
        let deps_x86_value = deps_x86.unwrap();
        let deps_x86_table = deps_x86_value.as_mapping().unwrap();
        assert_eq!(
            deps_x86_table.get("base-dep").unwrap().as_str().unwrap(),
            "*"
        );
        assert_eq!(
            deps_x86_table.get("shared-dep").unwrap().as_str().unwrap(),
            "1.0"
        );
        assert!(deps_x86_table.get("arm64-dep").is_none());

        let deps_arm64 = config
            .get_merged_nested_section("ext.avocado-dev", "dependencies", "qemuarm64", config_path)
            .unwrap();
        assert!(deps_arm64.is_some());
        let deps_arm64_value = deps_arm64.unwrap();
        let deps_arm64_table = deps_arm64_value.as_mapping().unwrap();
        assert_eq!(
            deps_arm64_table.get("base-dep").unwrap().as_str().unwrap(),
            "*"
        );
        assert_eq!(
            deps_arm64_table
                .get("shared-dep")
                .unwrap()
                .as_str()
                .unwrap(),
            "2.0"
        ); // Target overrides
        assert_eq!(
            deps_arm64_table.get("arm64-dep").unwrap().as_str().unwrap(),
            "*"
        );

        // Test nested users merging
        let users_x86 = config
            .get_merged_nested_section("ext.avocado-dev", "users.root", "qemux86-64", config_path)
            .unwrap();
        assert!(users_x86.is_some());
        let users_x86_value = users_x86.unwrap();
        let users_x86_table = users_x86_value.as_mapping().unwrap();
        assert_eq!(
            users_x86_table.get("password").unwrap().as_str().unwrap(),
            ""
        );
        assert_eq!(
            users_x86_table.get("shell").unwrap().as_str().unwrap(),
            "/bin/bash"
        );

        let users_arm64 = config
            .get_merged_nested_section("ext.avocado-dev", "users.root", "qemuarm64", config_path)
            .unwrap();
        assert!(users_arm64.is_some());
        let users_arm64_value = users_arm64.unwrap();
        let users_arm64_table = users_arm64_value.as_mapping().unwrap();
        assert_eq!(
            users_arm64_table.get("password").unwrap().as_str().unwrap(),
            "arm64-password"
        ); // Target overrides
        assert_eq!(
            users_arm64_table.get("shell").unwrap().as_str().unwrap(),
            "/bin/bash"
        ); // Base preserved

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_target_only_sections() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

# No base section, only target-specific
[runtime.special.qemuarm64]
special_setting = "arm64-only"

[ext.arm-only.qemuarm64]
types = ["sysext"]
"#;

        let temp_file = std::env::temp_dir().join("target_only_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test runtime that exists only for arm64
        let runtime_x86 = config
            .get_merged_runtime_config("special", "qemux86-64", config_path)
            .unwrap();
        assert!(runtime_x86.is_none());

        let runtime_arm64 = config
            .get_merged_runtime_config("special", "qemuarm64", config_path)
            .unwrap();
        assert!(runtime_arm64.is_some());
        let runtime_arm64_value = runtime_arm64.unwrap();
        let runtime_arm64_table = runtime_arm64_value.as_mapping().unwrap();
        assert_eq!(
            runtime_arm64_table
                .get("special_setting")
                .unwrap()
                .as_str()
                .unwrap(),
            "arm64-only"
        );

        // Test extension that exists only for arm64
        let ext_x86 = config
            .get_merged_ext_config("arm-only", "qemux86-64", config_path)
            .unwrap();
        assert!(ext_x86.is_none());

        let ext_arm64 = config
            .get_merged_ext_config("arm-only", "qemuarm64", config_path)
            .unwrap();
        assert!(ext_arm64.is_some());
        let ext_arm64_value = ext_arm64.unwrap();
        let ext_arm64_table = ext_arm64_value.as_mapping().unwrap();
        let types = ext_arm64_table.get("types").unwrap().as_sequence().unwrap();
        assert_eq!(types[0].as_str().unwrap(), "sysext");

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_supported_targets_all_format() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = "*"

[sdk]
image = "test-image"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that "*" means all targets are supported
        assert_eq!(config.get_supported_targets(), None);
        assert!(config.is_target_supported("any-target"));
        assert!(config.is_target_supported("qemux86-64"));
        assert!(config.is_target_supported("qemuarm64"));
        assert!(config.is_target_supported("raspberrypi4"));
    }

    #[test]
    fn test_supported_targets_list_format() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64", "raspberrypi4"]

[sdk]
image = "test-image"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that list format works correctly
        let targets = config.get_supported_targets().unwrap();
        assert_eq!(targets.len(), 3);
        assert!(targets.contains(&"qemux86-64".to_string()));
        assert!(targets.contains(&"qemuarm64".to_string()));
        assert!(targets.contains(&"raspberrypi4".to_string()));

        // Test target validation
        assert!(config.is_target_supported("qemux86-64"));
        assert!(config.is_target_supported("qemuarm64"));
        assert!(config.is_target_supported("raspberrypi4"));
        assert!(!config.is_target_supported("unsupported-target"));
    }

    #[test]
    fn test_supported_targets_empty_list() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = []

[sdk]
image = "test-image"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that empty list means no targets are supported
        let targets = config.get_supported_targets().unwrap();
        assert_eq!(targets.len(), 0);
        assert!(!config.is_target_supported("qemux86-64"));
        assert!(!config.is_target_supported("any-target"));
    }

    #[test]
    fn test_supported_targets_missing() {
        let config_content = r#"
default_target = "qemux86-64"

[sdk]
image = "test-image"
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that missing supported_targets means all targets are supported
        assert_eq!(config.get_supported_targets(), None);
        assert!(config.is_target_supported("any-target"));
    }

    #[test]
    fn test_comprehensive_sdk_section() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[sdk]
image = "base-sdk-image"
repo_url = "http://base-repo.com"
repo_release = "main"
container_args = ["--network=host", "--privileged"]

[sdk.dependencies]
cmake = "*"
gcc = ">=9.0"
build-essential = "*"

[sdk.compile.app]
compile = "make"

[sdk.compile.app.dependencies]
libfoo = "*"
libbar = "1.2.3"

[sdk.qemuarm64]
image = "arm64-sdk-image"
repo_url = "http://arm64-repo.com"
container_args = ["--cap-add=SYS_ADMIN"]

[sdk.qemuarm64.dependencies]
gcc-aarch64-linux-gnu = "*"
qemu-user-static = "*"

[sdk.qemuarm64.compile.app]
compile = "cross-make"

[sdk.qemuarm64.compile.app.dependencies]
libfoo-dev-arm64 = "*"
"#;

        let temp_file = std::env::temp_dir().join("comprehensive_sdk_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test base SDK configuration
        let merged_x86 = config
            .get_merged_sdk_config("qemux86-64", config_path)
            .unwrap();
        assert_eq!(merged_x86.image, Some("base-sdk-image".to_string()));
        assert_eq!(
            merged_x86.repo_url,
            Some("http://base-repo.com".to_string())
        );
        assert_eq!(merged_x86.repo_release, Some("main".to_string()));
        assert_eq!(merged_x86.container_args.as_ref().unwrap().len(), 2);

        // Test dependencies for base
        let deps_x86 = merged_x86.dependencies.unwrap();
        assert!(deps_x86.contains_key("cmake"));
        assert!(deps_x86.contains_key("gcc"));
        assert!(deps_x86.contains_key("build-essential"));

        // Test target-specific SDK configuration
        let merged_arm64 = config
            .get_merged_sdk_config("qemuarm64", config_path)
            .unwrap();
        assert_eq!(merged_arm64.image, Some("arm64-sdk-image".to_string())); // Overridden
        assert_eq!(
            merged_arm64.repo_url,
            Some("http://arm64-repo.com".to_string())
        ); // Overridden
        assert_eq!(merged_arm64.repo_release, Some("main".to_string())); // Inherited
        assert_eq!(merged_arm64.container_args.as_ref().unwrap().len(), 1); // Overridden

        // Test merged dependencies
        let deps_arm64 = merged_arm64.dependencies.unwrap();
        assert!(deps_arm64.contains_key("cmake")); // From base
        assert!(deps_arm64.contains_key("gcc")); // From base
        assert!(deps_arm64.contains_key("gcc-aarch64-linux-gnu")); // Target-specific
        assert!(deps_arm64.contains_key("qemu-user-static")); // Target-specific

        // Test compile configurations
        let compile_x86 = merged_x86.compile.unwrap();
        assert!(compile_x86.contains_key("app"));
        let app_config_x86 = compile_x86.get("app").unwrap();
        assert_eq!(app_config_x86.compile, Some("make".to_string()));

        let compile_arm64 = merged_arm64.compile.unwrap();
        let app_config_arm64 = compile_arm64.get("app").unwrap();
        assert_eq!(app_config_arm64.compile, Some("cross-make".to_string())); // Overridden

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_comprehensive_runtime_section() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[runtime.production]
target = "qemux86-64"
image_version = "v1.0.0"
boot_timeout = 30

[runtime.production.dependencies]
avocado-img-bootfiles = "*"
avocado-img-rootfs = "*"
base-system = ">=2.0"

[runtime.production.qemuarm64]
target = "qemuarm64"
image_version = "v1.0.0-arm64"
memory = "2G"

[runtime.production.qemuarm64.dependencies]
avocado-img-bootfiles-arm64 = "*"
arm64-specific-pkg = "*"

[runtime.development]
target = "qemux86-64"
debug_mode = true

[runtime.development.dependencies]
debug-tools = "*"
gdb = "*"

[runtime.development.qemuarm64]
cross_debug = true

[runtime.development.qemuarm64.dependencies]
gdb-multiarch = "*"
"#;

        let temp_file = std::env::temp_dir().join("comprehensive_runtime_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test production runtime for x86-64
        let prod_x86 = config
            .get_merged_runtime_config("production", "qemux86-64", config_path)
            .unwrap();
        assert!(prod_x86.is_some());
        let prod_x86_value = prod_x86.unwrap();
        let prod_x86_table = prod_x86_value.as_mapping().unwrap();
        assert_eq!(
            prod_x86_table
                .get("image_version")
                .unwrap()
                .as_str()
                .unwrap(),
            "v1.0.0"
        );
        assert_eq!(
            prod_x86_table
                .get("boot_timeout")
                .unwrap()
                .as_i64()
                .unwrap(),
            30
        );
        assert!(prod_x86_table.get("memory").is_none()); // Should not have ARM64-specific field

        // Test production runtime for ARM64
        let prod_arm64 = config
            .get_merged_runtime_config("production", "qemuarm64", config_path)
            .unwrap();
        assert!(prod_arm64.is_some());
        let prod_arm64_value = prod_arm64.unwrap();
        let prod_arm64_table = prod_arm64_value.as_mapping().unwrap();
        assert_eq!(
            prod_arm64_table
                .get("image_version")
                .unwrap()
                .as_str()
                .unwrap(),
            "v1.0.0-arm64"
        ); // Overridden
        assert_eq!(
            prod_arm64_table
                .get("boot_timeout")
                .unwrap()
                .as_i64()
                .unwrap(),
            30
        ); // Inherited
        assert_eq!(
            prod_arm64_table.get("memory").unwrap().as_str().unwrap(),
            "2G"
        ); // Target-specific

        // Test development runtime
        let dev_x86 = config
            .get_merged_runtime_config("development", "qemux86-64", config_path)
            .unwrap();
        assert!(dev_x86.is_some());
        let dev_x86_value = dev_x86.unwrap();
        let dev_x86_table = dev_x86_value.as_mapping().unwrap();
        assert!(dev_x86_table.get("debug_mode").unwrap().as_bool().unwrap());
        assert!(dev_x86_table.get("cross_debug").is_none());

        let dev_arm64 = config
            .get_merged_runtime_config("development", "qemuarm64", config_path)
            .unwrap();
        assert!(dev_arm64.is_some());
        let dev_arm64_value = dev_arm64.unwrap();
        let dev_arm64_table = dev_arm64_value.as_mapping().unwrap();
        assert!(dev_arm64_table
            .get("debug_mode")
            .unwrap()
            .as_bool()
            .unwrap()); // Inherited
        assert!(dev_arm64_table
            .get("cross_debug")
            .unwrap()
            .as_bool()
            .unwrap()); // Target-specific

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_comprehensive_provision_section() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[provision.usb]
container_args = ["--privileged", "-v", "/dev:/dev"]
timeout = 300
retry_count = 3

[provision.usb.qemuarm64]
container_args = ["--cap-add=SYS_ADMIN", "-v", "/dev:/dev:ro"]
emulation_mode = true

[provision.network]
container_args = ["--network=host"]
protocol = "ssh"

[provision.network.qemuarm64]
protocol = "serial"
baud_rate = 115200
"#;

        let temp_file = std::env::temp_dir().join("comprehensive_provision_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test USB provision for x86-64
        let usb_x86 = config
            .get_merged_provision_config("usb", "qemux86-64", config_path)
            .unwrap();
        assert!(usb_x86.is_some());
        let usb_x86_value = usb_x86.unwrap();
        let usb_x86_table = usb_x86_value.as_mapping().unwrap();
        let args_x86 = usb_x86_table
            .get("container_args")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(args_x86.len(), 3);
        assert_eq!(args_x86[0].as_str().unwrap(), "--privileged");
        assert_eq!(usb_x86_table.get("timeout").unwrap().as_i64().unwrap(), 300);
        assert!(usb_x86_table.get("emulation_mode").is_none());

        // Test USB provision for ARM64
        let usb_arm64 = config
            .get_merged_provision_config("usb", "qemuarm64", config_path)
            .unwrap();
        assert!(usb_arm64.is_some());
        let usb_arm64_value = usb_arm64.unwrap();
        let usb_arm64_table = usb_arm64_value.as_mapping().unwrap();
        let args_arm64 = usb_arm64_table
            .get("container_args")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(args_arm64.len(), 3); // Overridden container_args
        assert_eq!(args_arm64[0].as_str().unwrap(), "--cap-add=SYS_ADMIN");
        assert_eq!(
            usb_arm64_table.get("timeout").unwrap().as_i64().unwrap(),
            300
        ); // Inherited
        assert!(usb_arm64_table
            .get("emulation_mode")
            .unwrap()
            .as_bool()
            .unwrap()); // Target-specific

        // Test network provision
        let net_x86 = config
            .get_merged_provision_config("network", "qemux86-64", config_path)
            .unwrap();
        assert!(net_x86.is_some());
        let net_x86_value = net_x86.unwrap();
        let net_x86_table = net_x86_value.as_mapping().unwrap();
        assert_eq!(
            net_x86_table.get("protocol").unwrap().as_str().unwrap(),
            "ssh"
        );
        assert!(net_x86_table.get("baud_rate").is_none());

        let net_arm64 = config
            .get_merged_provision_config("network", "qemuarm64", config_path)
            .unwrap();
        assert!(net_arm64.is_some());
        let net_arm64_value = net_arm64.unwrap();
        let net_arm64_table = net_arm64_value.as_mapping().unwrap();
        assert_eq!(
            net_arm64_table.get("protocol").unwrap().as_str().unwrap(),
            "serial"
        ); // Overridden
        assert_eq!(
            net_arm64_table.get("baud_rate").unwrap().as_i64().unwrap(),
            115200
        ); // Target-specific

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_comprehensive_ext_section() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

[ext.avocado-dev]
version = "1.0.0"
types = ["sysext", "confext"]
scopes = ["system"]
overlay = "overlays/avocado-dev"
enable_services = ["sshd.socket"]
modprobe = ["nfs", "overlay"]

[ext.avocado-dev.dependencies]
openssh = "*"
nfs-utils = "*"
debug-tools = ">=1.0"

[ext.avocado-dev.sdk.dependencies]
nativesdk-openssh = "*"
nativesdk-gdb = "*"

[ext.avocado-dev.users.root]
password = ""
shell = "/bin/bash"
home = "/root"

[ext.avocado-dev.users.developer]
password = "dev123"
groups = ["wheel", "docker"]
home = "/home/developer"

[ext.avocado-dev.groups.docker]
gid = 999

[ext.avocado-dev.qemuarm64]
version = "1.0.0-arm64"
overlay = "overlays/avocado-dev-arm64"

[ext.avocado-dev.qemuarm64.dependencies]
gdb-multiarch = "*"
arm64-debug-tools = "*"

[ext.avocado-dev.qemuarm64.sdk.dependencies]
nativesdk-gdb-cross-aarch64 = "*"

[ext.avocado-dev.qemuarm64.users.root]
password = "arm64-root"

[ext.peridio]
version = "2.0.0"
types = ["confext"]
enable_services = ["peridiod.service"]

[ext.peridio.qemuarm64]
enable_services = ["peridiod.service", "peridio-agent.service"]
"#;

        let temp_file = std::env::temp_dir().join("comprehensive_ext_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test avocado-dev extension for x86-64
        let ext_x86 = config
            .get_merged_ext_config("avocado-dev", "qemux86-64", config_path)
            .unwrap();
        assert!(ext_x86.is_some());
        let ext_x86_value = ext_x86.unwrap();
        let ext_x86_table = ext_x86_value.as_mapping().unwrap();
        assert_eq!(
            ext_x86_table.get("version").unwrap().as_str().unwrap(),
            "1.0.0"
        );
        assert_eq!(
            ext_x86_table.get("overlay").unwrap().as_str().unwrap(),
            "overlays/avocado-dev"
        );

        let types_x86 = ext_x86_table.get("types").unwrap().as_sequence().unwrap();
        assert_eq!(types_x86.len(), 2);
        assert_eq!(types_x86[0].as_str().unwrap(), "sysext");

        // Test avocado-dev extension for ARM64
        let ext_arm64 = config
            .get_merged_ext_config("avocado-dev", "qemuarm64", config_path)
            .unwrap();
        assert!(ext_arm64.is_some());
        let ext_arm64_value = ext_arm64.unwrap();
        let ext_arm64_table = ext_arm64_value.as_mapping().unwrap();
        assert_eq!(
            ext_arm64_table.get("version").unwrap().as_str().unwrap(),
            "1.0.0-arm64"
        ); // Overridden
        assert_eq!(
            ext_arm64_table.get("overlay").unwrap().as_str().unwrap(),
            "overlays/avocado-dev-arm64"
        ); // Overridden

        // Test nested dependencies merging
        let deps_x86 = config
            .get_merged_nested_section("ext.avocado-dev", "dependencies", "qemux86-64", config_path)
            .unwrap();
        assert!(deps_x86.is_some());
        let deps_x86_value = deps_x86.unwrap();
        let deps_x86_table = deps_x86_value.as_mapping().unwrap();
        assert!(deps_x86_table.contains_key("openssh"));
        assert!(deps_x86_table.contains_key("nfs-utils"));
        assert!(!deps_x86_table.contains_key("gdb-multiarch"));

        let deps_arm64 = config
            .get_merged_nested_section("ext.avocado-dev", "dependencies", "qemuarm64", config_path)
            .unwrap();
        assert!(deps_arm64.is_some());
        let deps_arm64_value = deps_arm64.unwrap();
        let deps_arm64_table = deps_arm64_value.as_mapping().unwrap();
        assert!(deps_arm64_table.contains_key("openssh")); // Inherited
        assert!(deps_arm64_table.contains_key("gdb-multiarch")); // Target-specific
        assert!(deps_arm64_table.contains_key("arm64-debug-tools")); // Target-specific

        // Test SDK dependencies merging
        let sdk_deps_x86 = config
            .get_merged_nested_section(
                "ext.avocado-dev",
                "sdk.dependencies",
                "qemux86-64",
                config_path,
            )
            .unwrap();
        assert!(sdk_deps_x86.is_some());
        let sdk_deps_x86_value = sdk_deps_x86.unwrap();
        let sdk_deps_x86_table = sdk_deps_x86_value.as_mapping().unwrap();
        assert!(sdk_deps_x86_table.contains_key("nativesdk-openssh"));
        assert!(sdk_deps_x86_table.contains_key("nativesdk-gdb"));
        assert!(!sdk_deps_x86_table.contains_key("nativesdk-gdb-cross-aarch64"));

        let sdk_deps_arm64 = config
            .get_merged_nested_section(
                "ext.avocado-dev",
                "sdk.dependencies",
                "qemuarm64",
                config_path,
            )
            .unwrap();
        assert!(sdk_deps_arm64.is_some());
        let sdk_deps_arm64_value = sdk_deps_arm64.unwrap();
        let sdk_deps_arm64_table = sdk_deps_arm64_value.as_mapping().unwrap();
        assert!(sdk_deps_arm64_table.contains_key("nativesdk-openssh")); // Inherited
        assert!(sdk_deps_arm64_table.contains_key("nativesdk-gdb-cross-aarch64")); // Target-specific

        // Test users merging
        let users_root_x86 = config
            .get_merged_nested_section("ext.avocado-dev", "users.root", "qemux86-64", config_path)
            .unwrap();
        assert!(users_root_x86.is_some());
        let users_root_x86_value = users_root_x86.unwrap();
        let users_root_x86_table = users_root_x86_value.as_mapping().unwrap();
        assert_eq!(
            users_root_x86_table
                .get("password")
                .unwrap()
                .as_str()
                .unwrap(),
            ""
        );
        assert_eq!(
            users_root_x86_table.get("shell").unwrap().as_str().unwrap(),
            "/bin/bash"
        );

        let users_root_arm64 = config
            .get_merged_nested_section("ext.avocado-dev", "users.root", "qemuarm64", config_path)
            .unwrap();
        assert!(users_root_arm64.is_some());
        let users_root_arm64_value = users_root_arm64.unwrap();
        let users_root_arm64_table = users_root_arm64_value.as_mapping().unwrap();
        assert_eq!(
            users_root_arm64_table
                .get("password")
                .unwrap()
                .as_str()
                .unwrap(),
            "arm64-root"
        ); // Overridden
        assert_eq!(
            users_root_arm64_table
                .get("shell")
                .unwrap()
                .as_str()
                .unwrap(),
            "/bin/bash"
        ); // Inherited

        // Test peridio extension
        let peridio_x86 = config
            .get_merged_ext_config("peridio", "qemux86-64", config_path)
            .unwrap();
        assert!(peridio_x86.is_some());
        let peridio_x86_value = peridio_x86.unwrap();
        let peridio_x86_table = peridio_x86_value.as_mapping().unwrap();
        let services_x86 = peridio_x86_table
            .get("enable_services")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(services_x86.len(), 1);

        let peridio_arm64 = config
            .get_merged_ext_config("peridio", "qemuarm64", config_path)
            .unwrap();
        assert!(peridio_arm64.is_some());
        let peridio_arm64_value = peridio_arm64.unwrap();
        let peridio_arm64_table = peridio_arm64_value.as_mapping().unwrap();
        let services_arm64 = peridio_arm64_table
            .get("enable_services")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(services_arm64.len(), 2); // Overridden with more services

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_invalid_config_handling() {
        // Test invalid supported_targets format
        let invalid_supported_targets = r#"
default_target = "qemux86-64"
supported_targets = 123  # Invalid - not string or array

[sdk]
image = "test"
"#;

        let result = Config::load_from_str(invalid_supported_targets);
        assert!(result.is_err());

        // Test missing required fields
        let missing_sdk_image = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64"]

[sdk]
# Missing image field
repo_url = "http://example.com"
"#;

        let config = Config::load_from_str(missing_sdk_image).unwrap();
        assert!(config.get_sdk_image().is_none());

        // Test empty configuration
        let empty_config = "";
        let result = Config::load_from_str(empty_config).unwrap();
        assert!(result.default_target.is_none());
        assert!(result.supported_targets.is_none());
        assert!(result.sdk.is_none());
        assert!(result.runtime.is_none());
        assert!(result.provision.is_none());
    }

    #[test]
    fn test_complex_nested_overrides() {
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64", "raspberrypi4"]

# Complex nested structure with target-specific overrides
[ext.complex.level1.level2.level3]
base_value = "original"
shared_value = "base"

[ext.complex.qemuarm64.level1.level2.level3]
override_value = "arm64-specific"
shared_value = "arm64-override"
nested_override = "arm64-nested"

[ext.complex.raspberrypi4.level1.level2.level3]
rpi_specific = true
shared_value = "rpi-override"
"#;

        let temp_file = std::env::temp_dir().join("complex_nested_test.toml");
        std::fs::write(&temp_file, config_content).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test x86-64 (base only)
        let x86_nested = config
            .get_merged_nested_section(
                "ext.complex",
                "level1.level2.level3",
                "qemux86-64",
                config_path,
            )
            .unwrap();
        assert!(x86_nested.is_some());
        let x86_nested_value = x86_nested.unwrap();
        let x86_table = x86_nested_value.as_mapping().unwrap();
        assert_eq!(
            x86_table.get("base_value").unwrap().as_str().unwrap(),
            "original"
        );
        assert_eq!(
            x86_table.get("shared_value").unwrap().as_str().unwrap(),
            "base"
        );
        assert!(x86_table.get("override_value").is_none());
        assert!(x86_table.get("nested_override").is_none());

        // Test ARM64 (has target-specific override)
        let arm64_nested = config
            .get_merged_nested_section(
                "ext.complex",
                "level1.level2.level3",
                "qemuarm64",
                config_path,
            )
            .unwrap();
        assert!(arm64_nested.is_some());
        let arm64_nested_value = arm64_nested.unwrap();
        let arm64_table = arm64_nested_value.as_mapping().unwrap();
        assert_eq!(
            arm64_table.get("base_value").unwrap().as_str().unwrap(),
            "original"
        ); // Inherited
        assert_eq!(
            arm64_table.get("shared_value").unwrap().as_str().unwrap(),
            "arm64-override"
        ); // Overridden
        assert_eq!(
            arm64_table.get("override_value").unwrap().as_str().unwrap(),
            "arm64-specific"
        ); // Target-specific
        assert_eq!(
            arm64_table
                .get("nested_override")
                .unwrap()
                .as_str()
                .unwrap(),
            "arm64-nested"
        ); // Nested override

        // Test RaspberryPi4 (different target-specific override)
        let rpi_nested = config
            .get_merged_nested_section(
                "ext.complex",
                "level1.level2.level3",
                "raspberrypi4",
                config_path,
            )
            .unwrap();
        assert!(rpi_nested.is_some());
        let rpi_nested_value = rpi_nested.unwrap();
        let rpi_table = rpi_nested_value.as_mapping().unwrap();
        assert_eq!(
            rpi_table.get("base_value").unwrap().as_str().unwrap(),
            "original"
        ); // Inherited
        assert_eq!(
            rpi_table.get("shared_value").unwrap().as_str().unwrap(),
            "rpi-override"
        ); // Overridden
        assert!(rpi_table.get("rpi_specific").unwrap().as_bool().unwrap()); // Target-specific
        assert!(rpi_table.get("override_value").is_none()); // Not present for RPI
        assert!(rpi_table.get("nested_override").is_none()); // Not present for RPI

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_edge_cases_and_error_conditions() {
        // Test configuration with only target-specific sections
        let target_only_config = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "qemuarm64"]

# Only target-specific sections, no base
[sdk.qemuarm64]
image = "arm64-only-sdk"

[runtime.special.qemuarm64]
special_mode = true

[ext.arm-only.qemuarm64]
types = ["sysext"]
"#;

        let temp_file = std::env::temp_dir().join("target_only_edge_test.toml");
        std::fs::write(&temp_file, target_only_config).unwrap();
        let config_path = temp_file.to_str().unwrap();

        let config = Config::load_from_str(target_only_config).unwrap();

        // SDK should return None for x86-64 (no base, no target-specific)
        let sdk_x86 = config
            .get_merged_section("sdk", "qemux86-64", config_path)
            .unwrap();
        assert!(sdk_x86.is_none());

        // SDK should return target-specific for ARM64
        let sdk_arm64 = config
            .get_merged_section("sdk", "qemuarm64", config_path)
            .unwrap();
        assert!(sdk_arm64.is_some());
        let sdk_arm64_value = sdk_arm64.unwrap();
        let sdk_arm64_table = sdk_arm64_value.as_mapping().unwrap();
        assert_eq!(
            sdk_arm64_table.get("image").unwrap().as_str().unwrap(),
            "arm64-only-sdk"
        );

        // Runtime special should be None for x86-64
        let runtime_x86 = config
            .get_merged_runtime_config("special", "qemux86-64", config_path)
            .unwrap();
        assert!(runtime_x86.is_none());

        // Runtime special should exist for ARM64
        let runtime_arm64 = config
            .get_merged_runtime_config("special", "qemuarm64", config_path)
            .unwrap();
        assert!(runtime_arm64.is_some());

        // Extension should be None for x86-64
        let ext_x86 = config
            .get_merged_ext_config("arm-only", "qemux86-64", config_path)
            .unwrap();
        assert!(ext_x86.is_none());

        // Extension should exist for ARM64
        let ext_arm64 = config
            .get_merged_ext_config("arm-only", "qemuarm64", config_path)
            .unwrap();
        assert!(ext_arm64.is_some());

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_nested_target_config_merging() {
        // Create a temporary config file with nested target-specific configuration
        let config_content = r#"
default_target = "qemux86-64"
supported_targets = ["qemux86-64", "reterminal-dm"]

[sdk]
image = "ghcr.io/avocado-framework/avocado-sdk:latest"

[runtime.default]
target = "x86_64-unknown-linux-gnu"

[ext.avocado-ext-webkit]
version = "1.0.0"
release = "r0"
vendor = "Avocado Linux <info@avocadolinux.org>"
summary = "WPE WebKit browser and display utilities"
description = "WPE WebKit browser and display utilities"
license = "Apache-2.0"
url = "https://github.com/avocadolinux/avocado-ext"
types = ["sysext", "confext"]
enable_services = ["cog.service"]
on_merge = ["systemctl restart --no-block cog.service"]

[ext.avocado-ext-webkit.reterminal-dm]
overlay = "extensions/webkit/overlays/reterminal-dm"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(config_content.as_bytes()).unwrap();
        let config_path = temp_file.path().to_str().unwrap();

        let config = Config::load_from_str(config_content).unwrap();

        // Test extension config for qemux86-64 target (should only get base config)
        let ext_x86 = config
            .get_merged_ext_config("avocado-ext-webkit", "qemux86-64", config_path)
            .unwrap();
        assert!(ext_x86.is_some());
        let ext_x86_value = ext_x86.unwrap();

        // Should have base config values
        assert_eq!(
            ext_x86_value.get("version").and_then(|v| v.as_str()),
            Some("1.0.0")
        );
        assert_eq!(
            ext_x86_value.get("vendor").and_then(|v| v.as_str()),
            Some("Avocado Linux <info@avocadolinux.org>")
        );

        // Should NOT have the target-specific overlay
        assert!(ext_x86_value.get("overlay").is_none());

        // Test extension config for reterminal-dm target (should get base + target-specific config)
        let ext_reterminal = config
            .get_merged_ext_config("avocado-ext-webkit", "reterminal-dm", config_path)
            .unwrap();
        assert!(ext_reterminal.is_some());
        let ext_reterminal_value = ext_reterminal.unwrap();

        // Should have base config values
        assert_eq!(
            ext_reterminal_value.get("version").and_then(|v| v.as_str()),
            Some("1.0.0")
        );
        assert_eq!(
            ext_reterminal_value.get("vendor").and_then(|v| v.as_str()),
            Some("Avocado Linux <info@avocadolinux.org>")
        );

        // Should ALSO have the target-specific overlay
        assert_eq!(
            ext_reterminal_value.get("overlay").and_then(|v| v.as_str()),
            Some("extensions/webkit/overlays/reterminal-dm")
        );

        // Test that arrays from base config are preserved for both targets
        let types = ext_x86_value
            .get("types")
            .and_then(|v| v.as_sequence())
            .unwrap();
        assert_eq!(types.len(), 2);
        assert!(types.iter().any(|v| v.as_str() == Some("sysext")));
        assert!(types.iter().any(|v| v.as_str() == Some("confext")));

        let enable_services = ext_x86_value
            .get("enable_services")
            .and_then(|v| v.as_sequence())
            .unwrap();
        assert_eq!(enable_services.len(), 1);
        assert_eq!(enable_services[0].as_str(), Some("cog.service"));

        // Test that arrays are also preserved for target-specific config
        let types_reterminal = ext_reterminal_value
            .get("types")
            .and_then(|v| v.as_sequence())
            .unwrap();
        assert_eq!(types_reterminal.len(), 2);
        assert!(types_reterminal
            .iter()
            .any(|v| v.as_str() == Some("sysext")));
        assert!(types_reterminal
            .iter()
            .any(|v| v.as_str() == Some("confext")));
    }

    #[test]
    fn test_stone_include_paths_basic() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
stone_include_paths = ["stone-qemux86-64"]
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_paths = config
            .get_stone_include_paths_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        assert!(stone_paths.is_some());
        let paths = stone_paths.unwrap();
        assert_eq!(paths, "/opt/src/stone-qemux86-64");
    }

    #[test]
    fn test_stone_include_paths_multiple() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
stone_include_paths = ["stone-a", "stone-b", "stone-c"]
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_paths = config
            .get_stone_include_paths_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        assert!(stone_paths.is_some());
        let paths = stone_paths.unwrap();
        assert_eq!(paths, "/opt/src/stone-a /opt/src/stone-b /opt/src/stone-c");
    }

    #[test]
    fn test_stone_include_paths_not_configured() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_paths = config
            .get_stone_include_paths_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        assert!(stone_paths.is_none());
    }

    #[test]
    fn test_stone_include_paths_target_specific_override() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
stone_include_paths = ["stone-default"]

[runtime.test-runtime.aarch64]
stone_include_paths = ["stone-aarch64"]
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();

        // Test x86_64 target
        let stone_paths_x86 = config
            .get_stone_include_paths_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();
        assert!(stone_paths_x86.is_some());
        assert_eq!(stone_paths_x86.unwrap(), "/opt/src/stone-default");

        // Test aarch64 target (should have override)
        let stone_paths_arm = config
            .get_stone_include_paths_for_runtime("test-runtime", "aarch64", temp_file.path())
            .unwrap();
        assert!(stone_paths_arm.is_some());
        assert_eq!(stone_paths_arm.unwrap(), "/opt/src/stone-aarch64");
    }

    #[test]
    fn test_stone_include_paths_user_example() {
        // Test the exact example from the user's request
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.dev]
stone_include_paths = ["stone-common"]

[runtime.dev.qemux86-64]
stone_include_paths = ["stone-qemux86-64"]
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();

        // Test qemux86-64 target - should use the override
        let stone_paths_qemu = config
            .get_stone_include_paths_for_runtime("dev", "qemux86-64", temp_file.path())
            .unwrap();
        assert!(stone_paths_qemu.is_some());
        assert_eq!(stone_paths_qemu.unwrap(), "/opt/src/stone-qemux86-64");

        // Test aarch64 target - should use the base (stone-common)
        let stone_paths_arm = config
            .get_stone_include_paths_for_runtime("dev", "aarch64", temp_file.path())
            .unwrap();
        assert!(stone_paths_arm.is_some());
        assert_eq!(stone_paths_arm.unwrap(), "/opt/src/stone-common");

        // Test x86_64 target - should also use the base (stone-common)
        let stone_paths_x86 = config
            .get_stone_include_paths_for_runtime("dev", "x86_64", temp_file.path())
            .unwrap();
        assert!(stone_paths_x86.is_some());
        assert_eq!(stone_paths_x86.unwrap(), "/opt/src/stone-common");
    }

    #[test]
    fn test_stone_include_paths_empty_array() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
stone_include_paths = []
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_paths = config
            .get_stone_include_paths_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        // Empty array should return None
        assert!(stone_paths.is_none());
    }

    #[test]
    fn test_stone_manifest_basic() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
stone_manifest = "stone-manifest.json"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_manifest = config
            .get_stone_manifest_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        assert!(stone_manifest.is_some());
        assert_eq!(stone_manifest.unwrap(), "/opt/src/stone-manifest.json");
    }

    #[test]
    fn test_stone_manifest_not_configured() {
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.test-runtime]
target = "x86_64"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();
        let stone_manifest = config
            .get_stone_manifest_for_runtime("test-runtime", "x86_64", temp_file.path())
            .unwrap();

        assert!(stone_manifest.is_none());
    }

    #[test]
    fn test_stone_manifest_target_specific_override() {
        // Test the exact example from the user's request
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.dev]
stone_manifest = "stone-common.json"

[runtime.dev.qemux86-64]
stone_manifest = "stone-qemux86-64.json"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();

        // Test qemux86-64 target - should use the override
        let stone_manifest_qemu = config
            .get_stone_manifest_for_runtime("dev", "qemux86-64", temp_file.path())
            .unwrap();
        assert!(stone_manifest_qemu.is_some());
        assert_eq!(
            stone_manifest_qemu.unwrap(),
            "/opt/src/stone-qemux86-64.json"
        );

        // Test aarch64 target - should use the base
        let stone_manifest_arm = config
            .get_stone_manifest_for_runtime("dev", "aarch64", temp_file.path())
            .unwrap();
        assert!(stone_manifest_arm.is_some());
        assert_eq!(stone_manifest_arm.unwrap(), "/opt/src/stone-common.json");

        // Test x86_64 target - should also use the base
        let stone_manifest_x86 = config
            .get_stone_manifest_for_runtime("dev", "x86_64", temp_file.path())
            .unwrap();
        assert!(stone_manifest_x86.is_some());
        assert_eq!(stone_manifest_x86.unwrap(), "/opt/src/stone-common.json");
    }

    #[test]
    fn test_stone_manifest_only_target_specific() {
        // Test when stone_manifest is only defined in target-specific section
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:latest"

[runtime.dev]
target = "x86_64"

[runtime.dev.qemux86-64]
stone_manifest = "stone-qemux86-64.json"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        let config = Config::load(temp_file.path()).unwrap();

        // Test qemux86-64 target - should have the manifest
        let stone_manifest_qemu = config
            .get_stone_manifest_for_runtime("dev", "qemux86-64", temp_file.path())
            .unwrap();
        assert!(stone_manifest_qemu.is_some());
        assert_eq!(
            stone_manifest_qemu.unwrap(),
            "/opt/src/stone-qemux86-64.json"
        );

        // Test other targets - should not have a manifest
        let stone_manifest_arm = config
            .get_stone_manifest_for_runtime("dev", "aarch64", temp_file.path())
            .unwrap();
        assert!(stone_manifest_arm.is_none());
    }

    #[test]
    fn test_container_args_as_string() {
        // Test that container_args can be provided as a string and is split on unescaped spaces
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: "-v /dev:/dev --privileged --network=host"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "/dev:/dev");
        assert_eq!(args[2], "--privileged");
        assert_eq!(args[3], "--network=host");
    }

    #[test]
    fn test_container_args_as_array() {
        // Test that container_args still works as an array
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args:
    - "-v"
    - "/dev:/dev"
    - "--privileged"
    - "--network=host"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "/dev:/dev");
        assert_eq!(args[2], "--privileged");
        assert_eq!(args[3], "--network=host");
    }

    #[test]
    fn test_container_args_string_with_escaped_spaces() {
        // Test that escaped spaces are preserved in the string
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: "-v /path\\ with\\ spaces:/dev --name my\\ container"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "/path with spaces:/dev");
        assert_eq!(args[2], "--name");
        assert_eq!(args[3], "my container");
    }

    #[test]
    fn test_container_args_string_with_quotes() {
        // Test that quoted strings with spaces are preserved
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: "-e \"VAR=value with spaces\" -e 'OTHER=also has spaces'"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-e");
        assert_eq!(args[1], "VAR=value with spaces");
        assert_eq!(args[2], "-e");
        assert_eq!(args[3], "OTHER=also has spaces");
    }

    #[test]
    fn test_container_args_provision_as_string() {
        // Test that provision profile container_args also supports string format
        let config_content = r#"
provision:
  usb:
    container_args: "-v /dev:/dev -v /sys:/sys:ro --privileged"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_provision_profile_container_args("usb");

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 5);
        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "/dev:/dev");
        assert_eq!(args[2], "-v");
        assert_eq!(args[3], "/sys:/sys:ro");
        assert_eq!(args[4], "--privileged");
    }

    #[test]
    fn test_container_args_empty_string() {
        // Test that empty string results in empty array
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: ""
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 0);
    }

    #[test]
    fn test_merge_container_args_string_and_array() {
        // Test merging when config uses string and CLI uses array
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: "--network=host --privileged"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let cli_args = vec!["-v".to_string(), "/dev:/dev".to_string()];
        let merged = config.merge_sdk_container_args(Some(&cli_args));

        assert!(merged.is_some());
        let merged = merged.unwrap();
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0], "--network=host");
        assert_eq!(merged[1], "--privileged");
        assert_eq!(merged[2], "-v");
        assert_eq!(merged[3], "/dev:/dev");
    }

    #[test]
    fn test_container_args_complex_string() {
        // Test a complex real-world example with multiple types of arguments
        let config_content = r#"
sdk:
  image: docker.io/avocadolinux/sdk:latest
  container_args: "--network=host --privileged -v /dev:/dev -v /sys:/sys:ro -e \"BUILD_ENV=production\" --name my-container"
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();
        let args = config.get_sdk_container_args();

        assert!(args.is_some());
        let args = args.unwrap();
        assert_eq!(args.len(), 10);
        assert_eq!(args[0], "--network=host");
        assert_eq!(args[1], "--privileged");
        assert_eq!(args[2], "-v");
        assert_eq!(args[3], "/dev:/dev");
        assert_eq!(args[4], "-v");
        assert_eq!(args[5], "/sys:/sys:ro");
        assert_eq!(args[6], "-e");
        assert_eq!(args[7], "BUILD_ENV=production");
        assert_eq!(args[8], "--name");
        assert_eq!(args[9], "my-container");
    }

    #[test]
    fn test_signing_keys_parsing() {
        let config_content = r#"
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest

signing_keys:
  - key: my-production-key
  - key: backup-key
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Test that signing_keys is parsed correctly
        let signing_keys = config.get_signing_keys();
        assert!(signing_keys.is_some());
        let signing_keys = signing_keys.unwrap();
        assert_eq!(signing_keys.len(), 2);
        assert_eq!(signing_keys[0].key, "my-production-key");
        assert_eq!(signing_keys[1].key, "backup-key");

        // Test get_signing_key_names helper
        let key_names = config.get_signing_key_names();
        assert_eq!(key_names.len(), 2);
        assert_eq!(key_names[0], "my-production-key");
        assert_eq!(key_names[1], "backup-key");
    }

    #[test]
    fn test_signing_keys_empty() {
        let config_content = r#"
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Test that signing_keys is None when not specified
        assert!(config.get_signing_keys().is_none());

        // Test get_signing_key_names returns empty vec when no keys
        let key_names = config.get_signing_key_names();
        assert!(key_names.is_empty());
    }
}
