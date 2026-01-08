//! Target resolution utilities for Avocado CLI.

use crate::utils::config::Config;
use crate::utils::output::{print_info, OutputLevel};
use std::env;

/// Source of target resolution
#[derive(Debug, Clone, PartialEq)]
pub enum TargetSource {
    /// Target came from CLI argument (--target)
    Cli,
    /// Target came from environment variable (AVOCADO_TARGET)
    Environment,
    /// Target came from configuration file (default_target)
    Config,
}

impl std::fmt::Display for TargetSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetSource::Cli => write!(f, "CLI argument (--target)"),
            TargetSource::Environment => write!(f, "environment variable (AVOCADO_TARGET)"),
            TargetSource::Config => write!(f, "config file (default_target)"),
        }
    }
}

/// Result of target resolution with source information
#[derive(Debug, Clone)]
pub struct TargetResolution {
    /// The resolved target architecture
    pub target: String,
    /// Source where the target was resolved from
    pub source: TargetSource,
}

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

/// Resolve the target architecture with source information and proper precedence.
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
/// TargetResolution with target and source information, or None if no target is specified
pub fn resolve_target_with_source(
    cli_target: Option<&str>,
    config: &Config,
) -> Option<TargetResolution> {
    // First priority: CLI argument
    if let Some(target) = cli_target {
        return Some(TargetResolution {
            target: target.to_string(),
            source: TargetSource::Cli,
        });
    }

    // Second priority: Environment variable
    if let Some(target) = get_target_from_env() {
        return Some(TargetResolution {
            target,
            source: TargetSource::Environment,
        });
    }

    // Third priority: Configuration file default_target
    if let Some(target) = config.get_default_target() {
        return Some(TargetResolution {
            target: target.to_string(),
            source: TargetSource::Config,
        });
    }

    None
}

/// Resolve and validate target with source information and early validation.
///
/// This function:
/// 1. Resolves the target using the normal precedence order
/// 2. Validates that the resolved target is in the supported_targets list
/// 3. Returns both target and source information
///
/// # Arguments
/// * `cli_target` - Target from CLI argument (highest priority)
/// * `config` - Configuration structure to get default_target and supported_targets from
///
/// # Returns
/// TargetResolution with validated target and source, or error if target is not found/supported
pub fn resolve_and_validate_target_with_source(
    cli_target: Option<&str>,
    config: &Config,
) -> anyhow::Result<TargetResolution> {
    // First resolve the target with source information
    let resolution = resolve_target_with_source(cli_target, config).ok_or_else(|| {
        anyhow::anyhow!(
            "No target architecture specified. Use --target, AVOCADO_TARGET env var, or set default_target in config."
        )
    })?;

    // Check if the target is supported
    if config.is_target_supported(&resolution.target) {
        Ok(resolution)
    } else {
        match config.get_supported_targets() {
            Some(supported_targets) => Err(anyhow::anyhow!(
                "Target '{}' is not supported by this configuration. Supported targets: {}",
                resolution.target,
                supported_targets.join(", ")
            )),
            None => Ok(resolution), // All targets supported
        }
    }
}

/// Get target from environment variable.
///
/// # Returns
/// Target from AVOCADO_TARGET environment variable or None
pub fn get_target_from_env() -> Option<String> {
    env::var("AVOCADO_TARGET").ok()
}

