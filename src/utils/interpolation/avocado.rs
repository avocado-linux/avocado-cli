//! Avocado computed values interpolation context.
//!
//! Provides interpolation for `{{ avocado.* }}` templates.
//!
//! **Available values:**
//! - `{{ avocado.target }}` - Resolved target architecture
//! - `{{ avocado.distro.release }}` - Distro release (feed year) from main config
//! - `{{ avocado.distro.version }}` - Alias for distro.release (backward compat)
//! - `{{ avocado.distro.channel }}` - Distro channel from main config
//!
//! **Behavior:**
//! - Returns None if value is not available (leaves template as-is)
//! - Never produces errors - CLI will handle validation later
//! - Follows the same precedence as CLI: CLI args > env vars > config
//! - distro values come from the main config context, not the current config

use anyhow::Result;
use serde_yaml::Value;
use std::env;

/// Convert a YAML value to a string, handling numbers (e.g., `release: 2024` parsed as integer).
fn yaml_value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                i.to_string()
            } else if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                f.to_string()
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        _ => format!("{v:?}"),
    }
}

/// Context for avocado interpolation values.
///
/// This struct holds values that are set by the main config and should be
/// available to all subsequent configs during interpolation. This ensures
/// that `avocado.*` values always reference the main config's values,
/// while `config.*` values reference the current config being interpolated.
#[derive(Debug, Clone, Default)]
pub struct AvocadoContext {
    /// Target architecture (CLI > env > config precedence)
    pub target: Option<String>,
    /// Distro release (feed year) from the main config
    pub distro_release: Option<String>,
    /// Distro channel from the main config
    pub distro_channel: Option<String>,
}

impl AvocadoContext {
    /// Create a new empty context.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a context with just the target value.
    ///
    /// This is useful for simple interpolation cases where only target is needed.
    #[allow(dead_code)]
    pub fn with_target(target: Option<&str>) -> Self {
        Self {
            target: target.map(|s| s.to_string()),
            distro_release: None,
            distro_channel: None,
        }
    }

    /// Create a context from a main config YAML value.
    ///
    /// Extracts target (with CLI override and env precedence) and distro values
    /// from the config to be used for interpolation in all subsequent configs.
    ///
    /// # Arguments
    /// * `root` - The main config YAML value
    /// * `cli_target` - Optional CLI target override (highest priority)
    pub fn from_main_config(root: &Value, cli_target: Option<&str>) -> Self {
        // Resolve target with precedence: CLI > env > config
        let target = Self::resolve_target_value(root, cli_target);

        // Extract distro values from the main config
        let (distro_release, distro_channel) = Self::extract_distro_values(root);

        Self {
            target,
            distro_release,
            distro_channel,
        }
    }

    /// Resolve the target value with standard precedence.
    fn resolve_target_value(root: &Value, cli_target: Option<&str>) -> Option<String> {
        // 1. CLI target (highest priority)
        if let Some(target) = cli_target {
            return Some(target.to_string());
        }

        // 2. Environment variable
        if let Ok(target) = env::var("AVOCADO_TARGET") {
            return Some(target);
        }

        // 3. Config default_target
        if let Some(default_target) = root.get("default_target") {
            if let Some(target_str) = default_target.as_str() {
                return Some(target_str.to_string());
            }
        }

        None
    }

    /// Extract distro release and channel from the config.
    /// Reads `distro.release` with fallback to `distro.version` for backward compat.
    fn extract_distro_values(root: &Value) -> (Option<String>, Option<String>) {
        let distro = match root.get("distro") {
            Some(d) => d,
            None => return (None, None),
        };

        let release = distro
            .get("release")
            .or_else(|| distro.get("version"))
            .map(yaml_value_to_string);

        let channel = distro.get("channel").map(yaml_value_to_string);

        (release, channel)
    }

