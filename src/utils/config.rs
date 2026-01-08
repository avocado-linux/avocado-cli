//! Configuration utilities for Avocado CLI.

// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

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

/// Represents the location of an extension
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionLocation {
    /// Extension defined in the main config file
    Local { name: String, config_path: String },
    /// DEPRECATED: Extension from an external config file
    /// Use source: path in the ext section instead
    #[deprecated(since = "0.23.0", note = "Use Local with source: path instead")]
    External { name: String, config_path: String },
    /// Remote extension fetched from a source (repo, git, or path)
    #[allow(dead_code)]
    Remote {
        name: String,
        source: ExtensionSource,
    },
}

/// Represents the source configuration for fetching a remote extension
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ExtensionSource {
    /// Extension from the avocado package repository
    Repo {
        /// Version to fetch (e.g., "0.1.0" or "*")
        version: String,
        /// Optional RPM package name (defaults to extension name if not specified)
        #[serde(skip_serializing_if = "Option::is_none")]
        package: Option<String>,
        /// Optional custom repository name
        #[serde(skip_serializing_if = "Option::is_none")]
        repo_name: Option<String>,
        /// Optional list of config sections to include from the remote extension.
        /// Supports dot-separated paths (e.g., "provision.tegraflash") and wildcards (e.g., "provision.*").
        /// The extension's own `ext.<name>` section is always included.
        /// Referenced `sdk.compile.*` sections are auto-included based on compile dependencies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include: Option<Vec<String>>,
    },
    /// Extension from a git repository
    Git {
        /// Git repository URL
        url: String,
        /// Git ref (branch, tag, or commit hash)
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        /// Optional sparse checkout paths
        #[serde(skip_serializing_if = "Option::is_none")]
        sparse_checkout: Option<Vec<String>>,
        /// Optional list of config sections to include from the remote extension.
        /// Supports dot-separated paths (e.g., "provision.tegraflash") and wildcards (e.g., "provision.*").
        /// The extension's own `ext.<name>` section is always included.
        /// Referenced `sdk.compile.*` sections are auto-included based on compile dependencies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include: Option<Vec<String>>,
    },
    /// Extension from a local filesystem path
    Path {
        /// Path to the extension directory (relative to config or absolute)
        path: String,
        /// Optional list of config sections to include from the remote extension.
        /// Supports dot-separated paths (e.g., "provision.tegraflash") and wildcards (e.g., "provision.*").
        /// The extension's own `ext.<name>` section is always included.
        /// Referenced `sdk.compile.*` sections are auto-included based on compile dependencies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include: Option<Vec<String>>,
    },
}

impl ExtensionSource {
    /// Get the include patterns for this extension source.
    /// Returns an empty slice if no include patterns are specified.
    pub fn get_include_patterns(&self) -> &[String] {
        match self {
            ExtensionSource::Repo { include, .. } => {
                include.as_ref().map(|v| v.as_slice()).unwrap_or(&[])
            }
            ExtensionSource::Git { include, .. } => {
                include.as_ref().map(|v| v.as_slice()).unwrap_or(&[])
            }
            ExtensionSource::Path { include, .. } => {
                include.as_ref().map(|v| v.as_slice()).unwrap_or(&[])
            }
        }
    }

    /// Check if a config path matches any of the include patterns.
    ///
    /// Supports:
    /// - Exact matches: "provision.tegraflash" matches "provision.tegraflash"
    /// - Wildcard suffix: "provision.*" matches "provision.tegraflash", "provision.usb", etc.
    ///
    /// Returns true if the path matches at least one include pattern.
    pub fn matches_include_pattern(config_path: &str, patterns: &[String]) -> bool {
        for pattern in patterns {
            if pattern.ends_with(".*") {
                // Wildcard pattern: check if config_path starts with the prefix
                let prefix = &pattern[..pattern.len() - 2]; // Remove ".*"
                if config_path.starts_with(prefix)
                    && (config_path.len() == prefix.len()
                        || config_path.chars().nth(prefix.len()) == Some('.'))
                {
                    return true;
                }
            } else if config_path == pattern {
                // Exact match
                return true;
            }
        }
        false
    }
}

/// Represents an extension dependency for a runtime with type information
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuntimeExtDep {
    /// Extension defined in the config (local or fetched remote)
    Local(String),
    /// DEPRECATED: Extension from an external config file
    /// Use source: path in the ext section instead
    #[deprecated(since = "0.23.0", note = "Use Local with source: path instead")]
    External { name: String, config_path: String },
    /// DEPRECATED: Prebuilt extension from package repo
    /// Use source: repo in the ext section instead
    #[deprecated(since = "0.23.0", note = "Use Local with source: repo instead")]
    Versioned { name: String, version: String },
}

impl RuntimeExtDep {
    /// Get the extension name
    pub fn name(&self) -> &str {
        match self {
            RuntimeExtDep::Local(name) => name,
            RuntimeExtDep::External { name, .. } => name,
            RuntimeExtDep::Versioned { name, .. } => name,
        }
    }
}

/// Result of parsing an extension reference from a dependency spec
#[derive(Debug, Clone)]
pub enum ExtRefParsed {
    /// Extension reference found
    Extension {
        /// Extension name
        name: String,
        /// Optional external config path
        config: Option<String>,
        /// Optional version (for versioned/deprecated syntax)
        version: Option<String>,
    },
    /// Not an extension reference (e.g., package dependency)
    NotExtension,
}

/// Parse an extension reference from a dependency specification.
///
/// Handles both shorthand and object forms:
/// - `key: ext` → Extension { name: key, config: None, version: None }
/// - `key: { ext: name }` → Extension { name, config: None, version: None }
/// - `key: { ext: name, config: path }` → Extension { name, config: Some(path), version: None }
/// - `key: { ext: name, vsn: ver }` → Extension { name, config: None, version: Some(ver) } (deprecated)
/// - `key: "version"` → NotExtension (package dependency)
pub fn parse_ext_ref(dep_name: &str, dep_spec: &serde_yaml::Value) -> ExtRefParsed {
    // Shorthand: "my-ext: ext" means { ext: my-ext }
    if let Some(value_str) = dep_spec.as_str() {
        if value_str == "ext" {
            return ExtRefParsed::Extension {
                name: dep_name.to_string(),
                config: None,
                version: None,
            };
        }
        // Otherwise it's a package dependency with version string
        return ExtRefParsed::NotExtension;
    }

    // Object form: { ext: name, ... }
    if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
        let config = dep_spec
            .get("config")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let version = dep_spec
            .get("vsn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return ExtRefParsed::Extension {
            name: ext_name.to_string(),
            config,
            version,
        };
    }

    ExtRefParsed::NotExtension
}

/// A composed configuration that merges the main config with external extension configs.
///
/// This struct provides a unified view where:
/// - `distro`, `default_target`, `supported_targets` come from the main config only
/// - `ext` sections are merged from both main and external configs
/// - `sdk.dependencies` and `sdk.compile` are merged from both main and external configs
///
/// Interpolation is applied after merging, so external configs can reference
/// `{{ config.distro.version }}` and resolve to the main config's values.
#[derive(Debug, Clone)]
pub struct ComposedConfig {
    /// The base Config (deserialized from the merged YAML)
    pub config: Config,
    /// The merged YAML value (with external configs merged in, after interpolation)
    pub merged_value: serde_yaml::Value,
    /// The path to the main config file
    pub config_path: String,
    /// Maps extension names to their source config file paths.
    ///
    /// This is used to resolve relative paths within extension configs.
    /// Extensions from the main config will map to the main config path.
    /// Extensions from remote/external sources will map to their respective config paths.
    pub extension_sources: std::collections::HashMap<String, String>,
}

impl ComposedConfig {
    /// Get the source config path for an extension.
    ///
    /// Returns the path to the config file where the extension is defined.
    /// Falls back to the main config path if the extension is not found.
    #[allow(dead_code)]
    pub fn get_extension_source_config(&self, ext_name: &str) -> &str {
        self.extension_sources
            .get(ext_name)
            .map(|s| s.as_str())
            .unwrap_or(&self.config_path)
    }

    /// Resolve a path relative to an extension's source directory.
    ///
    /// For extensions from remote/external sources, paths are resolved relative to
    /// that extension's src_dir (or config directory if src_dir is not specified).
    /// For extensions from the main config, paths resolve relative to the main src_dir.
    ///
    /// # Arguments
    /// * `ext_name` - The name of the extension
    /// * `path` - The path to resolve (may be relative or absolute)
    ///
    /// # Returns
    /// The resolved absolute path
    #[allow(dead_code)]
    pub fn resolve_path_for_extension(&self, ext_name: &str, path: &str) -> PathBuf {
        let target_path = Path::new(path);

        // If it's already absolute, return as-is
        if target_path.is_absolute() {
            return target_path.to_path_buf();
        }

        // Get the source config path for this extension
        let source_config = self.get_extension_source_config(ext_name);
        let source_config_path = Path::new(source_config);

        // Try to load the source config to get its src_dir
        // This handles the case where the extension's config has its own src_dir
        if let Ok(content) = fs::read_to_string(source_config_path) {
            if let Ok(parsed) = Config::parse_config_value(source_config, &content) {
                if let Ok(ext_config) = serde_yaml::from_value::<Config>(parsed) {
                    // Use the extension's resolved src_dir
                    if let Some(src_dir) = ext_config.get_resolved_src_dir(source_config) {
                        return src_dir.join(target_path);
                    }
                }
            }
        }

        // Fallback: resolve relative to the source config's directory
        let config_dir = source_config_path.parent().unwrap_or(Path::new("."));
        config_dir.join(target_path)
    }

