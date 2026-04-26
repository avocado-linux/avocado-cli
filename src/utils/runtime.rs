//! Runtime resolution utilities for Avocado CLI.
//!
//! Mirrors the [`crate::utils::target`] module shape: same precedence
//! semantics (CLI > env > config > auto-resolve > error), same source-tagging
//! pattern, same error-message style. Many commands now operate against a
//! named runtime (extension lifecycle, install, clean), and consistent
//! resolution keeps the CLI ergonomics uniform.

use crate::utils::config::Config;
use crate::utils::output::{print_info, OutputLevel};
use std::env;

/// Source of runtime resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeSource {
    /// Runtime came from CLI argument (--runtime / -r).
    Cli,
    /// Runtime came from environment variable (AVOCADO_RUNTIME).
    Environment,
    /// Runtime came from configuration file (default_runtime).
    Config,
    /// Runtime was auto-resolved as the project's sole defined runtime.
    Auto,
}

impl std::fmt::Display for RuntimeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeSource::Cli => write!(f, "CLI argument (--runtime)"),
            RuntimeSource::Environment => write!(f, "environment variable (AVOCADO_RUNTIME)"),
            RuntimeSource::Config => write!(f, "config file (default_runtime)"),
            RuntimeSource::Auto => write!(f, "sole runtime in config"),
        }
    }
}

/// Result of runtime resolution with source information.
#[derive(Debug, Clone)]
pub struct RuntimeResolution {
    /// The resolved runtime name.
    pub runtime: String,
    /// Source where the runtime was resolved from.
    pub source: RuntimeSource,
}

/// Get runtime name from environment variable.
pub fn get_runtime_from_env() -> Option<String> {
    env::var("AVOCADO_RUNTIME").ok().filter(|s| !s.is_empty())
}

/// Resolve the runtime name with precedence:
///
/// 1. CLI flag `--runtime` / `-r`
/// 2. Environment variable `AVOCADO_RUNTIME`
/// 3. Configuration `default_runtime:`
/// 4. Auto-resolve when the project defines exactly one runtime
/// 5. Otherwise `None`
///
/// Returns the resolved name and its source, or `None` when no resolution
/// path applies.
pub fn resolve_runtime_with_source(
    cli_runtime: Option<&str>,
    config: &Config,
) -> Option<RuntimeResolution> {
    if let Some(name) = cli_runtime.filter(|s| !s.is_empty()) {
        return Some(RuntimeResolution {
            runtime: name.to_string(),
            source: RuntimeSource::Cli,
        });
    }

    if let Some(name) = get_runtime_from_env() {
        return Some(RuntimeResolution {
            runtime: name,
            source: RuntimeSource::Environment,
        });
    }

    if let Some(name) = config.default_runtime.as_deref() {
        return Some(RuntimeResolution {
            runtime: name.to_string(),
            source: RuntimeSource::Config,
        });
    }

    // Auto-resolve: exactly one runtime defined.
    if let Some(runtimes) = config.runtimes.as_ref() {
        if runtimes.len() == 1 {
            let name = runtimes.keys().next().unwrap().clone();
            return Some(RuntimeResolution {
                runtime: name,
                source: RuntimeSource::Auto,
            });
        }
    }

    None
}

/// Resolve the runtime name without source tagging.
#[allow(dead_code)] // Consumed by per-command wiring (Phase 1c+).
pub fn resolve_runtime(cli_runtime: Option<&str>, config: &Config) -> Option<String> {
    resolve_runtime_with_source(cli_runtime, config).map(|r| r.runtime)
}

/// Resolve the runtime name, returning a precise error when none applies.
///
/// Mirrors the error format used by `target::resolve_target_required` so the
/// two surface areas feel consistent.
#[allow(dead_code)] // Consumed by per-command wiring (Phase 1c+).
pub fn resolve_runtime_required(
    cli_runtime: Option<&str>,
    config: &Config,
) -> anyhow::Result<String> {
    resolve_runtime_with_source(cli_runtime, config)
        .map(|r| r.runtime)
        .ok_or_else(|| {
            let runtime_count = config.runtimes.as_ref().map(|m| m.len()).unwrap_or(0);
            if runtime_count > 1 {
                let mut names: Vec<&str> = config
                    .runtimes
                    .as_ref()
                    .unwrap()
                    .keys()
                    .map(|s| s.as_str())
                    .collect();
                names.sort_unstable();
                anyhow::anyhow!(
                    "Multiple runtimes defined ({}); specify one with --runtime, AVOCADO_RUNTIME, \
                     or set `default_runtime:` in avocado.yaml",
                    names.join(", ")
                )
            } else {
                anyhow::anyhow!(
                    "No runtime specified and no `runtimes:` defined in avocado.yaml. \
                     Use --runtime, AVOCADO_RUNTIME, or `default_runtime:`."
                )
            }
        })
}