    /// Create a context with all values explicitly provided.
    ///
    /// This is useful when constructing from a deserialized Config struct.
    ///
    /// # Arguments
    /// * `target` - The resolved target (CLI > env > config precedence should be applied by caller)
    /// * `distro_release` - The distro release from the main config
    /// * `distro_channel` - The distro channel from the main config
    #[allow(dead_code)]
    pub fn with_values(
        target: Option<String>,
        distro_release: Option<String>,
        distro_channel: Option<String>,
    ) -> Self {
        Self {
            target,
            distro_release,
            distro_channel,
        }
    }
}

/// Resolve an avocado computed value using path segments.
///
/// # Arguments
/// * `path` - The avocado path segments (e.g., ["target"] or ["distro", "release"])
/// * `root` - The root YAML value for fallback lookups (used for target resolution)
/// * `context` - Optional avocado context with pre-resolved values from main config
///
/// # Returns
/// Result with Option<String> - Some(value) if available, None to leave template as-is
///
/// # Examples
/// ```
/// # use avocado_cli::utils::interpolation::avocado::{resolve, AvocadoContext};
/// let yaml = serde_yaml::from_str("default_target: x86_64-unknown-linux-gnu").unwrap();
///
/// // With context containing target
/// let ctx = AvocadoContext::with_target(Some("cli-target"));
/// let result = resolve(&["target"], &yaml, Some(&ctx)).unwrap();
/// assert_eq!(result, Some("cli-target".to_string()));
///
/// // With distro context
/// let ctx = AvocadoContext {
///     target: None,
///     distro_release: Some("2024".to_string()),
///     distro_channel: Some("edge".to_string()),
/// };
/// let result = resolve(&["distro", "release"], &yaml, Some(&ctx)).unwrap();
/// assert_eq!(result, Some("2024".to_string()));
/// ```
pub fn resolve(
    path: &[&str],
    root: &Value,
    context: Option<&AvocadoContext>,
) -> Result<Option<String>> {
    match path {
        ["target"] => resolve_target(root, context),
        ["distro", "release"] | ["distro", "version"] => resolve_distro_release(context),
        ["distro", "channel"] => resolve_distro_channel(context),
        _ => {
            // Other avocado keys are not yet supported, but don't error
            // Just leave the template as-is for future extension
            Ok(None)
        }
    }
}

/// Resolve the target architecture value.
///
/// Precedence:
/// 1. Context target (from CLI or previously resolved)
/// 2. Environment variable (AVOCADO_TARGET)
/// 3. Config default_target (from root - the current config)
fn resolve_target(root: &Value, context: Option<&AvocadoContext>) -> Result<Option<String>> {
    // 1. Context target (highest priority - from CLI or pre-resolved)
    if let Some(ctx) = context {
        if let Some(ref target) = ctx.target {
            return Ok(Some(target.clone()));
        }
    }

    // 2. Environment variable
    if let Ok(target) = env::var("AVOCADO_TARGET") {
        return Ok(Some(target));
    }

    // 3. Config default_target (from the current config being processed)
    if let Some(default_target) = root.get("default_target") {
        if let Some(target_str) = default_target.as_str() {
            return Ok(Some(target_str.to_string()));
        }
    }

    // Target not available - leave template as-is
    // CLI will handle validation later
    Ok(None)
}

/// Resolve the distro release from the avocado context.
///
/// This value comes from the main config and is passed through the context,
/// ensuring all configs use the same distro release.
/// Handles both `avocado.distro.release` and `avocado.distro.version` (alias).
fn resolve_distro_release(context: Option<&AvocadoContext>) -> Result<Option<String>> {
    if let Some(ctx) = context {
        if let Some(ref release) = ctx.distro_release {
            return Ok(Some(release.clone()));
        }
    }
    // Not available - leave template as-is
    Ok(None)
}