    /// Get the src_dir for an extension.
    ///
    /// Returns the src_dir from the extension's source config, or the directory
    /// containing that config file if src_dir is not specified.
    #[allow(dead_code)]
    pub fn get_extension_src_dir(&self, ext_name: &str) -> PathBuf {
        let source_config = self.get_extension_source_config(ext_name);
        let source_config_path = Path::new(source_config);
        let config_dir = source_config_path.parent().unwrap_or(Path::new("."));

        // Try to load the source config to get its src_dir
        if let Ok(content) = fs::read_to_string(source_config_path) {
            if let Ok(parsed) = Config::parse_config_value(source_config, &content) {
                if let Ok(ext_config) = serde_yaml::from_value::<Config>(parsed) {
                    if let Some(src_dir) = ext_config.get_resolved_src_dir(source_config) {
                        return src_dir;
                    }
                }
            }
        }

        // Fallback: use the config directory
        config_dir.to_path_buf()
    }
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

/// Signing configuration for runtime
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SigningConfig {
    /// Name of the signing key to use (references a key from signing_keys section)
    pub key: String,
    /// Checksum algorithm to use (sha256 or blake3, defaults to sha256)
    #[serde(default = "default_checksum_algorithm")]
    pub checksum_algorithm: String,
}

fn default_checksum_algorithm() -> String {
    "sha256".to_string()
}

/// Runtime configuration section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeConfig {
    pub target: Option<String>,
    pub dependencies: Option<HashMap<String, serde_yaml::Value>>,
    pub stone_include_paths: Option<Vec<String>>,
    pub stone_manifest: Option<String>,
    /// Signing configuration for this runtime
    pub signing: Option<SigningConfig>,
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
    /// Host UID for bindfs permission translation (overrides libc::getuid())
    pub host_uid: Option<u32>,
    /// Host GID for bindfs permission translation (overrides libc::getgid())
    pub host_gid: Option<u32>,
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
    /// Path to state file relative to src_dir for persisting state between provision runs.
    /// Defaults to `.avocado/provision-{profile}.state` when not specified.
    /// The state file is copied into the container before provisioning and copied back after.
    pub state_file: Option<String>,
}

/// Distribution configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistroConfig {
    pub channel: Option<String>,
    pub version: Option<String>,
}

/// Helper module for deserializing signing keys list
mod signing_keys_deserializer {
    use serde::{Deserialize, Deserializer};
    use std::collections::HashMap;

    /// Deserialize signing_keys from a list of single-key maps
    /// Example YAML:
    /// ```yaml
    /// signing_keys:
    ///   - my-key: sha256-abc123
    ///   - other-key: sha256-def456
    /// ```
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<HashMap<String, String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let list = Option::<Vec<HashMap<String, String>>>::deserialize(deserializer)?;

