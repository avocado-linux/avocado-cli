//! Target resolution utilities for Avocado CLI.

use crate::utils::config::Config;
use std::env;

/// Resolve the target architecture with proper precedence.
///
/// Precedence order:
/// 1. CLI argument (--target, -t)
/// 2. Environment variable (AVOCADO_TARGET)
/// 3. Configuration file target
///
/// # Arguments
/// * `cli_target` - Target from CLI argument (highest priority)
/// * `config_target` - Target from configuration file (lowest priority)
///
/// # Returns
/// Resolved target string or None if no target is specified
pub fn resolve_target(cli_target: Option<&str>, config_target: Option<&str>) -> Option<String> {
    // First priority: CLI argument
    if let Some(target) = cli_target {
        return Some(target.to_string());
    }

    // Second priority: Environment variable
    if let Some(target) = get_target_from_env() {
        return Some(target);
    }

    // Third priority: Configuration file
    if let Some(target) = config_target {
        return Some(target.to_string());
    }

    None
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

    #[test]
    fn test_resolve_target_cli_priority() {
        let result = resolve_target(Some("cli-target"), Some("config-target"));
        assert_eq!(result, Some("cli-target".to_string()));
    }

    #[test]
    fn test_resolve_target_env_priority() {
        env::set_var("AVOCADO_TARGET", "env-target");
        let result = resolve_target(None, Some("config-target"));
        assert_eq!(result, Some("env-target".to_string()));
        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    fn test_resolve_target_config_fallback() {
        env::remove_var("AVOCADO_TARGET");
        let result = resolve_target(None, Some("config-target"));
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    fn test_resolve_target_none() {
        env::remove_var("AVOCADO_TARGET");
        let result = resolve_target(None, None);
        assert_eq!(result, None);
    }

    #[test]
    fn test_get_target_from_env() {
        env::set_var("AVOCADO_TARGET", "test-target");
        assert_eq!(get_target_from_env(), Some("test-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
        assert_eq!(get_target_from_env(), None);
    }
}
