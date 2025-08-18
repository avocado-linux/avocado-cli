//! Configuration utilities for Avocado CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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

/// Provision profile configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisionProfileConfig {
    pub container_args: Option<Vec<String>>,
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
    pub runtime: Option<HashMap<String, RuntimeConfig>>,
    pub sdk: Option<SdkConfig>,
    pub provision: Option<HashMap<String, ProvisionProfileConfig>>,
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
    /// Merged TOML value with target-specific overrides applied
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_section(
        &self,
        section_path: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<toml::Value>> {
        // Read the raw TOML to access target-specific sections
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed: toml::Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {config_path}"))?;

        // Get the base section
        let base_section = self.get_nested_section(&parsed, section_path);

        // Get the target-specific section
        let target_section_path = format!("{section_path}.{target}");
        let target_section = self.get_nested_section(&parsed, &target_section_path);

        // Merge the sections, but filter out target-specific keys from the base
        match (base_section, target_section) {
            (Some(base), Some(target_override)) => Ok(Some(
                self.merge_toml_values(base.clone(), target_override.clone()),
            )),
            (Some(base), None) => {
                // Filter out target-specific subsections from base before returning
                let supported_targets = self.get_supported_targets().unwrap_or_default();
                let filtered_base =
                    self.filter_target_subsections(base.clone(), &supported_targets);
                if filtered_base.as_table().is_some_and(|t| t.is_empty()) {
                    Ok(None)
                } else {
                    Ok(Some(filtered_base))
                }
            }
            (None, Some(target_override)) => Ok(Some(target_override.clone())),
            (None, None) => Ok(None),
        }
    }

    /// Helper function to get a nested section from TOML using dot notation
    #[allow(dead_code)] // Helper for merging system
    fn get_nested_section<'a>(&self, toml: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = toml;

        for part in parts {
            match current.get(part) {
                Some(value) => current = value,
                None => return None,
            }
        }

        Some(current)
    }

    /// Filter out target-specific subsections from a TOML value
    #[allow(dead_code)] // Helper for merging system
    fn filter_target_subsections(
        &self,
        mut value: toml::Value,
        supported_targets: &[String],
    ) -> toml::Value {
        if let toml::Value::Table(ref mut table) = value {
            // Remove any keys that match supported targets
            for target in supported_targets {
                table.remove(target);
            }
        }
        value
    }

    /// Merge two TOML values with the target value taking precedence
    #[allow(dead_code)] // Helper for merging system
    #[allow(clippy::only_used_in_recursion)] // Recursive merge function needs self parameter
    fn merge_toml_values(&self, mut base: toml::Value, target: toml::Value) -> toml::Value {
        match (&mut base, target) {
            // If both are tables, merge them recursively
            (toml::Value::Table(base_map), toml::Value::Table(target_map)) => {
                for (key, target_value) in target_map {
                    if let Some(base_value) = base_map.get_mut(&key) {
                        // Recursively merge if both are tables, otherwise override
                        *base_value = self.merge_toml_values(base_value.clone(), target_value);
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
    ) -> Result<Option<toml::Value>> {
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
    ) -> Result<Option<toml::Value>> {
        let section_path = format!("provision.{profile_name}");
        self.get_merged_section(&section_path, target, config_path)
    }

    /// Get merged extension configuration for a specific extension and target
    #[allow(dead_code)] // Future API for command integration
    pub fn get_merged_ext_config(
        &self,
        ext_name: &str,
        target: &str,
        config_path: &str,
    ) -> Result<Option<toml::Value>> {
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
    ) -> Result<Option<toml::Value>> {
        // Read the raw TOML to access target-specific sections
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed: toml::Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {config_path}"))?;

        // Get the base section: base_path.nested_path
        let base_section_path = format!("{base_path}.{nested_path}");
        let base_section = self.get_nested_section(&parsed, &base_section_path);

        // Get the target-specific section: base_path.target.nested_path
        let target_section_path = format!("{base_path}.{target}.{nested_path}");
        let target_section = self.get_nested_section(&parsed, &target_section_path);

        // Merge the sections
        match (base_section, target_section) {
            (Some(base), Some(target_override)) => Ok(Some(
                self.merge_toml_values(base.clone(), target_override.clone()),
            )),
            (Some(base), None) => Ok(Some(base.clone())),
            (None, Some(target_override)) => Ok(Some(target_override.clone())),
            (None, None) => Ok(None),
        }
    }

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

    /// Create a Config object from a TOML value
    pub fn from_toml_value(value: &toml::Value) -> Result<Self> {
        let config: Config = value
            .clone()
            .try_into()
            .with_context(|| "Failed to parse TOML value into Config")?;
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

    /// Load and parse external extension configuration from a config file
    /// Returns a map of extension name to extension configuration
    pub fn load_external_extensions<P: AsRef<Path>>(
        &self,
        config_path: P,
        external_config_path: &str,
    ) -> Result<HashMap<String, toml::Value>> {
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

        let parsed: toml::Value = toml::from_str(&content).with_context(|| {
            format!(
                "Failed to parse external config file: {}",
                resolved_path.display()
            )
        })?;

        let mut external_extensions = HashMap::new();

        // Find all [ext.*] sections in the external config
        if let Some(ext_section) = parsed.get("ext").and_then(|e| e.as_table()) {
            for (ext_name, ext_config) in ext_section {
                external_extensions.insert(ext_name.clone(), ext_config.clone());
            }
        }

        Ok(external_extensions)
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
        self.get_extension_sdk_dependencies_with_config_path(config_content, None)
    }

    /// Get extension SDK dependencies from configuration, including nested external extension dependencies
    /// Returns a HashMap where keys are extension names and values are their SDK dependencies
    pub fn get_extension_sdk_dependencies_with_config_path(
        &self,
        config_content: &str,
        config_path: Option<&str>,
    ) -> Result<HashMap<String, HashMap<String, toml::Value>>> {
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
    ) -> Result<HashMap<String, HashMap<String, toml::Value>>> {
        let parsed: toml::Value =
            toml::from_str(config_content).with_context(|| "Failed to parse TOML configuration")?;

        let mut extension_sdk_deps = HashMap::new();
        let mut visited = std::collections::HashSet::new();

        // Process local extensions in the current config
        if let Some(ext_section) = parsed.get("ext") {
            if let Some(ext_table) = ext_section.as_table() {
                for (ext_name, ext_config) in ext_table {
                    if let Some(ext_config_table) = ext_config.as_table() {
                        // Extract SDK dependencies for this extension (base and target-specific)
                        let mut merged_deps = HashMap::new();

                        // First, collect base SDK dependencies from [ext.<ext_name>.sdk.dependencies]
                        if let Some(sdk_section) = ext_config_table.get("sdk") {
                            if let Some(sdk_table) = sdk_section.as_table() {
                                if let Some(dependencies) = sdk_table.get("dependencies") {
                                    if let Some(deps_table) = dependencies.as_table() {
                                        for (k, v) in deps_table.iter() {
                                            merged_deps.insert(k.clone(), v.clone());
                                        }
                                    }
                                }
                            }
                        }

                        // Then, if we have a target, collect target-specific dependencies from [ext.<ext_name>.<target>.sdk.dependencies]
                        if let Some(target) = target {
                            if let Some(target_section) = ext_config_table.get(target) {
                                if let Some(target_table) = target_section.as_table() {
                                    if let Some(sdk_section) = target_table.get("sdk") {
                                        if let Some(sdk_table) = sdk_section.as_table() {
                                            if let Some(dependencies) =
                                                sdk_table.get("dependencies")
                                            {
                                                if let Some(deps_table) = dependencies.as_table() {
                                                    // Target-specific dependencies override base dependencies
                                                    for (k, v) in deps_table.iter() {
                                                        merged_deps.insert(k.clone(), v.clone());
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
                            extension_sdk_deps.insert(ext_name.clone(), merged_deps);
                        }

                        // If we have a config path, traverse external extension dependencies
                        if let Some(config_path) = config_path {
                            if let Some(dependencies) = ext_config_table.get("dependencies") {
                                if let Some(deps_table) = dependencies.as_table() {
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

        // Also process extensions referenced in runtime dependencies
        if let Some(config_path) = config_path {
            if let Some(runtime_section) = parsed.get("runtime") {
                if let Some(runtime_table) = runtime_section.as_table() {
                    for (_runtime_name, runtime_config) in runtime_table {
                        if let Some(runtime_config_table) = runtime_config.as_table() {
                            // Check base runtime dependencies
                            if let Some(dependencies) = runtime_config_table.get("dependencies") {
                                if let Some(deps_table) = dependencies.as_table() {
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
                                    if let Some(target_table) = target_section.as_table() {
                                        if let Some(dependencies) = target_table.get("dependencies")
                                        {
                                            if let Some(deps_table) = dependencies.as_table() {
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
        dependencies: &toml::Table,
        extension_sdk_deps: &mut HashMap<String, HashMap<String, toml::Value>>,
        visited: &mut std::collections::HashSet<String>,
        target: Option<&str>,
    ) -> Result<()> {
        for (_dep_name, dep_spec) in dependencies {
            if let Some(dep_spec_table) = dep_spec.as_table() {
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
                                match toml::from_str::<toml::Value>(&external_config_content) {
                                    Ok(external_parsed) => {
                                        // Create a temporary Config object for the external config
                                        if let Ok(external_config_obj) =
                                            Config::from_toml_value(&external_parsed)
                                        {
                                            // Only process the specific extension that's being referenced
                                            if let Some(ext_section) = external_parsed.get("ext") {
                                                if let Some(ext_table) = ext_section.as_table() {
                                                    if let Some(external_ext_config) =
                                                        ext_table.get(ext_name)
                                                    {
                                                        if let Some(external_ext_config_table) =
                                                            external_ext_config.as_table()
                                                        {
                                                            // Extract SDK dependencies for this specific external extension (base and target-specific)
                                                            let mut merged_deps = HashMap::new();

                                                            // First, collect base SDK dependencies from [ext.<ext_name>.sdk.dependencies]
                                                            if let Some(sdk_section) =
                                                                external_ext_config_table.get("sdk")
                                                            {
                                                                if let Some(sdk_table) =
                                                                    sdk_section.as_table()
                                                                {
                                                                    if let Some(dependencies) =
                                                                        sdk_table
                                                                            .get("dependencies")
                                                                    {
                                                                        if let Some(deps_table) =
                                                                            dependencies.as_table()
                                                                        {
                                                                            for (k, v) in
                                                                                deps_table.iter()
                                                                            {
                                                                                merged_deps.insert(
                                                                                    k.clone(),
                                                                                    v.clone(),
                                                                                );
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
                                                                        target_section.as_table()
                                                                    {
                                                                        if let Some(sdk_section) =
                                                                            target_table.get("sdk")
                                                                        {
                                                                            if let Some(sdk_table) =
                                                                                sdk_section
                                                                                    .as_table()
                                                                            {
                                                                                if let Some(
                                                                                    dependencies,
                                                                                ) = sdk_table
                                                                                    .get(
                                                                                    "dependencies",
                                                                                ) {
                                                                                    if let Some(deps_table) = dependencies.as_table() {
                                                                                        // Target-specific dependencies override base dependencies
                                                                                        for (k, v) in deps_table.iter() {
                                                                                            merged_deps.insert(k.clone(), v.clone());
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
                                                                    nested_dependencies.as_table()
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
    /// * `config_path` - Path to the configuration file for raw TOML access
    ///
    /// # Returns
    /// Merged SDK configuration or error if parsing fails
    pub fn get_merged_sdk_config(&self, target: &str, config_path: &str) -> Result<SdkConfig> {
        // Read the raw TOML to access target-specific sections
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed: toml::Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {config_path}"))?;

        // Start with the base SDK config
        let mut merged_config = self.sdk.clone().unwrap_or_default();

        // If there's a target-specific SDK section, merge it
        // First try the nested approach: [sdk] -> [qemux86-64]
        if let Some(sdk_section) = parsed.get("sdk") {
            if let Some(target_section) = sdk_section.get(target) {
                // Merge target-specific SDK configuration
                if let Ok(target_config) = target_section.clone().try_into::<SdkConfig>() {
                    merged_config = merge_sdk_configs(merged_config, target_config);
                }
            }
        }

        // Also try the top-level approach: [sdk.qemux86-64]
        let target_section_name = format!("sdk.{target}");
        if let Some(target_section) = parsed.get(&target_section_name) {
            // Merge target-specific SDK configuration
            if let Ok(target_config) = target_section.clone().try_into::<SdkConfig>() {
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
    /// * `config_path` - Path to the configuration file for raw TOML access
    ///
    /// # Returns
    /// Merged dependencies map or None if no dependencies are defined
    #[allow(dead_code)] // Future API for other SDK commands
    pub fn get_merged_sdk_dependencies(
        &self,
        target: &str,
        config_path: &str,
    ) -> Result<Option<HashMap<String, toml::Value>>> {
        // Read the raw TOML to access target-specific sections
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {config_path}"))?;
        let parsed: toml::Value = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {config_path}"))?;

        let mut merged_deps = HashMap::new();

        // First, add base SDK dependencies
        if let Some(sdk_section) = parsed.get("sdk") {
            if let Some(deps) = sdk_section.get("dependencies") {
                if let Some(deps_table) = deps.as_table() {
                    for (key, value) in deps_table {
                        merged_deps.insert(key.clone(), value.clone());
                    }
                }
            }

            // Then, add/override with target-specific dependencies
            if let Some(target_section) = sdk_section.get(target) {
                if let Some(target_deps) = target_section.get("dependencies") {
                    if let Some(target_deps_table) = target_deps.as_table() {
                        for (key, value) in target_deps_table {
                            merged_deps.insert(key.clone(), value.clone());
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
    fn test_src_dir_absolute_path() {
        let config_content = r#"
src_dir = "/absolute/path/to/source"

[sdk]
image = "avocadolinux/sdk:apollo-edge"
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
image = "avocadolinux/sdk:apollo-edge"
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
image = "avocadolinux/sdk:apollo-edge"
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
    fn test_extension_sdk_dependencies_with_target() {
        let config_content = r#"
[sdk]
image = "avocadolinux/sdk:apollo-edge"

[ext.avocado-dev]
types = ["sysext", "confext"]

[ext.avocado-dev.sdk.dependencies]
nativesdk-avocado-hitl = "*"
nativesdk-base-tool = "1.0.0"

[ext.avocado-dev.qemux86-64.sdk.dependencies]
nativesdk-avocado-hitl = "2.0.0"
nativesdk-target-specific = "*"

[ext.another-ext]
types = ["sysext"]

[ext.another-ext.sdk.dependencies]
nativesdk-tool = "*"

[ext.another-ext.qemuarm64.sdk.dependencies]
nativesdk-arm-tool = "*"
"#;

        let config = Config::load_from_str(config_content).unwrap();

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
[sdk]
image = "avocadolinux/sdk:apollo-edge"

[runtime.dev.dependencies]
avocado-ext-dev = { ext = "avocado-ext-dev", config = "extensions/dev/avocado.toml" }

[runtime.dev.raspberrypi4.dependencies]
avocado-bsp-raspberrypi4 = { ext = "avocado-bsp-raspberrypi4", config = "bsp/raspberrypi4/avocado.toml" }

[ext.config]
types = ["confext"]
"#;

        let config = Config::load_from_str(config_content).unwrap();

        // Test without config_path (should not find runtime dependencies)
        let extension_deps_no_config = config
            .get_extension_sdk_dependencies_with_config_path_and_target(config_content, None, None)
            .unwrap();

        // Should only find the local extension (config)
        assert_eq!(extension_deps_no_config.len(), 0);

        // Test with config_path (should find runtime dependencies, but we can't test file access in unit test)
        // This demonstrates the method signature and logic, actual file access would be tested in integration tests
        let result = config.get_extension_sdk_dependencies_with_config_path_and_target(
            config_content,
            Some("dummy_path"),
            None,
        );

        // Should not error (file access would fail silently in the implementation)
        assert!(result.is_ok());
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
image = "avocadolinux/sdk:apollo-edge"
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
image = "avocadolinux/sdk:apollo-edge"
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
        let sdk_x86_table = sdk_x86_value.as_table().unwrap();
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
        let sdk_arm64_table = sdk_arm64_value.as_table().unwrap();
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
        let provision_x86_table = provision_x86_value.as_table().unwrap();
        let args_x86 = provision_x86_table
            .get("container_args")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(args_x86[0].as_str().unwrap(), "--network=host");

        let provision_arm64 = config
            .get_merged_provision_config("usb", "qemuarm64", config_path)
            .unwrap();
        assert!(provision_arm64.is_some());
        let provision_arm64_value = provision_arm64.unwrap();
        let provision_arm64_table = provision_arm64_value.as_table().unwrap();
        let args_arm64 = provision_arm64_table
            .get("container_args")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(args_arm64[0].as_str().unwrap(), "--privileged");

        // Test runtime merging
        let runtime_x86 = config
            .get_merged_runtime_config("prod", "qemux86-64", config_path)
            .unwrap();
        assert!(runtime_x86.is_some());
        let runtime_x86_value = runtime_x86.unwrap();
        let runtime_x86_table = runtime_x86_value.as_table().unwrap();
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
        let runtime_arm64_table = runtime_arm64_value.as_table().unwrap();
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
        let deps_x86_table = deps_x86_value.as_table().unwrap();
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
        let deps_arm64_table = deps_arm64_value.as_table().unwrap();
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
        let users_x86_table = users_x86_value.as_table().unwrap();
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
        let users_arm64_table = users_arm64_value.as_table().unwrap();
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
        let runtime_arm64_table = runtime_arm64_value.as_table().unwrap();
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
        let ext_arm64_table = ext_arm64_value.as_table().unwrap();
        let types = ext_arm64_table.get("types").unwrap().as_array().unwrap();
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
        let prod_x86_table = prod_x86_value.as_table().unwrap();
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
                .as_integer()
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
        let prod_arm64_table = prod_arm64_value.as_table().unwrap();
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
                .as_integer()
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
        let dev_x86_table = dev_x86_value.as_table().unwrap();
        assert!(dev_x86_table.get("debug_mode").unwrap().as_bool().unwrap());
        assert!(dev_x86_table.get("cross_debug").is_none());

        let dev_arm64 = config
            .get_merged_runtime_config("development", "qemuarm64", config_path)
            .unwrap();
        assert!(dev_arm64.is_some());
        let dev_arm64_value = dev_arm64.unwrap();
        let dev_arm64_table = dev_arm64_value.as_table().unwrap();
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
        let usb_x86_table = usb_x86_value.as_table().unwrap();
        let args_x86 = usb_x86_table
            .get("container_args")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(args_x86.len(), 3);
        assert_eq!(args_x86[0].as_str().unwrap(), "--privileged");
        assert_eq!(
            usb_x86_table.get("timeout").unwrap().as_integer().unwrap(),
            300
        );
        assert!(usb_x86_table.get("emulation_mode").is_none());

        // Test USB provision for ARM64
        let usb_arm64 = config
            .get_merged_provision_config("usb", "qemuarm64", config_path)
            .unwrap();
        assert!(usb_arm64.is_some());
        let usb_arm64_value = usb_arm64.unwrap();
        let usb_arm64_table = usb_arm64_value.as_table().unwrap();
        let args_arm64 = usb_arm64_table
            .get("container_args")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(args_arm64.len(), 3); // Overridden container_args
        assert_eq!(args_arm64[0].as_str().unwrap(), "--cap-add=SYS_ADMIN");
        assert_eq!(
            usb_arm64_table
                .get("timeout")
                .unwrap()
                .as_integer()
                .unwrap(),
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
        let net_x86_table = net_x86_value.as_table().unwrap();
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
        let net_arm64_table = net_arm64_value.as_table().unwrap();
        assert_eq!(
            net_arm64_table.get("protocol").unwrap().as_str().unwrap(),
            "serial"
        ); // Overridden
        assert_eq!(
            net_arm64_table
                .get("baud_rate")
                .unwrap()
                .as_integer()
                .unwrap(),
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
        let ext_x86_table = ext_x86_value.as_table().unwrap();
        assert_eq!(
            ext_x86_table.get("version").unwrap().as_str().unwrap(),
            "1.0.0"
        );
        assert_eq!(
            ext_x86_table.get("overlay").unwrap().as_str().unwrap(),
            "overlays/avocado-dev"
        );

        let types_x86 = ext_x86_table.get("types").unwrap().as_array().unwrap();
        assert_eq!(types_x86.len(), 2);
        assert_eq!(types_x86[0].as_str().unwrap(), "sysext");

        // Test avocado-dev extension for ARM64
        let ext_arm64 = config
            .get_merged_ext_config("avocado-dev", "qemuarm64", config_path)
            .unwrap();
        assert!(ext_arm64.is_some());
        let ext_arm64_value = ext_arm64.unwrap();
        let ext_arm64_table = ext_arm64_value.as_table().unwrap();
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
        let deps_x86_table = deps_x86_value.as_table().unwrap();
        assert!(deps_x86_table.contains_key("openssh"));
        assert!(deps_x86_table.contains_key("nfs-utils"));
        assert!(!deps_x86_table.contains_key("gdb-multiarch"));

        let deps_arm64 = config
            .get_merged_nested_section("ext.avocado-dev", "dependencies", "qemuarm64", config_path)
            .unwrap();
        assert!(deps_arm64.is_some());
        let deps_arm64_value = deps_arm64.unwrap();
        let deps_arm64_table = deps_arm64_value.as_table().unwrap();
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
        let sdk_deps_x86_table = sdk_deps_x86_value.as_table().unwrap();
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
        let sdk_deps_arm64_table = sdk_deps_arm64_value.as_table().unwrap();
        assert!(sdk_deps_arm64_table.contains_key("nativesdk-openssh")); // Inherited
        assert!(sdk_deps_arm64_table.contains_key("nativesdk-gdb-cross-aarch64")); // Target-specific

        // Test users merging
        let users_root_x86 = config
            .get_merged_nested_section("ext.avocado-dev", "users.root", "qemux86-64", config_path)
            .unwrap();
        assert!(users_root_x86.is_some());
        let users_root_x86_value = users_root_x86.unwrap();
        let users_root_x86_table = users_root_x86_value.as_table().unwrap();
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
        let users_root_arm64_table = users_root_arm64_value.as_table().unwrap();
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
        let peridio_x86_table = peridio_x86_value.as_table().unwrap();
        let services_x86 = peridio_x86_table
            .get("enable_services")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(services_x86.len(), 1);

        let peridio_arm64 = config
            .get_merged_ext_config("peridio", "qemuarm64", config_path)
            .unwrap();
        assert!(peridio_arm64.is_some());
        let peridio_arm64_value = peridio_arm64.unwrap();
        let peridio_arm64_table = peridio_arm64_value.as_table().unwrap();
        let services_arm64 = peridio_arm64_table
            .get("enable_services")
            .unwrap()
            .as_array()
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
        let x86_table = x86_nested_value.as_table().unwrap();
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
        let arm64_table = arm64_nested_value.as_table().unwrap();
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
        let rpi_table = rpi_nested_value.as_table().unwrap();
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
        let sdk_arm64_table = sdk_arm64_value.as_table().unwrap();
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
}
