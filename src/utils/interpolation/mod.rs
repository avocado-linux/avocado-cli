//! Configuration interpolation utilities for Avocado CLI.
//!
//! Provides template interpolation for YAML configuration files using {{ }} syntax.
//!
//! # Interpolation Contexts
//!
//! This module supports three interpolation contexts, each organized in its own sub-module:
//!
//! ## [`env`] - Environment Variables
//! ```yaml
//! value: "{{ env.MY_VAR }}"
//! ```
//! - Looks up variables from the caller's environment
//! - Outputs a warning if variable is not set
//! - Replaces with empty string when missing
//!
//! ## [`config`] - Config Self-References
//! ```yaml
//! base: "value"
//! derived: "{{ config.base }}"
//! nested: "{{ config.some.deep.path }}"
//! distro:
//!   channel: apollo-edge
//! sdk:
//!   image: "docker.io/avocadolinux/sdk:{{ config.distro.channel }}"
//! ```
//! - Navigates the YAML tree using dot notation
//! - Returns an error if path doesn't exist (fatal)
//! - Converts non-string values to strings
//!
//! ## [`avocado`] - Computed Internal Values
//! ```yaml
//! target_pkg: "pkg-{{ avocado.target }}"
//! ```
//! - Provides access to computed values like target architecture
//! - Leaves template as-is if value unavailable
//! - Never produces errors (CLI handles validation)
//!
//! # Features
//!
//! - **Nested interpolation**: Templates can produce templates
//! - **Recursive resolution**: Multiple passes until stable
//! - **Circular detection**: Max 100 iterations
//! - **Multiple templates**: Multiple templates in single value

use anyhow::Result;
use regex::Regex;
use serde_yaml::Value;
use std::collections::HashSet;

pub mod avocado;
pub mod config;
pub mod env;

const MAX_ITERATIONS: usize = 100;

/// Interpolate configuration values in a YAML structure.
///
/// This function recursively walks the YAML structure and replaces template strings
/// with their resolved values. It supports nested interpolation and will continue
/// iterating until no more templates can be resolved.
///
/// # Arguments
/// * `yaml_value` - The YAML value to interpolate (modified in place)
/// * `cli_target` - Optional CLI target value for avocado.target interpolation
///
/// # Returns
/// Result indicating success or error if config references cannot be resolved
///
/// # Examples
/// ```
/// # use avocado_cli::utils::interpolation::interpolate_config;
/// let mut config = serde_yaml::from_str(r#"
/// base: "value"
/// derived: "{{ config.base }}"
/// "#).unwrap();
///
/// interpolate_config(&mut config, None).unwrap();
/// assert_eq!(config.get("derived").unwrap().as_str().unwrap(), "value");
/// ```
pub fn interpolate_config(yaml_value: &mut Value, cli_target: Option<&str>) -> Result<()> {
    let mut iteration = 0;
    let mut changed = true;
    let mut previous_states: Vec<String> = Vec::new();

    // Keep iterating until no more changes or we hit the iteration limit
    while changed && iteration < MAX_ITERATIONS {
        // Serialize current state to detect cycles
        let current_state = serde_yaml::to_string(yaml_value)?;

        // Check if we've seen this exact state before (cycle detection)
        if previous_states.contains(&current_state) {
            // Find which templates are stuck in a cycle
            anyhow::bail!(
                "Circular reference detected: configuration contains templates that reference each other in a cycle. \
                 This typically happens when config values reference each other (e.g., a: '{{{{ config.b }}}}', b: '{{{{ config.a }}}}')"
            );
        }

        previous_states.push(current_state);

        // Clone the value to use as root for lookups
        let root = yaml_value.clone();
        // Create a new resolving stack for each iteration
        let mut resolving_stack = HashSet::new();
        changed = interpolate_value(yaml_value, &root, cli_target, &mut resolving_stack)?;
        iteration += 1;
    }

    if iteration >= MAX_ITERATIONS {
        anyhow::bail!(
            "Maximum interpolation iterations ({MAX_ITERATIONS}) exceeded. Possible circular reference detected."
        );
    }

    Ok(())
}