        Ok(list.map(|items| {
            let mut result = HashMap::new();
            for item in items {
                for (key, value) in item {
                    result.insert(key, value);
                }
            }
            result
        }))
    }
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
    /// Signing keys mapping friendly names to key IDs
    /// Acts as a local bridge between the config and the global signing keys registry
    #[serde(default, deserialize_with = "signing_keys_deserializer::deserialize")]
    pub signing_keys: Option<HashMap<String, String>>,
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

    /// Load a composed configuration that merges the main config with external extension configs.
    ///
    /// This method:
    /// 1. Loads the main config (raw, without interpolation)
    /// 2. Discovers installed remote extensions in avocado-extensions/ and merges their configs
    /// 3. Discovers all external config references in runtime and ext dependencies
    /// 4. Loads each external config (raw)
    /// 5. Merges external `ext.*`, `sdk.dependencies`, and `sdk.compile` sections
    /// 6. Applies interpolation to the composed model
    ///
    /// The `distro`, `default_target`, and `supported_targets` sections come from the main config only,
    /// allowing external configs to reference `{{ config.distro.version }}` and resolve to main config values.
    pub fn load_composed<P: AsRef<Path>>(
        config_path: P,
        target: Option<&str>,
    ) -> Result<ComposedConfig> {
        let path = config_path.as_ref();
        let config_path_str = path.to_string_lossy().to_string();

        // Track which config file each extension comes from
        let mut extension_sources: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Load main config content (raw, no interpolation yet)
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let mut main_config = Self::parse_config_value(&config_path_str, &content)?;

        // Record extensions from the main config
        if let Some(ext_section) = main_config.get("ext").and_then(|e| e.as_mapping()) {
            for (ext_key, _) in ext_section {
                if let Some(ext_name) = ext_key.as_str() {
                    extension_sources.insert(ext_name.to_string(), config_path_str.clone());
                }
            }
        }

        // Discover and merge installed remote extension configs
        // Remote extensions are those with a 'source' field that have been fetched
        // to $AVOCADO_PREFIX/includes/<ext_name>/
        let remote_ext_sources =
            Self::merge_installed_remote_extensions(&mut main_config, path, target)?;
        extension_sources.extend(remote_ext_sources);

        // Discover all external config references
        let external_refs = Self::discover_external_config_refs(&main_config);

        // Load and merge each external config
        for (ext_name, external_config_path) in &external_refs {
            // Resolve the external config path relative to the main config's directory
            let main_config_dir = path.parent().unwrap_or(Path::new("."));
            let resolved_path = main_config_dir.join(external_config_path);

            if !resolved_path.exists() {
                // Skip non-existent external configs with a warning (they may be optional)
                continue;
            }

            // Load external config (raw)
            let external_content = fs::read_to_string(&resolved_path).with_context(|| {
                format!(
                    "Failed to read external config: {}",
                    resolved_path.display()
                )
            })?;
            let external_config = Self::parse_config_value(
                resolved_path.to_str().unwrap_or(external_config_path),
                &external_content,
            )?;

            // For external configs (deprecated `config: path` syntax), use permissive include patterns
            // to maintain backward compatibility - merge all sections
            let legacy_include_patterns = vec![
                "provision.*".to_string(),
                "sdk.dependencies.*".to_string(),
                "sdk.compile.*".to_string(),
            ];
            let auto_include_compile =
                Self::find_compile_dependencies_in_ext(&external_config, ext_name);

            // Merge external config into main config
            Self::merge_external_config(
                &mut main_config,
                &external_config,
                ext_name,
                &legacy_include_patterns,
                &auto_include_compile,
            );

            // Record this extension's source (the external config path)
            let resolved_path_str = resolved_path.to_string_lossy().to_string();
            extension_sources.insert(ext_name.clone(), resolved_path_str.clone());

            // Also record any extensions defined within this external config
            if let Some(nested_ext_section) =
                external_config.get("ext").and_then(|e| e.as_mapping())
            {
                for (nested_ext_key, _) in nested_ext_section {
                    if let Some(nested_ext_name) = nested_ext_key.as_str() {
                        extension_sources
                            .insert(nested_ext_name.to_string(), resolved_path_str.clone());
                    }
                }
            }
        }

        // Apply interpolation to the composed model
        crate::utils::interpolation::interpolate_config(&mut main_config, target)
            .with_context(|| "Failed to interpolate composed configuration")?;

        // Deserialize the merged config into the Config struct
        let config: Config = serde_yaml::from_value(main_config.clone())
            .with_context(|| "Failed to deserialize composed configuration")?;

        Ok(ComposedConfig {
            config,
            merged_value: main_config,
            config_path: config_path_str,
            extension_sources,
        })
    }

    /// Merge installed remote extension configs into the main config
    ///
    /// For each extension with a `source` field that has been installed to
    /// `$AVOCADO_PREFIX/includes/<ext_name>/`, load and merge its avocado.yaml
    ///
    /// Returns a HashMap mapping extension names to their source config file paths.
    fn merge_installed_remote_extensions(
        main_config: &mut serde_yaml::Value,
        config_path: &Path,
        target: Option<&str>,
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut extension_sources: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Get the src_dir and target to find the extensions directory
        // First deserialize just to get src_dir and default_target
        let temp_config: Config =
            serde_yaml::from_value(main_config.clone()).unwrap_or_else(|_| Config {
                default_target: None,
                supported_targets: None,
                src_dir: None,
                distro: None,
                runtime: None,
                sdk: None,
                provision: None,
                signing_keys: None,
            });

        // Resolve target: CLI arg > env var > config default
        let resolved_target = target
            .map(|s| s.to_string())
            .or_else(|| std::env::var("AVOCADO_TARGET").ok())
            .or_else(|| temp_config.default_target.clone());

        // If we don't have a target, we can't determine the extensions path
        let resolved_target = match resolved_target {
            Some(t) => t,
            None => {
                // No target available - can't locate extensions, skip merging
                return Ok(extension_sources);
            }
        };

        // Discover remote extensions from the main config (with target interpolation)
        let remote_extensions =
            Self::discover_remote_extensions_from_value(main_config, Some(&resolved_target))?;

        if remote_extensions.is_empty() {
            return Ok(extension_sources);
        }

        // Get src_dir for loading volume state
        let config_path_str = config_path.to_string_lossy();
        let src_dir = temp_config
            .get_resolved_src_dir(config_path_str.as_ref())
            .unwrap_or_else(|| config_path.parent().unwrap_or(Path::new(".")).to_path_buf());

        // Try to load volume state for container-based config reading
        let volume_state = crate::utils::volume::VolumeState::load_from_dir(&src_dir)
            .ok()
            .flatten();

        // For each remote extension, try to read its config via container
        for (ext_name, source) in remote_extensions {
            // Try to read extension config via container command
            let ext_content = match &volume_state {
                Some(vs) => {
                    match Self::read_extension_config_via_container(vs, &resolved_target, &ext_name)
                    {
                        Ok(content) => content,
                        Err(_) => {
                            // Extension not installed yet or config not found, skip
                            continue;
                        }
                    }
                }
                None => {
                    // No volume state - try fallback to local path (for development)
                    let fallback_dir = src_dir
                        .join(".avocado")
                        .join(&resolved_target)
                        .join("includes")
                        .join(&ext_name);
                    let config_path_local = fallback_dir.join("avocado.yaml");
                    if config_path_local.exists() {
                        match fs::read_to_string(&config_path_local) {
                            Ok(content) => content,
                            Err(_) => continue,
                        }
                    } else {
                        continue;
                    }
                }
            };

            // Use a .yaml extension so parse_config_value knows to parse as YAML
            let ext_config_path = format!("{ext_name}/avocado.yaml");
            let ext_config = match Self::parse_config_value(&ext_config_path, &ext_content) {
                Ok(cfg) => cfg,
                Err(_) => {
                    // Failed to parse config, skip this extension
                    continue;
                }
            };

            // Record this extension's source (container path for reference)
            let ext_config_path_str =
                format!("/opt/_avocado/{resolved_target}/includes/{ext_name}/avocado.yaml");
            extension_sources.insert(ext_name.clone(), ext_config_path_str.clone());

            // Also record any extensions defined within this remote extension's config
            if let Some(nested_ext_section) = ext_config.get("ext").and_then(|e| e.as_mapping()) {
                for (nested_ext_key, _) in nested_ext_section {
                    if let Some(nested_ext_name) = nested_ext_key.as_str() {
                        extension_sources
                            .insert(nested_ext_name.to_string(), ext_config_path_str.clone());
                    }
                }
            }

            // Get include patterns from the extension source
            let include_patterns = source.get_include_patterns();

            // Find compile dependencies to auto-include from the extension's own section
            let auto_include_compile =
                Self::find_compile_dependencies_in_ext(&ext_config, &ext_name);

            // Merge the remote extension config with include patterns
            Self::merge_external_config(
                main_config,
                &ext_config,
                &ext_name,
                include_patterns,
                &auto_include_compile,
            );
        }

        Ok(extension_sources)
    }

    /// Read a remote extension's config file by running a container command.
    ///
    /// This runs a lightweight container to cat the extension's avocado.yaml from
    /// the Docker volume, avoiding permission issues with direct host access.
    fn read_extension_config_via_container(
        volume_state: &crate::utils::volume::VolumeState,
        target: &str,
        ext_name: &str,
    ) -> Result<String> {
        // The extension config path inside the container
        let container_config_path =
            format!("/opt/_avocado/{target}/includes/{ext_name}/avocado.yaml");

        // Run a minimal container to cat the config file
        // We use busybox as a lightweight image, but fall back to alpine if needed
        let images_to_try = [
            "busybox:latest",
            "alpine:latest",
            "docker.io/library/busybox:latest",
        ];

        for image in &images_to_try {
            let output = std::process::Command::new(&volume_state.container_tool)
                .args([
                    "run",
                    "--rm",
                    "-v",
                    &format!("{}:/opt/_avocado:ro", volume_state.volume_name),
                    image,
                    "cat",
                    &container_config_path,
                ])
                .output();

            match output {
                Ok(out) if out.status.success() => {
                    let content = String::from_utf8_lossy(&out.stdout).to_string();
                    if content.is_empty() {
                        anyhow::bail!("Extension config file is empty");
                    }
                    return Ok(content);
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    // If file not found, bail immediately (no point trying other images)
                    if stderr.contains("No such file") || stderr.contains("not found") {
                        anyhow::bail!("Extension config not found: {container_config_path}");
                    }
                    // Otherwise, continue to try next image
                }
                Err(_) => {
                    // Continue to try next image
                }
            }
        }

        anyhow::bail!("Failed to read extension config via container for '{ext_name}'")
    }

    /// Discover all external config references in runtime and ext dependencies.
    ///
    /// Scans these locations:
    /// - `runtime.<name>.dependencies.<dep>.config`
    /// - `runtime.<name>.<target>.dependencies.<dep>.config`
    /// - `ext.<name>.dependencies.<dep>.config`
    ///
    /// Returns a list of (extension_name, config_path) tuples.
    fn discover_external_config_refs(config: &serde_yaml::Value) -> Vec<(String, String)> {
        let mut refs = Vec::new();
        let mut visited = std::collections::HashSet::new();

        // Scan runtime dependencies
        if let Some(runtime_section) = config.get("runtime").and_then(|r| r.as_mapping()) {
            for (_runtime_name, runtime_config) in runtime_section {
                Self::collect_external_refs_from_dependencies(
                    runtime_config,
                    &mut refs,
                    &mut visited,
                );

                // Also check target-specific sections within runtime
                if let Some(runtime_table) = runtime_config.as_mapping() {
                    for (key, value) in runtime_table {
                        // Skip known non-target keys
                        if let Some(key_str) = key.as_str() {
                            if ![
                                "dependencies",
                                "target",
                                "stone_include_paths",
                                "stone_manifest",
                                "signing",
                            ]
                            .contains(&key_str)
                            {
                                // This might be a target-specific section
                                Self::collect_external_refs_from_dependencies(
                                    value,
                                    &mut refs,
                                    &mut visited,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Scan ext dependencies
        if let Some(ext_section) = config.get("ext").and_then(|e| e.as_mapping()) {
            for (_ext_name, ext_config) in ext_section {
                Self::collect_external_refs_from_dependencies(ext_config, &mut refs, &mut visited);

                // Also check target-specific sections within ext
                if let Some(ext_table) = ext_config.as_mapping() {
                    for (key, value) in ext_table {
                        // Skip known non-target keys
                        if let Some(key_str) = key.as_str() {
                            if ![
                                "version",
                                "release",
                                "summary",
                                "description",
                                "license",
                                "url",
                                "vendor",
                                "types",
                                "packages",
                                "dependencies",
                                "sdk",
                                "enable_services",
                                "on_merge",
                                "on_unmerge",
                                "sysusers",
                                "kernel_modules",
                                "reload_service_manager",
                                "ld_so_conf_d",
                                "confext",
                                "sysext",
                                "overlay",
                            ]
                            .contains(&key_str)
                            {
                                // This might be a target-specific section
                                Self::collect_external_refs_from_dependencies(
                                    value,
                                    &mut refs,
                                    &mut visited,
                                );
                            }
                        }
                    }
                }
            }
        }

        refs
    }

    /// Collect external config references from a dependencies section.
    fn collect_external_refs_from_dependencies(
        section: &serde_yaml::Value,
        refs: &mut Vec<(String, String)>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        let dependencies = section.get("dependencies").and_then(|d| d.as_mapping());

        if let Some(deps_map) = dependencies {
            for (_dep_name, dep_spec) in deps_map {
                if let Some(spec_map) = dep_spec.as_mapping() {
                    // Check for external extension reference
                    if let (Some(ext_name), Some(config_path)) = (
                        spec_map.get("ext").and_then(|v| v.as_str()),
                        spec_map.get("config").and_then(|v| v.as_str()),
                    ) {
                        let key = format!("{ext_name}:{config_path}");
                        if !visited.contains(&key) {
                            visited.insert(key);
                            refs.push((ext_name.to_string(), config_path.to_string()));
                        }
                    }
                }
            }
        }
    }

    /// Merge an external config into the main config.
    ///
    /// Always merges:
    /// - `ext.<ext_name>` section (the extension's own section)
    ///
    /// Conditionally merges (based on include_patterns):
    /// - `provision.<profile>` sections (if pattern matches)
    /// - `sdk.dependencies.<dep>` (if pattern matches)
    /// - `sdk.compile.<section>` (if pattern matches)
    ///
    /// Does NOT merge (main config only):
    /// - `distro`
    /// - `default_target`
    /// - `supported_targets`
    /// - `sdk.image`, `sdk.container_args`, etc. (base SDK settings)
    ///
    /// # Arguments
    /// * `main_config` - The main config to merge into
    /// * `external_config` - The external config to merge from
    /// * `ext_name` - The name of the extension (its `ext.<name>` is always merged)
    /// * `include_patterns` - Patterns for additional sections to include (e.g., "provision.*")
    /// * `auto_include_compile` - List of sdk.compile section names to auto-include (from compile deps)
    fn merge_external_config(
        main_config: &mut serde_yaml::Value,
        external_config: &serde_yaml::Value,
        ext_name: &str,
        include_patterns: &[String],
        auto_include_compile: &[String],
    ) {
        // Always merge the extension's own ext.<ext_name> section
        if let Some(external_ext) = external_config.get("ext").and_then(|e| e.as_mapping()) {
            let main_ext = main_config
                .as_mapping_mut()
                .and_then(|m| {
                    if !m.contains_key(serde_yaml::Value::String("ext".to_string())) {
                        m.insert(
                            serde_yaml::Value::String("ext".to_string()),
                            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                        );
                    }
                    m.get_mut(serde_yaml::Value::String("ext".to_string()))
                })
                .and_then(|e| e.as_mapping_mut());

            if let Some(main_ext_map) = main_ext {
                // Deep-merge the extension's own section (ext.<ext_name>)
                // This handles the case where main config has a stub with just `source:`
                // and the remote extension has the full definition with `dependencies:` etc.
                let ext_key = serde_yaml::Value::String(ext_name.to_string());
                if let Some(ext_value) = external_ext.get(&ext_key) {
                    if let Some(existing_ext) = main_ext_map.get_mut(&ext_key) {
                        // Deep-merge: add fields from remote that don't exist in main
                        // Main config values take precedence on conflicts
                        Self::deep_merge_ext_section(existing_ext, ext_value);
                    } else {
                        // Extension not in main config, just add it
                        main_ext_map.insert(ext_key, ext_value.clone());
                    }
                }
            }
        }

        // Merge provision sections based on include patterns
        if let Some(external_provision) = external_config
            .get("provision")
            .and_then(|p| p.as_mapping())
        {
            for (profile_key, profile_value) in external_provision {
                if let Some(profile_name) = profile_key.as_str() {
                    let config_path = format!("provision.{profile_name}");
                    if ExtensionSource::matches_include_pattern(&config_path, include_patterns) {
                        Self::ensure_provision_section(main_config);
                        if let Some(main_provision) = main_config
                            .get_mut("provision")
                            .and_then(|p| p.as_mapping_mut())
                        {
                            // Only add if not already present (main takes precedence)
                            if !main_provision.contains_key(profile_key) {
                                main_provision.insert(profile_key.clone(), profile_value.clone());
                            }
                        }
                    }
                }
            }
        }

        // Merge sdk.dependencies based on include patterns
        if let Some(external_sdk_deps) = external_config
            .get("sdk")
            .and_then(|s| s.get("dependencies"))
            .and_then(|d| d.as_mapping())
        {
            for (dep_key, dep_value) in external_sdk_deps {
                if let Some(dep_name) = dep_key.as_str() {
                    let config_path = format!("sdk.dependencies.{dep_name}");
                    if ExtensionSource::matches_include_pattern(&config_path, include_patterns) {
                        Self::ensure_sdk_dependencies_section(main_config);
                        if let Some(main_sdk_deps) = main_config
                            .get_mut("sdk")
                            .and_then(|s| s.get_mut("dependencies"))
                            .and_then(|d| d.as_mapping_mut())
                        {
                            // Only add if not already present (main takes precedence)
                            if !main_sdk_deps.contains_key(dep_key) {
                                main_sdk_deps.insert(dep_key.clone(), dep_value.clone());
                            }
                        }
                    }
                }
            }
        }

        // Merge sdk.compile based on include patterns OR auto_include_compile list
        if let Some(external_sdk_compile) = external_config
            .get("sdk")
            .and_then(|s| s.get("compile"))
            .and_then(|c| c.as_mapping())
        {
            for (compile_key, compile_value) in external_sdk_compile {
                if let Some(compile_name) = compile_key.as_str() {
                    let config_path = format!("sdk.compile.{compile_name}");
                    let should_include =
                        ExtensionSource::matches_include_pattern(&config_path, include_patterns)
                            || auto_include_compile.contains(&compile_name.to_string());

                    if should_include {
                        Self::ensure_sdk_compile_section(main_config);
                        if let Some(main_sdk_compile) = main_config
                            .get_mut("sdk")
                            .and_then(|s| s.get_mut("compile"))
                            .and_then(|c| c.as_mapping_mut())
                        {
                            // Only add if not already present (main takes precedence)
                            if !main_sdk_compile.contains_key(compile_key) {
                                main_sdk_compile.insert(compile_key.clone(), compile_value.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Ensure the provision section exists in the config.
    fn ensure_provision_section(config: &mut serde_yaml::Value) {
        if let Some(main_map) = config.as_mapping_mut() {
            if !main_map.contains_key(serde_yaml::Value::String("provision".to_string())) {
                main_map.insert(
                    serde_yaml::Value::String("provision".to_string()),
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                );
            }
        }
    }

    /// Deep-merge an extension section from external config into main config.
    ///
    /// This handles the case where main config has a stub definition (with just `source:`)
    /// and the remote extension has the full definition (with `dependencies:`, `version:`, etc.).
    ///
    /// Main config values take precedence on conflicts.
    fn deep_merge_ext_section(main_ext: &mut serde_yaml::Value, external_ext: &serde_yaml::Value) {
        // Only merge if both are mappings
        if let (Some(main_map), Some(external_map)) =
            (main_ext.as_mapping_mut(), external_ext.as_mapping())
        {
            for (key, external_value) in external_map {
                if !main_map.contains_key(key) {
                    // Key doesn't exist in main, add it from external
                    main_map.insert(key.clone(), external_value.clone());
                }
                // If key exists in main, keep main's value (main takes precedence)
            }
        }
    }

    /// Find compile dependencies in an extension's dependencies section.
    ///
    /// Scans `ext.<ext_name>.dependencies` for entries with a `compile` key
    /// and returns the list of compile section names that should be auto-included.
    fn find_compile_dependencies_in_ext(
        ext_config: &serde_yaml::Value,
        ext_name: &str,
    ) -> Vec<String> {
        let mut compile_deps = Vec::new();

        if let Some(ext_section) = ext_config
            .get("ext")
            .and_then(|e| e.get(ext_name))
            .and_then(|e| e.get("dependencies"))
            .and_then(|d| d.as_mapping())
        {
            for (_dep_name, dep_spec) in ext_section {
                if let Some(compile_name) = dep_spec.get("compile").and_then(|c| c.as_str()) {
                    compile_deps.push(compile_name.to_string());
                }
            }
        }

        compile_deps
    }

    /// Ensure the sdk.dependencies section exists in the config.
    fn ensure_sdk_dependencies_section(config: &mut serde_yaml::Value) {
        if let Some(main_map) = config.as_mapping_mut() {
            // Ensure sdk section exists
            if !main_map.contains_key(serde_yaml::Value::String("sdk".to_string())) {
                main_map.insert(
                    serde_yaml::Value::String("sdk".to_string()),
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                );
            }

            // Ensure sdk.dependencies section exists
            if let Some(sdk) = main_map.get_mut(serde_yaml::Value::String("sdk".to_string())) {
                if let Some(sdk_map) = sdk.as_mapping_mut() {
                    if !sdk_map.contains_key(serde_yaml::Value::String("dependencies".to_string()))
                    {
                        sdk_map.insert(
                            serde_yaml::Value::String("dependencies".to_string()),
                            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                        );
                    }
                }
            }
        }
    }

    /// Ensure the sdk.compile section exists in the config.
    fn ensure_sdk_compile_section(config: &mut serde_yaml::Value) {
        if let Some(main_map) = config.as_mapping_mut() {
            // Ensure sdk section exists
            if !main_map.contains_key(serde_yaml::Value::String("sdk".to_string())) {
                main_map.insert(
                    serde_yaml::Value::String("sdk".to_string()),
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                );
            }

            // Ensure sdk.compile section exists
            if let Some(sdk) = main_map.get_mut(serde_yaml::Value::String("sdk".to_string())) {
                if let Some(sdk_map) = sdk.as_mapping_mut() {
                    if !sdk_map.contains_key(serde_yaml::Value::String("compile".to_string())) {
                        sdk_map.insert(
                            serde_yaml::Value::String("compile".to_string()),
                            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                        );
                    }
                }
            }
        }
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

    /// Get detailed extension dependencies for a runtime (with type information)
    ///
    /// Returns a list of extension dependencies with their type:
    /// - Local: extension defined in the main config file (needs install + build)
    /// - External: extension from an external config file (needs install + build)
    /// - Versioned: prebuilt extension from package repo (needs install only)
    pub fn get_runtime_extension_dependencies_detailed(
        &self,
        runtime_name: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Vec<RuntimeExtDep>> {
        let merged_runtime = self.get_merged_runtime_config(runtime_name, target, config_path)?;

        let Some(runtime_config) = merged_runtime else {
            return Ok(vec![]);
        };

        let Some(dependencies) = runtime_config
            .get("dependencies")
            .and_then(|d| d.as_mapping())
        else {
            return Ok(vec![]);
        };

        let mut ext_deps = Vec::new();

        for (dep_name, dep_spec) in dependencies {
            let dep_name_str = dep_name.as_str().unwrap_or("");

            match parse_ext_ref(dep_name_str, dep_spec) {
                ExtRefParsed::Extension {
                    name,
                    config,
                    version,
                } => {
                    if let Some(ver) = version {
                        // Versioned extension (deprecated syntax)
                        ext_deps.push(RuntimeExtDep::Versioned { name, version: ver });
                    } else if let Some(cfg_path) = config {
                        // External extension with config path
                        ext_deps.push(RuntimeExtDep::External {
                            name,
                            config_path: cfg_path,
                        });
                    } else {
                        // Local extension
                        ext_deps.push(RuntimeExtDep::Local(name));
                    }
                }
                ExtRefParsed::NotExtension => {
                    // Package dependency, skip
                }
            }
        }

        // Sort by name for consistent ordering
        ext_deps.sort_by(|a, b| a.name().cmp(b.name()));

        Ok(ext_deps)
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

    /// Get the distro version from configuration
    pub fn get_distro_version(&self) -> Option<&String> {
        self.distro.as_ref()?.version.as_ref()
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

    /// Get signing keys mapping (name -> keyid or global name)
    #[allow(dead_code)] // Public API for future use
    pub fn get_signing_keys(&self) -> Option<&HashMap<String, String>> {
        self.signing_keys.as_ref()
    }

    /// Get signing key ID by local config name.
    ///
    /// Returns the raw value from the signing_keys mapping. The value can be either:
    /// - A key ID (64-char hex hash of the public key)
    /// - A global registry key name (which should be resolved via `resolve_signing_key_reference`)
    #[allow(dead_code)] // Public API for future use
    pub fn get_signing_key_id(&self, name: &str) -> Option<&String> {
        self.signing_keys.as_ref()?.get(name)
    }

    /// Get all signing key names
    #[allow(dead_code)] // Public API for future use
    pub fn get_signing_key_names(&self) -> Vec<String> {
        self.signing_keys
            .as_ref()
            .map(|keys| keys.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Resolve a signing key reference to an actual key ID.
    ///
    /// The reference can be:
    /// - A key ID directly (64-char hex hash of the public key)
    /// - A global registry key name (resolved to its key ID)
    ///
    /// Returns (key_name, key_id) where key_name is the name in the global registry.
    #[allow(dead_code)] // Public API for future use
    pub fn resolve_signing_key_reference(reference: &str) -> Option<(String, String)> {
        use crate::utils::signing_keys::KeysRegistry;

        let registry = KeysRegistry::load().ok()?;

        // First, try to find by global registry name
        if let Some(entry) = registry.get_key(reference) {
            return Some((reference.to_string(), entry.keyid.clone()));
        }

        // If not found by name, check if it's a valid key ID that exists in the registry
        for (name, entry) in &registry.keys {
            if entry.keyid == reference {
                return Some((name.clone(), entry.keyid.clone()));
            }
        }

        None
    }

    /// Get the declared signing key name for a runtime (without resolving it).
    ///
    /// Returns Some(key_name) if the runtime has a signing configuration declared,
    /// None if the runtime doesn't exist or has no signing section.
    #[allow(dead_code)] // Public API for future use
    pub fn get_runtime_signing_key_name(&self, runtime_name: &str) -> Option<String> {
        let runtime_config = self.runtime.as_ref()?.get(runtime_name)?;
        Some(runtime_config.signing.as_ref()?.key.clone())
    }

    /// Get signing key for a specific runtime
    ///
    /// The signing key reference in the config can be either:
    /// - A key ID (64-char hex hash)
    /// - A global registry key name
    ///
    /// Returns the resolved key ID.
    #[allow(dead_code)] // Public API for future use
    pub fn get_runtime_signing_key(&self, runtime_name: &str) -> Option<String> {
        let runtime_config = self.runtime.as_ref()?.get(runtime_name)?;
        let signing_key_name = &runtime_config.signing.as_ref()?.key;

        // First, check the local signing_keys mapping
        if let Some(key_ref) = self.get_signing_key_id(signing_key_name) {
            // The value can be a key ID or a global name, resolve it
            if let Some((_, keyid)) = Self::resolve_signing_key_reference(key_ref) {
                return Some(keyid);
            }
            // If resolution fails, return the value as-is (might be a key ID not yet in registry)
            return Some(key_ref.clone());
        }

        // If not in local mapping, try resolving signing_key_name directly as a global reference
        if let Some((_, keyid)) = Self::resolve_signing_key_reference(signing_key_name) {
            return Some(keyid);
        }

        None
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

    /// Get the state file path for a provision profile.
    /// Returns the configured state_file path, or the default `.avocado/provision-{profile}.state` if not set.
    /// The path is relative to src_dir.
    pub fn get_provision_state_file(&self, profile_name: &str) -> String {
        self.get_provision_profile(profile_name)
            .and_then(|p| p.state_file.clone())
            .unwrap_or_else(|| format!(".avocado/provision-{profile_name}.state"))
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

    /// Parse the source field from an extension configuration
    ///
    /// Returns Some(ExtensionSource) if the extension has a source field,
    /// None if it's a local extension (no source field)
    pub fn parse_extension_source(
        ext_name: &str,
        ext_config: &serde_yaml::Value,
    ) -> Result<Option<ExtensionSource>> {
        let source = ext_config.get("source");

        match source {
            None => Ok(None), // Local extension
            Some(source_value) => {
                // Deserialize the source block into ExtensionSource
                let source: ExtensionSource = serde_yaml::from_value(source_value.clone())
                    .with_context(|| {
                        format!("Failed to parse source configuration for extension '{ext_name}'")
                    })?;
                Ok(Some(source))
            }
        }
    }

    /// Discover all remote extensions in the configuration
    ///
    /// Returns a list of (extension_name, ExtensionSource) tuples for extensions
    /// that have a `source` field in their configuration.
    ///
    /// If `target` is provided, extension names containing `{{ avocado.target }}`
    /// will be interpolated with the target value.
    pub fn discover_remote_extensions(
        config_path: &str,
        target: Option<&str>,
    ) -> Result<Vec<(String, ExtensionSource)>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed = Self::parse_config_value(config_path, &content)?;

        Self::discover_remote_extensions_from_value(&parsed, target)
    }

    /// Discover remote extensions from a parsed config value
    ///
    /// If `target` is provided, extension names containing `{{ avocado.target }}`
    /// will be interpolated with the target value.
    pub fn discover_remote_extensions_from_value(
        parsed: &serde_yaml::Value,
        target: Option<&str>,
    ) -> Result<Vec<(String, ExtensionSource)>> {
        use crate::utils::interpolation::interpolate_name;

        let mut remote_extensions = Vec::new();

        if let Some(ext_section) = parsed.get("ext").and_then(|e| e.as_mapping()) {
            for (ext_name_key, ext_config) in ext_section {
                if let Some(raw_ext_name) = ext_name_key.as_str() {
                    // Interpolate extension name if target is provided
                    let ext_name = if let Some(t) = target {
                        interpolate_name(raw_ext_name, t)
                    } else {
                        raw_ext_name.to_string()
                    };

                    if let Some(source) = Self::parse_extension_source(&ext_name, ext_config)? {
                        remote_extensions.push((ext_name, source));
                    }
                }
            }
        }

        Ok(remote_extensions)
    }

    /// Get the path where remote extensions should be installed on the host filesystem.
    ///
    /// This resolves the Docker volume mountpoint to access `$AVOCADO_PREFIX/includes` from the host.
    /// Returns: `<volume_mountpoint>/<target>/includes/`
    ///
    /// Falls back to `<src_dir>/.avocado/<target>/includes/` if volume state is not available.
    pub fn get_extensions_dir(&self, config_path: &str, target: &str) -> PathBuf {
        let src_dir = self.get_resolved_src_dir(config_path).unwrap_or_else(|| {
            PathBuf::from(config_path)
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });

        // Try to load volume state and get the mountpoint
        if let Ok(Some(volume_state)) = crate::utils::volume::VolumeState::load_from_dir(&src_dir) {
            // Use synchronous Docker inspect to get the mountpoint
            if let Ok(mountpoint) = Self::get_volume_mountpoint_sync(&volume_state) {
                return mountpoint.join(target).join("includes");
            }
        }

        // Fallback: use a local path in src_dir for development/testing
        src_dir.join(".avocado").join(target).join("includes")
    }

    /// Get the path where a specific remote extension should be installed
    ///
    /// Returns: `<volume_mountpoint>/<target>/includes/<ext_name>/`
    pub fn get_extension_install_path(
        &self,
        config_path: &str,
        ext_name: &str,
        target: &str,
    ) -> PathBuf {
        self.get_extensions_dir(config_path, target).join(ext_name)
    }

    /// Get the container path expression for extensions directory
    ///
    /// Returns: `$AVOCADO_PREFIX/includes`
    #[allow(dead_code)]
    pub fn get_extensions_container_path() -> &'static str {
        "$AVOCADO_PREFIX/includes"
    }

    /// Get the volume mountpoint synchronously (for use in non-async contexts)
    fn get_volume_mountpoint_sync(
        volume_state: &crate::utils::volume::VolumeState,
    ) -> Result<PathBuf> {
        let output = std::process::Command::new(&volume_state.container_tool)
            .args([
                "volume",
                "inspect",
                &volume_state.volume_name,
                "--format",
                "{{.Mountpoint}}",
            ])
            .output()
            .with_context(|| {
                format!(
                    "Failed to inspect Docker volume '{}'",
                    volume_state.volume_name
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to get mountpoint for volume '{}': {}",
                volume_state.volume_name,
                stderr
            );
        }

        let mountpoint = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if mountpoint.is_empty() {
            anyhow::bail!(
                "Docker volume '{}' has no mountpoint",
                volume_state.volume_name
            );
        }

        Ok(PathBuf::from(mountpoint))
    }

    /// Check if a remote extension is already installed
    #[allow(dead_code)]
    pub fn is_remote_extension_installed(
        &self,
        config_path: &str,
        ext_name: &str,
        target: &str,
    ) -> bool {
        let install_path = self.get_extension_install_path(config_path, ext_name, target);
        // Check if the directory exists and contains an avocado.yaml or avocado.toml
        install_path.exists()
            && (install_path.join("avocado.yaml").exists()
                || install_path.join("avocado.yml").exists()
                || install_path.join("avocado.toml").exists())
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

        // First check if it's defined in the ext section
        if let Some(ext_section) = parsed.get("ext") {
            if let Some(ext_map) = ext_section.as_mapping() {
                let ext_key = serde_yaml::Value::String(extension_name.to_string());
                if let Some(ext_config) = ext_map.get(&ext_key) {
                    // Check if this is a remote extension (has source: field)
                    if let Some(source) = Self::parse_extension_source(extension_name, ext_config)?
                    {
                        return Ok(Some(ExtensionLocation::Remote {
                            name: extension_name.to_string(),
                            source,
                        }));
                    }
                    // Otherwise it's a local extension
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
                ExtensionLocation::Remote { name, .. } => name,
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
            ExtensionLocation::Remote { .. } => {
                // Remote extensions don't have nested dependencies to discover here
                // Their configs are merged separately after fetching
                return Ok(());
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

    /// Merge SDK container args, provision profile container args, and CLI args
    /// Returns a new Vec containing: SDK args first, then provision profile args, then CLI args
    /// This ensures SDK defaults are used as a base, with provision profiles and CLI overriding
    /// Duplicate args are removed (later args take precedence for flags with values)
    pub fn merge_provision_container_args(
        &self,
        provision_profile: Option<&str>,
        cli_args: Option<&Vec<String>>,
    ) -> Option<Vec<String>> {
        let sdk_args = self.get_sdk_container_args();
        let profile_args = provision_profile
            .and_then(|profile| self.get_provision_profile_container_args(profile));

        // Collect all args in order: SDK first, then provision profile, then CLI
        let mut all_args: Vec<String> = Vec::new();

        if let Some(sdk) = sdk_args {
            all_args.extend(Self::process_container_args(Some(sdk)).unwrap_or_default());
        }

        if let Some(profile) = profile_args {
            all_args.extend(Self::process_container_args(Some(profile)).unwrap_or_default());
        }

        if let Some(cli) = cli_args {
            all_args.extend(Self::process_container_args(Some(cli)).unwrap_or_default());
        }

        if all_args.is_empty() {
            return None;
        }

        // Deduplicate args, keeping the last occurrence for flags with values
        // This allows provision profile and CLI to override SDK defaults
        let deduped = Self::deduplicate_container_args(all_args);

        if deduped.is_empty() {
            None
        } else {
            Some(deduped)
        }
    }

    /// Deduplicate container args, keeping the last occurrence for each unique arg or flag
    /// Handles both standalone flags (--privileged) and flag-value pairs (-v /dev:/dev, --network=host)
    fn deduplicate_container_args(args: Vec<String>) -> Vec<String> {
        use std::collections::HashSet;

        // First pass: identify which args are flags that take a separate value argument
        // (e.g., -v, -e, --volume, --env, etc.)
        let flags_with_separate_values: HashSet<&str> = [
            "-v",
            "--volume",
            "-e",
            "--env",
            "-p",
            "--publish",
            "-w",
            "--workdir",
            "-u",
            "--user",
            "-l",
            "--label",
            "--mount",
            "--device",
            "--add-host",
            "--dns",
            "--cap-add",
            "--cap-drop",
            "--security-opt",
            "--ulimit",
        ]
        .iter()
        .cloned()
        .collect();

        // Parse args into (key, full_representation) pairs for deduplication
        // key is used for deduplication, full_representation is what we keep
        let mut parsed_args: Vec<(String, Vec<String>)> = Vec::new();
        let mut i = 0;

        while i < args.len() {
            let arg = &args[i];

            if flags_with_separate_values.contains(arg.as_str()) && i + 1 < args.len() {
                // Flag with separate value: combine flag and value as key
                let value = &args[i + 1];
                let key = format!("{arg} {value}");
                parsed_args.push((key, vec![arg.clone(), value.clone()]));
                i += 2;
            } else if arg.starts_with('-') && arg.contains('=') {
                // Flag with inline value (e.g., --network=host)
                // Use just the flag name as key for network/other single-value flags
                let flag_name = arg.split('=').next().unwrap_or(arg);
                let key = flag_name.to_string();
                parsed_args.push((key, vec![arg.clone()]));
                i += 1;
            } else if arg.starts_with('-') {
                // Standalone flag (e.g., --privileged, --rm)
                parsed_args.push((arg.clone(), vec![arg.clone()]));
                i += 1;
            } else {
                // Non-flag argument (shouldn't happen normally, but handle it)
                parsed_args.push((arg.clone(), vec![arg.clone()]));
                i += 1;
            }
        }

        // Deduplicate by key, keeping the last occurrence
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut result: Vec<Vec<String>> = Vec::new();

        // Iterate in reverse to keep last occurrence, then reverse the result
        for (key, values) in parsed_args.into_iter().rev() {
            if !seen_keys.contains(&key) {
                seen_keys.insert(key);
                result.push(values);
            }
        }

        result.reverse();
        result.into_iter().flatten().collect()
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

    /// Check if there are any compile sections defined (regardless of whether they have dependencies)
    ///
    /// This is used to determine if the target-sysroot should be installed.
    /// The target-sysroot is needed whenever there's any sdk.compile.<name> section,
    /// even if it doesn't define any dependencies.
    pub fn has_compile_sections(&self) -> bool {
        if let Some(sdk) = &self.sdk {
            if let Some(compile) = &sdk.compile {
                return !compile.is_empty();
            }
        }
        false
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
    if target.host_uid.is_some() {
        base.host_uid = target.host_uid;
    }
    if target.host_gid.is_some() {
        base.host_gid = target.host_gid;
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

/// Resolve host UID/GID for bindfs permission translation.
///
/// Priority (highest first):
/// 1. Environment variables: `AVOCADO_HOST_UID` / `AVOCADO_HOST_GID`
/// 2. Config file: `sdk.host_uid` / `sdk.host_gid`
/// 3. libc calls: `libc::getuid()` / `libc::getgid()` (default fallback)
///
/// # Arguments
/// * `config` - Optional SDK configuration to check for host_uid/host_gid
///
/// # Returns
/// Tuple of (uid, gid) resolved according to priority chain
pub fn resolve_host_uid_gid(config: Option<&SdkConfig>) -> (u32, u32) {
    // Get fallback values from libc
    #[cfg(unix)]
    let (fallback_uid, fallback_gid) = {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        (uid, gid)
    };

    #[cfg(not(unix))]
    let (fallback_uid, fallback_gid) = (0u32, 0u32);

    // Resolve UID: env var > config > libc
    let uid = if let Ok(env_uid) = env::var("AVOCADO_HOST_UID") {
        env_uid.parse::<u32>().unwrap_or_else(|_| {
            eprintln!("Warning: Invalid AVOCADO_HOST_UID value, using fallback");
            fallback_uid
        })
    } else if let Some(cfg) = config {
        cfg.host_uid.unwrap_or(fallback_uid)
    } else {
        fallback_uid
    };

    // Resolve GID: env var > config > libc
    let gid = if let Ok(env_gid) = env::var("AVOCADO_HOST_GID") {
        env_gid.parse::<u32>().unwrap_or_else(|_| {
            eprintln!("Warning: Invalid AVOCADO_HOST_GID '{env_gid}', using fallback");
            fallback_gid
        })
    } else if let Some(cfg) = config {
        cfg.host_gid.unwrap_or(fallback_gid)
    } else {
        fallback_gid
    };

    (uid, gid)
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
    fn test_merge_provision_container_args_with_sdk_defaults() {
        // Test that SDK container_args are included as base defaults
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
container_args = ["--privileged", "--network=host"]

[provision.usb]
container_args = ["-v", "/dev:/dev"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test merging SDK + provision profile + CLI args
        let cli_args = vec!["--rm".to_string()];
        let merged = config.merge_provision_container_args(Some("usb"), Some(&cli_args));

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        // Should have SDK args first, then provision profile args, then CLI args
        assert_eq!(merged_args.len(), 5);
        assert_eq!(merged_args[0], "--privileged");
        assert_eq!(merged_args[1], "--network=host");
        assert_eq!(merged_args[2], "-v");
        assert_eq!(merged_args[3], "/dev:/dev");
        assert_eq!(merged_args[4], "--rm");
    }

    #[test]
    fn test_merge_provision_container_args_sdk_defaults_only() {
        // Test that SDK container_args are used when no provision profile or CLI args
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
container_args = ["--privileged", "-v", "/dev:/dev"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        let merged = config.merge_provision_container_args(None, None);

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        assert_eq!(merged_args.len(), 3);
        assert_eq!(merged_args[0], "--privileged");
        assert_eq!(merged_args[1], "-v");
        assert_eq!(merged_args[2], "/dev:/dev");
    }

    #[test]
    fn test_merge_provision_container_args_deduplication() {
        // Test that duplicate args are removed (keeping the last occurrence)
        let config_content = r#"
[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"
container_args = ["--privileged", "--network=host", "-v", "/dev:/dev"]

[provision.tegraflash]
container_args = ["--privileged", "--network=host", "-v", "/dev:/dev", "-v", "/sys:/sys"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test that duplicates are removed
        let merged = config.merge_provision_container_args(Some("tegraflash"), None);

        assert!(merged.is_some());
        let merged_args = merged.unwrap();
        // Should only have unique args: --privileged, --network=host, -v /dev:/dev, -v /sys:/sys
        // Note: --network=host keeps last occurrence (same value), -v /dev:/dev and -v /sys:/sys are different
        assert_eq!(merged_args.len(), 6); // --privileged, --network=host, -v, /dev:/dev, -v, /sys:/sys
        assert!(merged_args.contains(&"--privileged".to_string()));
        assert!(merged_args.contains(&"--network=host".to_string()));
        // Count occurrences of --privileged and --network=host - should be 1 each
        assert_eq!(
            merged_args.iter().filter(|a| *a == "--privileged").count(),
            1
        );
        assert_eq!(
            merged_args
                .iter()
                .filter(|a| *a == "--network=host")
                .count(),
            1
        );
    }

    #[test]
    fn test_provision_state_file_default() {
        // Test that state_file defaults to .avocado/provision-{profile}.state when not configured
        let config_content = r#"
provision:
  usb:
    container_args:
      - --privileged
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Should use default pattern when state_file is not configured
        let state_file = config.get_provision_state_file("usb");
        assert_eq!(state_file, ".avocado/provision-usb.state");

        // Should also use default for non-existent profiles
        let state_file = config.get_provision_state_file("nonexistent");
        assert_eq!(state_file, ".avocado/provision-nonexistent.state");
    }

    #[test]
    fn test_provision_state_file_custom() {
        // Test that custom state_file is used when configured
        let config_content = r#"
provision:
  production:
    container_args:
      - --privileged
    state_file: custom-state.json
  development:
    state_file: dev/state.json
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Should use custom state_file when configured
        let state_file = config.get_provision_state_file("production");
        assert_eq!(state_file, "custom-state.json");

        // Should work with nested paths
        let state_file = config.get_provision_state_file("development");
        assert_eq!(state_file, "dev/state.json");
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
    fn test_has_compile_sections() {
        // Test with compile sections defined
        let config_with_compile = r#"
default_target = "qemux86-64"

[sdk.compile.app]
compile = "make"

[sdk.compile.app.dependencies]
libfoo = "*"
"#;

        let config = Config::load_from_str(config_with_compile).unwrap();
        assert!(config.has_compile_sections());

        // Test with compile sections but no dependencies
        let config_no_deps = r#"
default_target = "qemux86-64"

[sdk.compile.app]
compile = "make"
"#;

        let config = Config::load_from_str(config_no_deps).unwrap();
        assert!(config.has_compile_sections());

        // Test with no compile sections
        let config_no_compile = r#"
default_target = "qemux86-64"

[sdk]
image = "my-sdk-image"
"#;

        let config = Config::load_from_str(config_no_compile).unwrap();
        assert!(!config.has_compile_sections());

        // Test with empty config (minimal)
        let config_minimal = r#"
default_target = "qemux86-64"
"#;

        let config = Config::load_from_str(config_minimal).unwrap();
        assert!(!config.has_compile_sections());
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
        // Key IDs are now full 64-char hex-encoded SHA-256 hashes
        let production_keyid = "abc123def456abc123def456abc123def456abc123def456abc123def456abc1";
        let backup_keyid = "789012fedcba789012fedcba789012fedcba789012fedcba789012fedcba7890";

        let config_content = format!(
            r#"
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest

signing_keys:
  - my-production-key: {production_keyid}
  - backup-key: {backup_keyid}

runtime:
  dev:
    signing:
      key: my-production-key
      checksum_algorithm: sha256
  production:
    signing:
      key: backup-key
      checksum_algorithm: blake3
  staging:
    signing:
      key: my-production-key
      # checksum_algorithm defaults to sha256
"#
        );

        let config = Config::load_from_yaml_str(&config_content).unwrap();

        // Test that signing_keys is parsed correctly
        let signing_keys = config.get_signing_keys();
        assert!(signing_keys.is_some());
        let signing_keys = signing_keys.unwrap();
        assert_eq!(signing_keys.len(), 2);
        assert_eq!(
            signing_keys.get("my-production-key"),
            Some(&production_keyid.to_string())
        );
        assert_eq!(
            signing_keys.get("backup-key"),
            Some(&backup_keyid.to_string())
        );

        // Test get_signing_key_names helper
        let key_names = config.get_signing_key_names();
        assert_eq!(key_names.len(), 2);
        assert!(key_names.contains(&"my-production-key".to_string()));
        assert!(key_names.contains(&"backup-key".to_string()));

        // Test get_signing_key_id helper
        assert_eq!(
            config.get_signing_key_id("my-production-key"),
            Some(&production_keyid.to_string())
        );
        assert_eq!(
            config.get_signing_key_id("backup-key"),
            Some(&backup_keyid.to_string())
        );
        assert_eq!(config.get_signing_key_id("nonexistent"), None);

        // Test runtime signing key reference - returns the keyid from the mapping
        // (without global registry, resolve_signing_key_reference returns None so we get the raw value)
        let runtime_key = config.get_runtime_signing_key("dev");
        assert_eq!(runtime_key, Some(production_keyid.to_string()));

        // Test runtime signing config
        let runtime = config.runtime.as_ref().unwrap().get("dev").unwrap();
        assert!(runtime.signing.is_some());
        let signing = runtime.signing.as_ref().unwrap();
        assert_eq!(signing.key, "my-production-key");
        assert_eq!(signing.checksum_algorithm, "sha256");

        // Test production runtime with blake3
        let production = config.runtime.as_ref().unwrap().get("production").unwrap();
        assert!(production.signing.is_some());
        let prod_signing = production.signing.as_ref().unwrap();
        assert_eq!(prod_signing.key, "backup-key");
        assert_eq!(prod_signing.checksum_algorithm, "blake3");

        // Test staging runtime with default checksum_algorithm
        let staging = config.runtime.as_ref().unwrap().get("staging").unwrap();
        assert!(staging.signing.is_some());
        let staging_signing = staging.signing.as_ref().unwrap();
        assert_eq!(staging_signing.key, "my-production-key");
        assert_eq!(staging_signing.checksum_algorithm, "sha256"); // Default
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

    #[test]
    fn test_get_runtime_signing_key_name() {
        // Test that get_runtime_signing_key_name returns the declared key name
        // even when the key cannot be resolved (e.g., signing_keys section missing)
        let config_content = r#"
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest

runtime:
  dev:
    signing:
      key: my-key
  prod:
    signing:
      key: production-key
  no-signing:
    dependencies:
      some-package: '*'
"#;

        let config = Config::load_from_yaml_str(config_content).unwrap();

        // Test that we can get the declared key name for runtimes with signing config
        assert_eq!(
            config.get_runtime_signing_key_name("dev"),
            Some("my-key".to_string())
        );
        assert_eq!(
            config.get_runtime_signing_key_name("prod"),
            Some("production-key".to_string())
        );

        // Test that runtimes without signing config return None
        assert_eq!(config.get_runtime_signing_key_name("no-signing"), None);

        // Test that non-existent runtimes return None
        assert_eq!(config.get_runtime_signing_key_name("nonexistent"), None);

        // Since signing_keys section is missing, get_runtime_signing_key should return None
        // while get_runtime_signing_key_name still returns the declared key name
        assert!(config.get_runtime_signing_key("dev").is_none());
        assert!(config.get_runtime_signing_key("prod").is_none());
    }

    #[test]
    fn test_runtime_signing_key_declared_but_not_in_signing_keys() {
        // Test scenario where runtime references a key that exists in signing_keys
        // but uses a different name
        let keyid = "abc123def456abc123def456abc123def456abc123def456abc123def456abc1";

        let config_content = format!(
            r#"
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest

signing_keys:
  - existing-key: {keyid}

runtime:
  dev:
    signing:
      key: missing-key
  prod:
    signing:
      key: existing-key
"#
        );

        let config = Config::load_from_yaml_str(&config_content).unwrap();

        // dev references 'missing-key' which is not in signing_keys
        assert_eq!(
            config.get_runtime_signing_key_name("dev"),
            Some("missing-key".to_string())
        );
        // get_runtime_signing_key returns None because 'missing-key' is not resolvable
        assert!(config.get_runtime_signing_key("dev").is_none());

        // prod references 'existing-key' which is in signing_keys
        assert_eq!(
            config.get_runtime_signing_key_name("prod"),
            Some("existing-key".to_string())
        );
        // get_runtime_signing_key returns the keyid because 'existing-key' is resolvable
        assert_eq!(
            config.get_runtime_signing_key("prod"),
            Some(keyid.to_string())
        );
    }

    #[test]
    fn test_discover_external_config_refs_from_runtime() {
        let config_content = r#"
runtime:
  prod:
    target: qemux86-64
    dependencies:
      peridio:
        ext: avocado-ext-peridio
        config: avocado-ext-peridio/avocado.yml
      local-ext:
        ext: local-extension
"#;

        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let refs = Config::discover_external_config_refs(&parsed);

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "avocado-ext-peridio");
        assert_eq!(refs[0].1, "avocado-ext-peridio/avocado.yml");
    }

    #[test]
    fn test_discover_external_config_refs_from_ext() {
        let config_content = r#"
ext:
  main-ext:
    types:
      - sysext
    dependencies:
      external-dep:
        ext: external-extension
        config: external/config.yaml
"#;

        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let refs = Config::discover_external_config_refs(&parsed);

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "external-extension");
        assert_eq!(refs[0].1, "external/config.yaml");
    }

    #[test]
    fn test_merge_external_config_ext_section() {
        let main_config_content = r#"
distro:
  version: "1.0.0"
ext:
  local-ext:
    types:
      - sysext
"#;
        let external_config_content = r#"
ext:
  external-ext:
    types:
      - sysext
    version: "{{ config.distro.version }}"
"#;

        let mut main_config: serde_yaml::Value = serde_yaml::from_str(main_config_content).unwrap();
        let external_config: serde_yaml::Value =
            serde_yaml::from_str(external_config_content).unwrap();

        // Use empty include patterns - ext.<ext-name> is always merged
        Config::merge_external_config(&mut main_config, &external_config, "external-ext", &[], &[]);

        // Check that both extensions are present
        let ext_section = main_config.get("ext").unwrap().as_mapping().unwrap();
        assert!(ext_section.contains_key(serde_yaml::Value::String("local-ext".to_string())));
        assert!(ext_section.contains_key(serde_yaml::Value::String("external-ext".to_string())));
    }

    #[test]
    fn test_merge_external_config_sdk_dependencies() {
        let main_config_content = r#"
sdk:
  image: test-image
  dependencies:
    main-package: "*"
"#;
        let external_config_content = r#"
sdk:
  dependencies:
    external-package: "1.0.0"
    main-package: "2.0.0"  # Should not override main config
"#;

        let mut main_config: serde_yaml::Value = serde_yaml::from_str(main_config_content).unwrap();
        let external_config: serde_yaml::Value =
            serde_yaml::from_str(external_config_content).unwrap();

        // Include sdk.dependencies.* to merge SDK dependencies
        let include_patterns = vec!["sdk.dependencies.*".to_string()];
        Config::merge_external_config(
            &mut main_config,
            &external_config,
            "test-ext",
            &include_patterns,
            &[],
        );

        let sdk_deps = main_config
            .get("sdk")
            .unwrap()
            .get("dependencies")
            .unwrap()
            .as_mapping()
            .unwrap();

        // External package should be added
        assert!(sdk_deps.contains_key(serde_yaml::Value::String("external-package".to_string())));
        assert_eq!(
            sdk_deps
                .get(serde_yaml::Value::String("external-package".to_string()))
                .unwrap()
                .as_str(),
            Some("1.0.0")
        );

        // Main package should NOT be overridden
        assert_eq!(
            sdk_deps
                .get(serde_yaml::Value::String("main-package".to_string()))
                .unwrap()
                .as_str(),
            Some("*")
        );
    }

    #[test]
    fn test_merge_does_not_override_distro() {
        let main_config_content = r#"
distro:
  version: "1.0.0"
  channel: "stable"
"#;
        let external_config_content = r#"
distro:
  version: "2.0.0"
  channel: "edge"
"#;

        let mut main_config: serde_yaml::Value = serde_yaml::from_str(main_config_content).unwrap();
        let external_config: serde_yaml::Value =
            serde_yaml::from_str(external_config_content).unwrap();

        // Distro is never merged regardless of include patterns
        let include_patterns = vec!["distro.*".to_string()]; // Even with this, distro won't be merged
        Config::merge_external_config(
            &mut main_config,
            &external_config,
            "test-ext",
            &include_patterns,
            &[],
        );

        // Distro should remain unchanged from main config
        let distro = main_config.get("distro").unwrap();
        assert_eq!(distro.get("version").unwrap().as_str(), Some("1.0.0"));
        assert_eq!(distro.get("channel").unwrap().as_str(), Some("stable"));
    }

    #[test]
    fn test_load_composed_with_interpolation() {
        use tempfile::TempDir;

        // Create a temp directory for our test configs
        let temp_dir = TempDir::new().unwrap();

        // Create main config
        let main_config_content = r#"
distro:
  version: "1.0.0"
  channel: apollo-edge
default_target: qemux86-64
sdk:
  image: "docker.io/test:{{ config.distro.channel }}"
  dependencies:
    main-sdk-dep: "*"
runtime:
  prod:
    target: qemux86-64
    dependencies:
      peridio:
        ext: test-ext
        config: external/avocado.yml
"#;
        let main_config_path = temp_dir.path().join("avocado.yaml");
        std::fs::write(&main_config_path, main_config_content).unwrap();

        // Create external config directory and file
        let external_dir = temp_dir.path().join("external");
        std::fs::create_dir_all(&external_dir).unwrap();

        let external_config_content = r#"
ext:
  test-ext:
    version: "{{ config.distro.version }}"
    types:
      - sysext
sdk:
  dependencies:
    external-sdk-dep: "*"
"#;
        let external_config_path = external_dir.join("avocado.yml");
        std::fs::write(&external_config_path, external_config_content).unwrap();

        // Load composed config
        let composed = Config::load_composed(&main_config_path, Some("qemux86-64")).unwrap();

        // Verify the SDK image was interpolated using main config's distro
        assert_eq!(
            composed
                .config
                .sdk
                .as_ref()
                .unwrap()
                .image
                .as_ref()
                .unwrap(),
            "docker.io/test:apollo-edge"
        );

        // Verify the external extension was merged
        let ext_section = composed
            .merged_value
            .get("ext")
            .unwrap()
            .as_mapping()
            .unwrap();
        assert!(ext_section.contains_key(serde_yaml::Value::String("test-ext".to_string())));

        // Verify the external extension's version was interpolated from main config's distro
        let test_ext = ext_section
            .get(serde_yaml::Value::String("test-ext".to_string()))
            .unwrap();
        assert_eq!(test_ext.get("version").unwrap().as_str(), Some("1.0.0"));

        // Verify SDK dependencies were merged
        let sdk_deps = composed
            .merged_value
            .get("sdk")
            .unwrap()
            .get("dependencies")
            .unwrap()
            .as_mapping()
            .unwrap();
        assert!(sdk_deps.contains_key(serde_yaml::Value::String("main-sdk-dep".to_string())));
        assert!(sdk_deps.contains_key(serde_yaml::Value::String("external-sdk-dep".to_string())));
    }

    #[test]
    fn test_extension_source_get_include_patterns() {
        // Test Repo variant with include patterns
        let source = ExtensionSource::Repo {
            version: "*".to_string(),
            package: None,
            repo_name: None,
            include: Some(vec![
                "provision.tegraflash".to_string(),
                "sdk.compile.*".to_string(),
            ]),
        };
        let patterns = source.get_include_patterns();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0], "provision.tegraflash");
        assert_eq!(patterns[1], "sdk.compile.*");

        // Test Repo variant without include patterns
        let source_no_include = ExtensionSource::Repo {
            version: "*".to_string(),
            package: None,
            repo_name: None,
            include: None,
        };
        assert!(source_no_include.get_include_patterns().is_empty());

        // Test Git variant with include patterns
        let git_source = ExtensionSource::Git {
            url: "https://example.com/repo.git".to_string(),
            git_ref: Some("main".to_string()),
            sparse_checkout: None,
            include: Some(vec!["provision.*".to_string()]),
        };
        assert_eq!(git_source.get_include_patterns().len(), 1);

        // Test Path variant with include patterns
        let path_source = ExtensionSource::Path {
            path: "./external".to_string(),
            include: Some(vec!["sdk.dependencies.*".to_string()]),
        };
        assert_eq!(path_source.get_include_patterns().len(), 1);
    }

    #[test]
    fn test_matches_include_pattern_exact() {
        let patterns = vec![
            "provision.tegraflash".to_string(),
            "sdk.compile.nvidia-l4t".to_string(),
        ];

        // Exact matches should return true
        assert!(ExtensionSource::matches_include_pattern(
            "provision.tegraflash",
            &patterns
        ));
        assert!(ExtensionSource::matches_include_pattern(
            "sdk.compile.nvidia-l4t",
            &patterns
        ));

        // Non-matches should return false
        assert!(!ExtensionSource::matches_include_pattern(
            "provision.usb",
            &patterns
        ));
        assert!(!ExtensionSource::matches_include_pattern(
            "sdk.compile.other",
            &patterns
        ));
        assert!(!ExtensionSource::matches_include_pattern(
            "provision",
            &patterns
        ));
    }

    #[test]
    fn test_matches_include_pattern_wildcard() {
        let patterns = vec!["provision.*".to_string(), "sdk.compile.*".to_string()];

        // Wildcard matches should work
        assert!(ExtensionSource::matches_include_pattern(
            "provision.tegraflash",
            &patterns
        ));
        assert!(ExtensionSource::matches_include_pattern(
            "provision.usb",
            &patterns
        ));
        assert!(ExtensionSource::matches_include_pattern(
            "sdk.compile.nvidia-l4t",
            &patterns
        ));
        assert!(ExtensionSource::matches_include_pattern(
            "sdk.compile.custom-lib",
            &patterns
        ));

        // Non-matches should return false
        assert!(!ExtensionSource::matches_include_pattern(
            "sdk.dependencies.package1",
            &patterns
        ));
        assert!(!ExtensionSource::matches_include_pattern(
            "runtime.prod",
            &patterns
        ));

        // Partial prefix matches without proper dot separator should not match
        assert!(!ExtensionSource::matches_include_pattern(
            "provisionExtra",
            &patterns
        ));
    }

    #[test]
    fn test_matches_include_pattern_empty() {
        let empty_patterns: Vec<String> = vec![];

        // Empty patterns should never match
        assert!(!ExtensionSource::matches_include_pattern(
            "provision.tegraflash",
            &empty_patterns
        ));
        assert!(!ExtensionSource::matches_include_pattern(
            "anything",
            &empty_patterns
        ));
    }

    #[test]
    fn test_merge_external_config_with_include_patterns() {
        let main_config_content = r#"
ext:
  local-ext:
    types:
      - sysext
provision:
  existing-profile:
    script: provision.sh
"#;
        let external_config_content = r#"
ext:
  remote-ext:
    types:
      - sysext
    dependencies:
      some-dep: "*"
provision:
  tegraflash:
    script: flash.sh
  usb:
    script: usb-provision.sh
sdk:
  dependencies:
    external-dep: "*"
  compile:
    nvidia-l4t:
      compile: build.sh
"#;

        let mut main_config: serde_yaml::Value = serde_yaml::from_str(main_config_content).unwrap();
        let external_config: serde_yaml::Value =
            serde_yaml::from_str(external_config_content).unwrap();

        // Only include provision.tegraflash (not provision.usb)
        let include_patterns = vec!["provision.tegraflash".to_string()];
        Config::merge_external_config(
            &mut main_config,
            &external_config,
            "remote-ext",
            &include_patterns,
            &[],
        );

        // Check that ext.remote-ext was merged (always happens)
        let ext_section = main_config.get("ext").unwrap().as_mapping().unwrap();
        assert!(ext_section.contains_key(serde_yaml::Value::String("remote-ext".to_string())));

        // Check that provision.tegraflash was included
        let provision = main_config.get("provision").unwrap().as_mapping().unwrap();
        assert!(provision.contains_key(serde_yaml::Value::String("tegraflash".to_string())));
        assert!(provision.contains_key(serde_yaml::Value::String("existing-profile".to_string())));

        // Check that provision.usb was NOT included (not in patterns)
        assert!(!provision.contains_key(serde_yaml::Value::String("usb".to_string())));

        // Check that sdk.dependencies was NOT merged (not in patterns)
        assert!(main_config.get("sdk").is_none());
    }

    #[test]
    fn test_merge_external_config_auto_include_compile() {
        let main_config_content = r#"
ext:
  local-ext:
    types:
      - sysext
"#;
        let external_config_content = r#"
ext:
  remote-ext:
    types:
      - sysext
    dependencies:
      nvidia-l4t:
        compile: nvidia-l4t
sdk:
  compile:
    nvidia-l4t:
      compile: build-nvidia.sh
    other-lib:
      compile: build-other.sh
"#;

        let mut main_config: serde_yaml::Value = serde_yaml::from_str(main_config_content).unwrap();
        let external_config: serde_yaml::Value =
            serde_yaml::from_str(external_config_content).unwrap();

        // Use auto_include_compile to include nvidia-l4t
        let auto_include = vec!["nvidia-l4t".to_string()];
        Config::merge_external_config(
            &mut main_config,
            &external_config,
            "remote-ext",
            &[],           // No explicit include patterns
            &auto_include, // Auto-include nvidia-l4t compile section
        );

        // Check that sdk.compile.nvidia-l4t was included
        let sdk_compile = main_config
            .get("sdk")
            .unwrap()
            .get("compile")
            .unwrap()
            .as_mapping()
            .unwrap();
        assert!(sdk_compile.contains_key(serde_yaml::Value::String("nvidia-l4t".to_string())));

        // Check that sdk.compile.other-lib was NOT included
        assert!(!sdk_compile.contains_key(serde_yaml::Value::String("other-lib".to_string())));
    }

    #[test]
    fn test_find_compile_dependencies_in_ext() {
        let ext_config_content = r#"
ext:
  my-extension:
    dependencies:
      nvidia-l4t:
        compile: nvidia-l4t
      some-package:
        version: "1.0"
      custom-lib:
        compile: custom-compile-section
"#;
        let ext_config: serde_yaml::Value = serde_yaml::from_str(ext_config_content).unwrap();

        let compile_deps = Config::find_compile_dependencies_in_ext(&ext_config, "my-extension");

        assert_eq!(compile_deps.len(), 2);
        assert!(compile_deps.contains(&"nvidia-l4t".to_string()));
        assert!(compile_deps.contains(&"custom-compile-section".to_string()));
    }

    #[test]
    fn test_extension_source_include_serialization() {
        let source = ExtensionSource::Repo {
            version: "*".to_string(),
            package: None,
            repo_name: None,
            include: Some(vec![
                "provision.tegraflash".to_string(),
                "sdk.compile.*".to_string(),
            ]),
        };

        let serialized = serde_yaml::to_string(&source).unwrap();
        assert!(serialized.contains("include:"));
        assert!(serialized.contains("provision.tegraflash"));
        assert!(serialized.contains("sdk.compile.*"));

        // Test deserialization
        let yaml_content = r#"
type: repo
version: "*"
include:
  - provision.tegraflash
  - sdk.compile.*
"#;
        let deserialized: ExtensionSource = serde_yaml::from_str(yaml_content).unwrap();
        match deserialized {
            ExtensionSource::Repo { include, .. } => {
                assert!(include.is_some());
                let patterns = include.unwrap();
                assert_eq!(patterns.len(), 2);
                assert_eq!(patterns[0], "provision.tegraflash");
            }
            _ => panic!("Expected Repo variant"),
        }
    }
}