/// Resolve and validate a runtime, then log the resolved name + source.
///
/// Validates that the resolved name matches a defined runtime in
/// `config.runtimes`. (Sole-runtime auto-resolution is inherently valid;
/// CLI/env/config paths can name a non-existent runtime, which we reject
/// here with a `Available: ...` hint.)
#[allow(dead_code)] // Consumed by per-command wiring (Phase 1c+).
pub fn validate_and_log_runtime(
    cli_runtime: Option<&str>,
    config: &Config,
) -> anyhow::Result<String> {
    let resolution = resolve_runtime_with_source(cli_runtime, config).ok_or_else(|| {
        let runtime_count = config.runtimes.as_ref().map(|m| m.len()).unwrap_or(0);
        if runtime_count > 1 {
            anyhow::anyhow!(
                "No runtime specified and the project defines multiple runtimes. \
                 Use --runtime, AVOCADO_RUNTIME, or `default_runtime:`."
            )
        } else {
            anyhow::anyhow!("No runtime specified and no `runtimes:` defined in avocado.yaml.")
        }
    })?;

    let defined = config
        .runtimes
        .as_ref()
        .is_some_and(|m| m.contains_key(&resolution.runtime));
    if !defined {
        let available = config
            .runtimes
            .as_ref()
            .map(|m| {
                let mut names: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
                names.sort_unstable();
                names.join(", ")
            })
            .unwrap_or_default();
        if available.is_empty() {
            return Err(anyhow::anyhow!(
                "Runtime '{}' (from {}) is not defined; no `runtimes:` block in avocado.yaml",
                resolution.runtime,
                resolution.source
            ));
        } else {
            return Err(anyhow::anyhow!(
                "Runtime '{}' (from {}) is not defined. Available: {}",
                resolution.runtime,
                resolution.source,
                available
            ));
        }
    }

    print_info(
        &format!(
            "Using runtime: {} (from {})",
            resolution.runtime, resolution.source
        ),
        OutputLevel::Normal,
    );

    Ok(resolution.runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::config::RuntimeConfig;
    use serial_test::serial;
    use std::collections::HashMap;

    fn empty_config() -> Config {
        Config {
            cli_requirement: None,
            source_date_epoch: None,
            default_target: None,
            supported_targets: None,
            src_dir: None,
            distro: None,
            runtimes: None,
            default_runtime: None,
            sdk: None,
            provision_profiles: None,
            signing_keys: None,
            connect: None,
            rootfs: None,
            initramfs: None,
            kernel: None,
        }
    }

    fn rt(name: &str) -> (String, RuntimeConfig) {
        (
            name.to_string(),
            RuntimeConfig {
                target: None,
                version: None,
                dependencies: None,
                stone_include_paths: None,
                stone_manifest: None,
                signing: None,
                kernel: None,
                rootfs: None,
                initramfs: None,
                var: None,
            },
        )
    }

    fn config_with_runtimes(names: &[&str]) -> Config {
        let mut c = empty_config();
        let mut map = HashMap::new();
        for n in names {
            let (k, v) = rt(n);
            map.insert(k, v);
        }
        c.runtimes = Some(map);
        c
    }

    #[test]
    #[serial]
    fn test_cli_wins_over_env_and_config() {
        env::set_var("AVOCADO_RUNTIME", "env-rt");
        let mut c = config_with_runtimes(&["dev", "prod"]);
        c.default_runtime = Some("prod".to_string());
        let r = resolve_runtime(Some("cli-rt"), &c);
        assert_eq!(r.as_deref(), Some("cli-rt"));
        env::remove_var("AVOCADO_RUNTIME");
    }

    #[test]
    #[serial]
    fn test_env_wins_over_config() {
        env::set_var("AVOCADO_RUNTIME", "env-rt");
        let mut c = config_with_runtimes(&["dev", "prod"]);
        c.default_runtime = Some("prod".to_string());
        let r = resolve_runtime(None, &c);
        assert_eq!(r.as_deref(), Some("env-rt"));
        env::remove_var("AVOCADO_RUNTIME");
    }

    #[test]
    #[serial]
    fn test_config_wins_over_auto() {
        env::remove_var("AVOCADO_RUNTIME");
        let mut c = config_with_runtimes(&["dev", "prod"]);
        c.default_runtime = Some("prod".to_string());
        let r = resolve_runtime(None, &c);
        assert_eq!(r.as_deref(), Some("prod"));
    }

    #[test]
    #[serial]
    fn test_auto_resolves_sole_runtime() {
        env::remove_var("AVOCADO_RUNTIME");
        let c = config_with_runtimes(&["solo"]);
        let resolution = resolve_runtime_with_source(None, &c).unwrap();
        assert_eq!(resolution.runtime, "solo");
        assert_eq!(resolution.source, RuntimeSource::Auto);
    }

    #[test]
    #[serial]
    fn test_required_errors_with_multi_runtime_no_default() {
        env::remove_var("AVOCADO_RUNTIME");
        let c = config_with_runtimes(&["dev", "prod"]);
        let err = resolve_runtime_required(None, &c).unwrap_err().to_string();
        assert!(
            err.contains("Multiple runtimes") && err.contains("dev") && err.contains("prod"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_required_errors_with_no_runtimes_block() {
        env::remove_var("AVOCADO_RUNTIME");
        let c = empty_config();
        let err = resolve_runtime_required(None, &c).unwrap_err().to_string();
        assert!(
            err.contains("No runtime") && err.contains("avocado.yaml"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_validate_rejects_unknown_runtime_name() {
        env::remove_var("AVOCADO_RUNTIME");
        let c = config_with_runtimes(&["dev", "prod"]);
        let err = validate_and_log_runtime(Some("staging"), &c)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'staging'") && err.contains("Available: dev, prod"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_validate_accepts_resolved_runtime() {
        env::remove_var("AVOCADO_RUNTIME");
        let c = config_with_runtimes(&["dev"]);
        let r = validate_and_log_runtime(None, &c).unwrap();
        assert_eq!(r, "dev");
    }

    #[test]
    #[serial]
    fn test_env_empty_string_treated_as_unset() {
        env::set_var("AVOCADO_RUNTIME", "");
        let c = config_with_runtimes(&["solo"]);
        let resolution = resolve_runtime_with_source(None, &c).unwrap();
        assert_eq!(resolution.source, RuntimeSource::Auto);
        env::remove_var("AVOCADO_RUNTIME");
    }
}
