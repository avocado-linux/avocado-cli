//! Avocado computed values interpolation context.
//!
//! Provides interpolation for `{{ avocado.* }}` templates.
//!
//! **Available values:**
//! - `{{ avocado.target }}` - Resolved target architecture
//!
//! **Behavior:**
//! - Returns None if value is not available (leaves template as-is)
//! - Never produces errors - CLI will handle validation later
//! - Follows the same precedence as CLI: CLI args > env vars > config

use anyhow::Result;
use serde_yaml::Value;
use std::env;

/// Resolve an avocado computed value.
///
/// # Arguments
/// * `key` - The avocado key (e.g., "target")
/// * `root` - The root YAML value for fallback lookups
/// * `cli_target` - Optional CLI target value (highest priority)
///
/// # Returns
/// Result with Option<String> - Some(value) if available, None to leave template as-is
///
/// # Examples
/// ```
/// # use avocado_cli::utils::interpolation::avocado::resolve;
/// let yaml = serde_yaml::from_str("default_target: x86_64-unknown-linux-gnu").unwrap();
///
/// // With CLI target (highest priority)
/// let result = resolve("target", &yaml, Some("cli-target")).unwrap();
/// assert_eq!(result, Some("cli-target".to_string()));
///
/// // From config
/// let result = resolve("target", &yaml, None).unwrap();
/// assert_eq!(result, Some("x86_64-unknown-linux-gnu".to_string()));
/// ```
pub fn resolve(key: &str, root: &Value, cli_target: Option<&str>) -> Result<Option<String>> {
    match key {
        "target" => resolve_target(root, cli_target),
        _ => {
            // Other avocado keys are not yet supported, but don't error
            // Just leave the template as-is for future extension
            Ok(None)
        }
    }
}

/// Resolve the target architecture value.
///
/// Follows the same precedence order as the CLI:
/// 1. CLI argument (if provided)
/// 2. Environment variable (AVOCADO_TARGET)
/// 3. Config default_target
///
/// # Arguments
/// * `root` - The root YAML value
/// * `cli_target` - Optional CLI target value
///
/// # Returns
/// Result with Option<String> - Some(target) if available, None if not available
fn resolve_target(root: &Value, cli_target: Option<&str>) -> Result<Option<String>> {
    // 1. CLI target (highest priority)
    if let Some(target) = cli_target {
        return Ok(Some(target.to_string()));
    }

    // 2. Environment variable
    if let Ok(target) = env::var("AVOCADO_TARGET") {
        return Ok(Some(target));
    }

    // 3. Config default_target
    if let Some(default_target) = root.get("default_target") {
        if let Some(target_str) = default_target.as_str() {
            return Ok(Some(target_str.to_string()));
        }
    }

    // Target not available - leave template as-is
    // CLI will handle validation later
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
    fn test_resolve_target_from_cli() {
        let config = parse_yaml("default_target: config-target");
        let result = resolve("target", &config, Some("cli-target")).unwrap();
        assert_eq!(result, Some("cli-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_from_env() {
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = parse_yaml("default_target: config-target");
        let result = resolve("target", &config, None).unwrap();
        assert_eq!(result, Some("env-target".to_string()));
        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_resolve_target_from_config() {
        env::remove_var("AVOCADO_TARGET");
        let config = parse_yaml("default_target: config-target");
        let result = resolve("target", &config, None).unwrap();
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_unavailable() {
        env::remove_var("AVOCADO_TARGET");
        let config = parse_yaml("{}");
        let result = resolve("target", &config, None).unwrap();
        // Should return None (leave template as-is)
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_unknown_key() {
        let config = parse_yaml("{}");
        let result = resolve("unknown", &config, None).unwrap();
        // Should return None (not supported yet, but no error)
        assert_eq!(result, None);
    }
}
