//! Config self-reference interpolation context.
//!
//! Provides interpolation for `{{ config.key.nested }}` templates.
//!
//! **Behavior:**
//! - Navigates the YAML configuration tree using dot notation
//! - Returns an error if the path doesn't exist (fatal error)
//! - Converts non-string values (numbers, bools) to strings

use anyhow::Result;
use serde_yaml::Value;

/// Resolve a config path reference.
///
/// # Arguments
/// * `root` - The root YAML value
/// * `path` - The path segments to navigate (e.g., ["nested", "key", "value"])
///
/// # Returns
/// Result with Option<String> - Some(value) if found, Error if path doesn't exist
///
/// # Examples
/// ```
/// # use avocado_cli::utils::interpolation::config::resolve;
/// let yaml = serde_yaml::from_str("base: value\nnested:\n  key: deep_value").unwrap();
/// let result = resolve(&yaml, &["base"]).unwrap();
/// assert_eq!(result, Some("value".to_string()));
///
/// let result = resolve(&yaml, &["nested", "key"]).unwrap();
/// assert_eq!(result, Some("deep_value".to_string()));
/// ```
pub fn resolve(root: &Value, path: &[&str]) -> Result<Option<String>> {
    let mut current = root;

    for segment in path {
        match current.get(segment) {
            Some(value) => current = value,
            None => {
                anyhow::bail!(
                    "Config path 'config.{}' not found in configuration",
                    path.join(".")
                );
            }
        }
    }

    // Convert the final value to a string
    Ok(Some(value_to_string(current)))
}

/// Convert a YAML value to a string representation.
///
/// # Arguments
/// * `value` - The YAML value to convert
///
/// # Returns
/// String representation of the value
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Sequence(_) | Value::Mapping(_) => {
            // For complex types, serialize to YAML-like string
            serde_yaml::to_string(value).unwrap_or_else(|_| String::new())
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_yaml(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_resolve_simple_path() {
        let config = parse_yaml("base: value");
        let result = resolve(&config, &["base"]).unwrap();
        assert_eq!(result, Some("value".to_string()));
    }

    #[test]
    fn test_resolve_nested_path() {
        let config = parse_yaml("nested:\n  deep:\n    value: deep_value");
        let result = resolve(&config, &["nested", "deep", "value"]).unwrap();
        assert_eq!(result, Some("deep_value".to_string()));
    }

    #[test]
    fn test_resolve_missing_path() {
        let config = parse_yaml("base: value");
        let result = resolve(&config, &["nonexistent"]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_number() {
        let config = parse_yaml("number: 42");
        let result = resolve(&config, &["number"]).unwrap();
        assert_eq!(result, Some("42".to_string()));
    }

    #[test]
    fn test_resolve_bool() {
        let config = parse_yaml("flag: true");
        let result = resolve(&config, &["flag"]).unwrap();
        assert_eq!(result, Some("true".to_string()));
    }
}