/// Recursively interpolate a single value.
///
/// # Arguments
/// * `value` - The current value to interpolate
/// * `root` - The root YAML value for config references
/// * `cli_target` - Optional CLI target value
/// * `resolving_stack` - Set of templates currently being resolved (for cycle detection)
///
/// # Returns
/// Result with a boolean indicating if any changes were made
fn interpolate_value(
    value: &mut Value,
    root: &Value,
    cli_target: Option<&str>,
    resolving_stack: &mut HashSet<String>,
) -> Result<bool> {
    let mut changed = false;

    match value {
        Value::String(s) => {
            if let Some(new_value) = interpolate_string(s, root, cli_target, resolving_stack)? {
                *s = new_value;
                changed = true;
            }
        }
        Value::Mapping(map) => {
            for (_, v) in map.iter_mut() {
                if interpolate_value(v, root, cli_target, resolving_stack)? {
                    changed = true;
                }
            }
        }
        Value::Sequence(seq) => {
            for item in seq.iter_mut() {
                if interpolate_value(item, root, cli_target, resolving_stack)? {
                    changed = true;
                }
            }
        }
        _ => {
            // Other types (numbers, bools, null) don't need interpolation
        }
    }

    Ok(changed)
}

/// Interpolate a string value by replacing all template expressions.
///
/// # Arguments
/// * `input` - The input string that may contain templates
/// * `root` - The root YAML value for config references
/// * `cli_target` - Optional CLI target value
/// * `resolving_stack` - Set of templates currently being resolved (for cycle detection)
///
/// # Returns
/// Result with Option<String> - Some(new_string) if changes were made, None if no templates found
fn interpolate_string(
    input: &str,
    root: &Value,
    cli_target: Option<&str>,
    resolving_stack: &mut HashSet<String>,
) -> Result<Option<String>> {
    // Regex to match {{ ... }} templates
    let re = Regex::new(r"\{\{\s*([^}]+)\s*\}\}").unwrap();

    if !re.is_match(input) {
        return Ok(None);
    }

    let mut result = input.to_string();
    let mut any_replaced = false;

    // Find all matches and replace them
    for capture in re.captures_iter(input) {
        let full_match = capture.get(0).unwrap().as_str();
        let template = capture.get(1).unwrap().as_str().trim();

        if let Some(replacement) = resolve_template(template, root, cli_target, resolving_stack)? {
            result = result.replace(full_match, &replacement);
            any_replaced = true;
        }
    }

    if any_replaced {
        Ok(Some(result))
    } else {
        Ok(None)
    }
}