/// Resolve the distro channel from the avocado context.
///
/// This value comes from the main config and is passed through the context,
/// ensuring all configs use the same distro channel.
fn resolve_distro_channel(context: Option<&AvocadoContext>) -> Result<Option<String>> {
    if let Some(ctx) = context {
        if let Some(ref channel) = ctx.distro_channel {
            return Ok(Some(channel.clone()));
        }
    }
    // Not available - leave template as-is
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn parse_yaml(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    #[serial]
    fn test_resolve_target_from_context() {
        let config = parse_yaml("default_target: config-target");
        let ctx = AvocadoContext::with_target(Some("cli-target"));
        let result = resolve(&["target"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("cli-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_from_env() {
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = parse_yaml("default_target: config-target");
        let result = resolve(&["target"], &config, None).unwrap();
        assert_eq!(result, Some("env-target".to_string()));
        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_resolve_target_from_config() {
        env::remove_var("AVOCADO_TARGET");
        let config = parse_yaml("default_target: config-target");
        let result = resolve(&["target"], &config, None).unwrap();
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_unavailable() {
        env::remove_var("AVOCADO_TARGET");
        let config = parse_yaml("{}");
        let result = resolve(&["target"], &config, None).unwrap();
        // Should return None (leave template as-is)
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_unknown_path() {
        let config = parse_yaml("{}");
        let result = resolve(&["unknown"], &config, None).unwrap();
        // Should return None (not supported yet, but no error)
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_distro_release_from_context() {
        let config = parse_yaml("{}");
        let ctx = AvocadoContext {
            target: None,
            distro_release: Some("2024".to_string()),
            distro_channel: None,
        };
        let result = resolve(&["distro", "release"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("2024".to_string()));
    }

    #[test]
    fn test_resolve_distro_version_alias() {
        let config = parse_yaml("{}");
        let ctx = AvocadoContext {
            target: None,
            distro_release: Some("2024".to_string()),
            distro_channel: None,
        };
        // "distro.version" should resolve to the same value as "distro.release"
        let result = resolve(&["distro", "version"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("2024".to_string()));
    }

    #[test]
    fn test_resolve_distro_channel_from_context() {
        let config = parse_yaml("{}");
        let ctx = AvocadoContext {
            target: None,
            distro_release: None,
            distro_channel: Some("apollo-edge".to_string()),
        };
        let result = resolve(&["distro", "channel"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("apollo-edge".to_string()));
    }

    #[test]
    fn test_resolve_distro_without_context() {
        let config = parse_yaml("{}");
        // Without context, distro values should return None
        let result = resolve(&["distro", "release"], &config, None).unwrap();
        assert_eq!(result, None);

        let result = resolve(&["distro", "version"], &config, None).unwrap();
        assert_eq!(result, None);

        let result = resolve(&["distro", "channel"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_avocado_context_from_main_config_with_release() {
        let config = parse_yaml(
            r#"
default_target: x86_64-unknown-linux-gnu
distro:
  release: 2024
  channel: edge
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None);
        assert_eq!(ctx.target, Some("x86_64-unknown-linux-gnu".to_string()));
        assert_eq!(ctx.distro_release, Some("2024".to_string()));
        assert_eq!(ctx.distro_channel, Some("edge".to_string()));
    }

    #[test]
    fn test_avocado_context_from_main_config_with_version_fallback() {
        // Backward compat: "version" field should populate distro_release
        let config = parse_yaml(
            r#"
default_target: x86_64-unknown-linux-gnu
distro:
  version: 0.1.0
  channel: apollo-edge
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None);
        assert_eq!(ctx.target, Some("x86_64-unknown-linux-gnu".to_string()));
        assert_eq!(ctx.distro_release, Some("0.1.0".to_string()));
        assert_eq!(ctx.distro_channel, Some("apollo-edge".to_string()));
    }

    #[test]
    fn test_avocado_context_cli_overrides_config() {
        let config = parse_yaml(
            r#"
default_target: config-target
distro:
  release: 2024
  channel: edge
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, Some("cli-target"));
        assert_eq!(ctx.target, Some("cli-target".to_string()));
        assert_eq!(ctx.distro_release, Some("2024".to_string()));
        assert_eq!(ctx.distro_channel, Some("edge".to_string()));
    }

    #[test]
    fn test_avocado_context_missing_distro() {
        let config = parse_yaml("default_target: x86_64");
        let ctx = AvocadoContext::from_main_config(&config, None);
        assert_eq!(ctx.target, Some("x86_64".to_string()));
        assert_eq!(ctx.distro_release, None);
        assert_eq!(ctx.distro_channel, None);
    }
}