/// Perform early target validation and logging for commands.
///
/// This function should be called at the beginning of command execution to:
/// 1. Resolve and validate the target with proper precedence
/// 2. Log the resolved target and its source
/// 3. Fail early if the target is not supported
///
/// # Arguments
/// * `cli_target` - Target from CLI argument (highest priority)
/// * `config` - Configuration structure to get default_target and supported_targets from
///
/// # Returns
/// Validated target string or error if target is not found/supported
pub fn validate_and_log_target(
    cli_target: Option<&str>,
    config: &Config,
) -> anyhow::Result<String> {
    let resolution = resolve_and_validate_target_with_source(cli_target, config)?;

    print_info(
        &format!(
            "Using target: {} (from {})",
            resolution.target, resolution.source
        ),
        OutputLevel::Normal,
    );

    Ok(resolution.target)
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
    use serial_test::serial;
    use std::env;

    fn create_test_config(default_target: Option<&str>) -> Config {
        Config {
            default_target: default_target.map(|s| s.to_string()),
            supported_targets: None,
            src_dir: None,
            distro: None,
            runtimes: None,
            sdk: None,
            provision_profiles: None,
            signing_keys: None,
        }
    }

    fn create_config_with_supported_targets(targets: Vec<String>) -> Config {
        use crate::utils::config::SupportedTargets;
        Config {
            default_target: Some("qemux86-64".to_string()),
            supported_targets: Some(SupportedTargets::List(targets)),
            src_dir: None,
            distro: None,
            runtimes: None,
            sdk: None,
            provision_profiles: None,
            signing_keys: None,
        }
    }

    fn create_config_with_supported_targets_all() -> Config {
        use crate::utils::config::SupportedTargets;
        Config {
            default_target: Some("qemux86-64".to_string()),
            supported_targets: Some(SupportedTargets::All("*".to_string())),
            src_dir: None,
            distro: None,
            runtimes: None,
            sdk: None,
            provision_profiles: None,
            signing_keys: None,
        }
    }

    #[test]
    #[serial]
    fn test_resolve_target_cli_priority() {
        // CLI target should have highest priority
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(Some("cli-target"), &config);
        assert_eq!(result, Some("cli-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_resolve_target_env_priority() {
        // Environment variable should have second priority
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(None, &config);
        assert_eq!(result, Some("env-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_resolve_target_config_fallback() {
        // Config default_target should be used as fallback
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(Some("config-target"));

        let result = resolve_target(None, &config);
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_none() {
        // Should return None when no target is available
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(None);

        let result = resolve_target(None, &config);
        assert_eq!(result, None);
    }

    #[test]
    #[serial]
    fn test_resolve_target_required_success() {
        // Should return target when available
        let config = create_test_config(Some("test-target"));

        let result = resolve_target_required(None, &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test-target");
    }

    #[test]
    #[serial]
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
    #[serial]
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
    #[serial]
    fn test_get_target_from_env() {
        // Test environment variable function
        env::set_var("AVOCADO_TARGET", "test-env-target");
        assert_eq!(get_target_from_env(), Some("test-env-target".to_string()));

        env::remove_var("AVOCADO_TARGET");
        assert_eq!(get_target_from_env(), None);
    }

    #[test]
    #[serial]
    fn test_empty_string_values() {
        // Test that empty strings are treated as no value
        env::remove_var("AVOCADO_TARGET");
        let config = create_test_config(Some(""));

        let result = resolve_target(Some(""), &config);
        // Empty string should still be returned as it was explicitly provided
        assert_eq!(result, Some("".to_string()));
    }

    #[test]
    #[serial]
    fn test_supported_targets_list() {
        // Test with explicit list of supported targets
        let config = create_config_with_supported_targets(vec![
            "qemux86-64".to_string(),
            "qemuarm64".to_string(),
            "raspberrypi4".to_string(),
        ]);

        let supported_targets = config.get_supported_targets().unwrap();
        assert_eq!(supported_targets.len(), 3);
        assert!(supported_targets.contains(&"qemux86-64".to_string()));
        assert!(supported_targets.contains(&"qemuarm64".to_string()));
        assert!(supported_targets.contains(&"raspberrypi4".to_string()));

        // Test individual target checks
        assert!(config.is_target_supported("qemux86-64"));
        assert!(config.is_target_supported("qemuarm64"));
        assert!(config.is_target_supported("raspberrypi4"));
        assert!(!config.is_target_supported("unsupported-target"));
    }

    #[test]
    #[serial]
    fn test_supported_targets_all() {
        // Test with "*" (all targets supported)
        let config = create_config_with_supported_targets_all();

        assert!(config.get_supported_targets().is_none());
        assert!(config.is_target_supported("any-target"));
        assert!(config.is_target_supported("qemux86-64"));
        assert!(config.is_target_supported("custom-target"));
    }

    #[test]
    #[serial]
    fn test_supported_targets_none() {
        // Test with no supported_targets defined (all targets supported)
        let config = create_test_config(Some("any-target"));

        assert!(config.get_supported_targets().is_none());
        assert!(config.is_target_supported("any-target"));
        assert!(config.is_target_supported("test-target"));
    }

    #[test]
    #[serial]
    fn test_resolve_and_validate_target_success() {
        // Ensure no environment variable interferes with resolution
        env::remove_var("AVOCADO_TARGET");
        let config = create_config_with_supported_targets(vec!["qemux86-64".to_string()]);

        let result = resolve_and_validate_target_with_source(None, &config).map(|r| r.target);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "qemux86-64");
    }

    #[test]
    #[serial]
    fn test_resolve_and_validate_target_unsupported() {
        env::remove_var("AVOCADO_TARGET");
        let config = create_config_with_supported_targets(vec!["qemux86-64".to_string()]);
        // Override the default target to be unsupported
        let mut config = config;
        config.default_target = Some("unsupported-target".to_string());

        let result = resolve_and_validate_target_with_source(None, &config).map(|r| r.target);
        assert!(result.is_err());
        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("not supported"));
        assert!(error_message.contains("qemux86-64"));
    }

    #[test]
    #[serial]
    fn test_resolve_and_validate_target_all_supported() {
        env::remove_var("AVOCADO_TARGET");
        // When supported_targets = "*", should allow any target
        let config = create_config_with_supported_targets_all();
        let mut config = config;
        config.default_target = Some("any-target".to_string());

        let result = resolve_and_validate_target_with_source(None, &config).map(|r| r.target);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "any-target");
    }

    #[test]
    #[serial]
    fn test_resolve_and_validate_target_no_supported_targets() {
        env::remove_var("AVOCADO_TARGET");
        // When no supported_targets are defined, should allow any target
        let config = create_test_config(Some("any-target"));

        let result = resolve_and_validate_target_with_source(None, &config).map(|r| r.target);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "any-target");
    }
}