/// Resolve a single template expression by delegating to the appropriate context module.
///
/// # Arguments
/// * `template` - The template expression (e.g., "env.VAR" or "config.key")
/// * `root` - The root YAML value for config references
/// * `cli_target` - Optional CLI target value
/// * `resolving_stack` - Set of templates currently being resolved (for cycle detection)
///
/// # Returns
/// Result with Option<String> - Some(value) if resolved, None if should be left as-is
fn resolve_template(
    template: &str,
    root: &Value,
    cli_target: Option<&str>,
    resolving_stack: &mut HashSet<String>,
) -> Result<Option<String>> {
    // Check for circular reference
    if resolving_stack.contains(template) {
        anyhow::bail!(
            "Circular reference detected: template '{{{{ {template} }}}}' references itself. \
             Resolution chain: {}",
            resolving_stack
                .iter()
                .map(|t| format!("'{{{{ {t} }}}}'"))
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }

    // Add to resolving stack
    resolving_stack.insert(template.to_string());

    let parts: Vec<&str> = template.split('.').collect();

    if parts.is_empty() {
        resolving_stack.remove(template);
        anyhow::bail!("Invalid template syntax: empty template");
    }

    let context = parts[0];

    let result = match context {
        "env" => {
            if parts.len() < 2 {
                anyhow::bail!("Invalid env template: {template}");
            }
            let var_name = parts[1..].join(".");
            env::resolve(&var_name)
        }
        "config" => {
            if parts.len() < 2 {
                anyhow::bail!("Invalid config template: {template}");
            }
            let path = &parts[1..];
            config::resolve(root, path)
        }
        "avocado" => {
            if parts.len() < 2 {
                anyhow::bail!("Invalid avocado template: {template}");
            }
            let key = parts[1];
            avocado::resolve(key, root, cli_target)
        }
        _ => {
            anyhow::bail!(
                "Unknown template context: {context}. Expected 'env', 'config', or 'avocado'"
            );
        }
    };

    // Remove from resolving stack after resolution
    resolving_stack.remove(template);

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    fn parse_yaml(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_basic_env_interpolation() {
        env::set_var("TEST_VAR", "test_value");

        let mut config = parse_yaml(
            r#"
key: "{{ env.TEST_VAR }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(config.get("key").unwrap().as_str().unwrap(), "test_value");

        env::remove_var("TEST_VAR");
    }

    #[test]
    #[serial]
    fn test_missing_env_var() {
        env::remove_var("MISSING_VAR");

        let mut config = parse_yaml(
            r#"
key: "{{ env.MISSING_VAR }}"
"#,
        );

        // Should succeed but replace with empty string
        interpolate_config(&mut config, None).unwrap();

        assert_eq!(config.get("key").unwrap().as_str().unwrap(), "");
    }

    #[test]
    fn test_config_self_reference() {
        let mut config = parse_yaml(
            r#"
base: "base_value"
derived: "{{ config.base }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("derived").unwrap().as_str().unwrap(),
            "base_value"
        );
    }

    #[test]
    fn test_config_nested_path() {
        let mut config = parse_yaml(
            r#"
nested:
  deep:
    value: "deep_value"
reference: "{{ config.nested.deep.value }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("reference").unwrap().as_str().unwrap(),
            "deep_value"
        );
    }

    #[test]
    fn test_missing_config_path() {
        let mut config = parse_yaml(
            r#"
reference: "{{ config.nonexistent.path }}"
"#,
        );

        // Should return an error
        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    #[serial]
    fn test_avocado_target_from_cli() {
        let mut config = parse_yaml(
            r#"
target_ref: "{{ avocado.target }}"
"#,
        );

        interpolate_config(&mut config, Some("cli-target")).unwrap();

        assert_eq!(
            config.get("target_ref").unwrap().as_str().unwrap(),
            "cli-target"
        );
    }

    #[test]
    #[serial]
    fn test_avocado_target_from_env() {
        env::set_var("AVOCADO_TARGET", "env-target");

        let mut config = parse_yaml(
            r#"
target_ref: "{{ avocado.target }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("target_ref").unwrap().as_str().unwrap(),
            "env-target"
        );

        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_avocado_target_from_config() {
        env::remove_var("AVOCADO_TARGET");

        let mut config = parse_yaml(
            r#"
default_target: "config-target"
target_ref: "{{ avocado.target }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("target_ref").unwrap().as_str().unwrap(),
            "config-target"
        );
    }

    #[test]
    #[serial]
    fn test_avocado_target_unavailable() {
        env::remove_var("AVOCADO_TARGET");

        let mut config = parse_yaml(
            r#"
target_ref: "{{ avocado.target }}"
"#,
        );

        // Should succeed but leave template as-is
        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("target_ref").unwrap().as_str().unwrap(),
            "{{ avocado.target }}"
        );
    }

    #[test]
    #[serial]
    fn test_nested_interpolation() {
        // This test demonstrates that interpolation happens in multiple passes
        // If one interpolation creates a new template, it will be resolved in the next pass
        env::set_var("TEMPLATE", "{{ config.nested.value }}");

        let mut config = parse_yaml(
            r#"
nested:
  value: "final_value"
reference: "{{ env.TEMPLATE }}"
"#,
        );

        // First iteration resolves env.TEMPLATE to "{{ config.nested.value }}"
        // Second iteration resolves "{{ config.nested.value }}" to "final_value"
        interpolate_config(&mut config, None).unwrap();

        // Should be resolved to the final value through multiple passes
        assert_eq!(
            config.get("reference").unwrap().as_str().unwrap(),
            "final_value"
        );

        env::remove_var("TEMPLATE");
    }

    #[test]
    fn test_recursive_resolution() {
        let mut config = parse_yaml(
            r#"
a: "value_a"
b: "{{ config.a }}"
c: "{{ config.b }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(config.get("b").unwrap().as_str().unwrap(), "value_a");
        assert_eq!(config.get("c").unwrap().as_str().unwrap(), "value_a");
    }

    #[test]
    fn test_circular_reference_detection() {
        let mut config = parse_yaml(
            r#"
a: "{{ config.b }}"
b: "{{ config.a }}"
"#,
        );

        // Should error due to circular reference
        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("Circular reference") || error_msg.contains("circular"),
            "Expected circular reference error, got: {error_msg}"
        );
    }

    #[test]
    fn test_direct_self_reference() {
        let mut config = parse_yaml(
            r#"
a: "{{ config.a }}"
"#,
        );

        // Should error immediately on direct self-reference
        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        eprintln!("Direct self-reference error: {error_msg}");
        assert!(
            error_msg.contains("Circular reference") || error_msg.contains("circular"),
            "Expected circular reference error, got: {error_msg}"
        );
    }

    #[test]
    fn test_indirect_circular_reference() {
        let mut config = parse_yaml(
            r#"
a: "{{ config.b }}"
b: "{{ config.c }}"
c: "{{ config.a }}"
"#,
        );

        // Should error due to indirect circular reference
        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Circular reference"));
    }

    #[test]
    fn test_multiple_templates_in_string() {
        env::set_var("VAR1", "hello");
        env::set_var("VAR2", "world");

        let mut config = parse_yaml(
            r#"
message: "{{ env.VAR1 }}-{{ env.VAR2 }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(
            config.get("message").unwrap().as_str().unwrap(),
            "hello-world"
        );

        env::remove_var("VAR1");
        env::remove_var("VAR2");
    }

    #[test]
    fn test_whitespace_handling() {
        env::set_var("TRIMMED", "value");

        let mut config = parse_yaml(
            r#"
key: "{{   env.TRIMMED   }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(config.get("key").unwrap().as_str().unwrap(), "value");

        env::remove_var("TRIMMED");
    }

    #[test]
    fn test_complex_yaml_structures() {
        env::set_var("PKG_VERSION", "1.2.3");

        let mut config = parse_yaml(
            r#"
default_target: "x86_64"
runtime:
  dev:
    dependencies:
      pkg1: "{{ env.PKG_VERSION }}"
      pkg2: "{{ config.default_target }}"
    array:
      - "{{ env.PKG_VERSION }}"
      - "static_value"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        let runtime = config.get("runtime").unwrap();
        let dev = runtime.get("dev").unwrap();
        let deps = dev.get("dependencies").unwrap();

        assert_eq!(deps.get("pkg1").unwrap().as_str().unwrap(), "1.2.3");
        assert_eq!(deps.get("pkg2").unwrap().as_str().unwrap(), "x86_64");

        let array = dev.get("array").unwrap().as_sequence().unwrap();
        assert_eq!(array[0].as_str().unwrap(), "1.2.3");
        assert_eq!(array[1].as_str().unwrap(), "static_value");

        env::remove_var("PKG_VERSION");
    }

    #[test]
    fn test_number_to_string_conversion() {
        let mut config = parse_yaml(
            r#"
number: 42
boolean: true
reference_num: "{{ config.number }}"
reference_bool: "{{ config.boolean }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        assert_eq!(config.get("reference_num").unwrap().as_str().unwrap(), "42");
        assert_eq!(
            config.get("reference_bool").unwrap().as_str().unwrap(),
            "true"
        );
    }

    #[test]
    fn test_invalid_template_syntax() {
        let mut config = parse_yaml(
            r#"
key: "{{ }}"
"#,
        );

        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_context() {
        let mut config = parse_yaml(
            r#"
key: "{{ unknown.value }}"
"#,
        );

        let result = interpolate_config(&mut config, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown template context"));
    }

    #[test]
    fn test_config_distro_interpolation() {
        let mut config = parse_yaml(
            r#"
distro:
  channel: apollo-edge
  version: 0.1.0
sdk:
  image: "docker.io/avocadolinux/sdk:{{ config.distro.channel }}"
  dependencies:
    avocado-sdk-toolchain: "{{ config.distro.version }}"
"#,
        );

        interpolate_config(&mut config, None).unwrap();

        let sdk = config.get("sdk").unwrap();
        assert_eq!(
            sdk.get("image").unwrap().as_str().unwrap(),
            "docker.io/avocadolinux/sdk:apollo-edge"
        );

        let deps = sdk.get("dependencies").unwrap();
        assert_eq!(
            deps.get("avocado-sdk-toolchain").unwrap().as_str().unwrap(),
            "0.1.0"
        );
    }
}
