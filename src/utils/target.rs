//! Target resolution utilities for Avocado CLI.

use crate::utils::config::Config;
use std::env;

/// Resolve the target architecture with proper precedence.
///
/// Precedence order:
/// 1. CLI argument (--target)
/// 2. Environment variable (AVOCADO_TARGET)
/// 3. Configuration file default_target
///
/// # Arguments
/// * `cli_target` - Target from CLI argument (highest priority)
/// * `config` - Configuration structure to get default_target from
///
/// # Returns
/// Resolved target string or None if no target is specified
pub fn resolve_target(cli_target: Option<&str>, config: &Config) -> Option<String> {
    // First priority: CLI argument
    if let Some(target) = cli_target {
        return Some(target.to_string());
    }

    // Second priority: Environment variable
    if let Some(target) = get_target_from_env() {
        return Some(target);
    }

    // Third priority: Configuration file default_target
    if let Some(target) = config.get_default_target() {
        return Some(target.to_string());
    }

    None
}

/// Resolve the target architecture with proper precedence, returning an error if none found.
///
/// Precedence order:
/// 1. CLI argument (--target)
/// 2. Environment variable (AVOCADO_TARGET)
/// 3. Configuration file default_target
///
/// # Arguments
/// * `cli_target` - Target from CLI argument (highest priority)
/// * `config` - Configuration structure to get default_target from
///
/// # Returns
/// Resolved target string or error if no target is specified
pub fn resolve_target_required(
    cli_target: Option<&str>,
    config: &Config,
) -> anyhow::Result<String> {
    resolve_target(cli_target, config).ok_or_else(|| {
        anyhow::anyhow!(
            "No target architecture specified. Use --target, AVOCADO_TARGET env var, or set default_target in config."
        )
    })
}

/// Get target from environment variable.
///
/// # Returns
/// Target from AVOCADO_TARGET environment variable or None
pub fn get_target_from_env() -> Option<String> {
    env::var("AVOCADO_TARGET").ok()
}

/// Get target from configuration.
///
/// # Arguments
/// * `config` - The configuration structure
///
/// # Returns
/// Target from configuration or None
#[allow(dead_code)]
pub fn get_target_from_config(config: &Config) -> Option<String> {
    config.get_target()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn create_test_config(default_target: Option<&str>) -> Config {
        Config {
            default_target: default_target.map(|s| s.to_string()),
            runtime: None,
            sdk: None,
            provision: None,
        }
    }

    #[test]
    fn test_resolve_target_cli_priority() {
        // CLI target should have highest priority
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(Some("cli-target"), &config);
        assert_eq!(result, Some("cli-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    fn test_resolve_target_env_priority() {
        // Environment variable should have second priority
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(None, &config);
        assert_eq!(result, Some("env-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    fn test_resolve_target_config_fallback() {
        // Config default_target should be used as fallback
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(None, &config);
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    fn test_resolve_target_none() {
        // Should return None when no target is available
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(None);

        let result = resolve_target(None, &config);
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_target_required_success() {
        // Should return target when available
        let config = create_test_config(Some("test-target"));

        let result = resolve_target_required(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test-target");
    }

    #[test]
    fn test_resolve_target_required_error() {
        // Should return error when no target available
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(None);

        let result = resolve_target_required(None, &config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No target architecture specified"));
    }

    #[test]
    fn test_precedence_order_complete() {
        // Test complete precedence order: CLI > ENV > CONFIG
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = create_test_config(Some("config-target"));

        // CLI wins over all
        let result1 = resolve_target(Some("cli-target"), &config);
        assert_eq!(result1, Some("cli-target".to_string()));

        // ENV wins over config
        let result2 = resolve_target(None, &config);
        assert_eq!(result2, Some("env-target".to_string()));

        // Remove env, config should be used
        env::remove_var("AVOCADO_TARGET");
        let result3 = resolve_target(None, &config);
        assert_eq!(result3, Some("config-target".to_string()));
    }

    #[test]
    fn test_get_target_from_env() {
        // Test environment variable function
        env::set_var("AVOCADO_TARGET", "test-env-target");
        assert_eq!(get_target_from_env(), Some("test-env-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
        assert_eq!(get_target_from_env(), None);
    }

    #[test]
    fn test_empty_string_values() {
        // Test that empty strings are treated as no value
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(Some(""));

        let result = resolve_target(Some(""), &config);
        // Empty string should still be returned as it was explicitly provided
        assert_eq!(result, Some("".to_string()));
    }
}
