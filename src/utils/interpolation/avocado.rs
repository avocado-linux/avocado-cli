//! Avocado computed values interpolation context.
//!
//! Provides interpolation for `{{ avocado.* }}` templates.
//!
//! **Available values:**
//! - `{{ avocado.target }}` - Resolved target architecture
//! - `{{ avocado.target.board }}` - Resolved target board (falls back to target)
//! - `{{ avocado.runtime }}` - Resolved runtime name (env → default_runtime → sole)
//! - `{{ avocado.distro.release }}` - Distro release (feed year) from main config
//! - `{{ avocado.distro.version }}` - Alias for distro.release (backward compat)
//! - `{{ avocado.distro.channel }}` - Distro channel from main config
//! - `{{ avocado.extensions.<name>.<field> }}` - Value from the merged extensions section
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
    /// Optional board variant within a target. `None` here means the resolver
    /// will fall back to `target` at lookup time, so `{{ avocado.target.board }}`
    /// equals `{{ avocado.target }}` by default. Precedence:
    /// env > resolved-runtime's `target_board` > top-level `default_target_board`.
    pub target_board: Option<String>,
    /// Resolved runtime name. Precedence: env `AVOCADO_RUNTIME` >
    /// `default_runtime` > sole runtime when exactly one is defined. `None`
    /// when no runtime can be resolved (e.g. multiple runtimes with no
    /// `default_runtime` set). Surfaces through `{{ avocado.runtime }}`.
    pub runtime: Option<String>,
    /// Distro release (feed year) from the main config
    pub distro_release: Option<String>,
    /// Distro channel from the main config
    pub distro_channel: Option<String>,
    /// Resolved kernel version for the current scope. Populated after kernel
    /// version resolution runs; `None` before resolution (or when no kernel is
    /// involved, e.g. SDK-host-only operations).
    pub kernel_version: Option<String>,
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
            target_board: None,
            runtime: None,
            distro_release: None,
            distro_channel: None,
            kernel_version: None,
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
    /// * `cli_target_board` - Optional CLI target board override (highest priority)
    pub fn from_main_config(
        root: &Value,
        cli_target: Option<&str>,
        cli_target_board: Option<&str>,
    ) -> Self {
        // Resolve target with precedence: CLI > env > config
        let target = Self::resolve_target_value(root, cli_target);

        // Resolve runtime once and reuse for target_board lookup. Precedence:
        // env > default_runtime > sole-runtime auto-resolve.
        let runtime = Self::resolve_runtime_name(root);

        // Resolve target_board with precedence: CLI > env > resolved-runtime's
        // `target_board` > top-level `default_target_board`. Stored as None
        // when none are set so the resolver can fall back to `target` at
        // lookup time.
        let target_board =
            Self::resolve_target_board_value(root, cli_target_board, runtime.as_deref());

        // Extract distro values from the main config
        let (distro_release, distro_channel) = Self::extract_distro_values(root);

        Self {
            target,
            target_board,
            runtime,
            distro_release,
            distro_channel,
            kernel_version: None,
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

    /// Resolve the target board value. Precedence: CLI override
    /// `cli_target_board`, then env `AVOCADO_TARGET_BOARD`, then the resolved
    /// runtime's `target_board`, then config `default_target_board`. Returns
    /// `None` when none are set; the resolver then falls back to the resolved
    /// target.
    ///
    /// `runtime_name` is the pre-resolved runtime (see
    /// [`Self::resolve_runtime_name`]); when `None`, the per-runtime
    /// `target_board` lookup is skipped.
    fn resolve_target_board_value(
        root: &Value,
        cli_target_board: Option<&str>,
        runtime_name: Option<&str>,
    ) -> Option<String> {
        // 1. CLI target board (highest priority)
        if let Some(board) = cli_target_board {
            return Some(board.to_string());
        }

        if let Ok(board) = env::var("AVOCADO_TARGET_BOARD") {
            return Some(board);
        }

        if let Some(name) = runtime_name {
            if let Some(board) = root
                .get("runtimes")
                .and_then(|v| v.get(name))
                .and_then(|v| v.get("target_board"))
                .and_then(|v| v.as_str())
            {
                return Some(board.to_string());
            }
        }

        if let Some(board) = root.get("default_target_board") {
            if let Some(board_str) = board.as_str() {
                return Some(board_str.to_string());
            }
        }

        None
    }

    /// Resolve the active runtime name from raw YAML. Precedence:
    /// 1. env `AVOCADO_RUNTIME`
    /// 2. `default_runtime`
    /// 3. Sole runtime (when exactly one is defined under `runtimes:`)
    ///
    /// Mirrors [`crate::utils::runtime::resolve_runtime_with_source`] but
    /// operates on the raw YAML (interpolation runs before deserialization).
    fn resolve_runtime_name(root: &Value) -> Option<String> {
        if let Ok(name) = env::var("AVOCADO_RUNTIME") {
            if !name.is_empty() {
                return Some(name);
            }
        }

        if let Some(name) = root.get("default_runtime").and_then(|v| v.as_str()) {
            return Some(name.to_string());
        }

        let runtimes = root.get("runtimes")?.as_mapping()?;
        if runtimes.len() == 1 {
            return runtimes
                .keys()
                .next()
                .and_then(|k| k.as_str())
                .map(|s| s.to_string());
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
        target_board: Option<String>,
        runtime: Option<String>,
        distro_release: Option<String>,
        distro_channel: Option<String>,
    ) -> Self {
        Self {
            target,
            target_board,
            runtime,
            distro_release,
            distro_channel,
            kernel_version: None,
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
///     target_board: None,
///     runtime: None,
///     distro_release: Some("2024".to_string()),
///     distro_channel: Some("edge".to_string()),
///     kernel_version: None,
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
        ["target", "board"] => resolve_target_board(root, context),
        ["runtime"] => resolve_runtime(root, context),
        ["distro", "release"] | ["distro", "version"] => resolve_distro_release(context),
        ["distro", "channel"] => resolve_distro_channel(context),
        ["kernel", "version"] => resolve_kernel_version(context),
        ["extensions", rest @ ..] => resolve_extensions(rest, root),
        _ => {
            // Other avocado keys are not yet supported, but don't error
            // Just leave the template as-is for future extension
            Ok(None)
        }
    }
}

/// Resolve the active runtime name. Precedence:
/// 1. Context `runtime` (pre-resolved at context creation)
/// 2. Direct lookup against `root` using the same precedence
///    ([`AvocadoContext::resolve_runtime_name`])
fn resolve_runtime(root: &Value, context: Option<&AvocadoContext>) -> Result<Option<String>> {
    if let Some(ctx) = context {
        if let Some(ref name) = ctx.runtime {
            return Ok(Some(name.clone()));
        }
    }

    Ok(AvocadoContext::resolve_runtime_name(root))
}

/// Resolve the kernel version from context. Returns `None` if the context
/// doesn't yet have a resolved kernel version (e.g. the resolver hasn't run
/// for this scope).
fn resolve_kernel_version(context: Option<&AvocadoContext>) -> Result<Option<String>> {
    Ok(context.and_then(|c| c.kernel_version.clone()))
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

/// Resolve the target board.
///
/// Precedence:
/// 1. Context `target_board` (populated from env or config at context creation)
/// 2. Environment variable `AVOCADO_TARGET_BOARD`
/// 3. Config `default_target_board` (from root)
/// 4. Fallback to the resolved target — `{{ avocado.target.board }}` defaults
///    to whatever `{{ avocado.target }}` would resolve to when no board is
///    explicitly set.
fn resolve_target_board(root: &Value, context: Option<&AvocadoContext>) -> Result<Option<String>> {
    if let Some(ctx) = context {
        if let Some(ref board) = ctx.target_board {
            return Ok(Some(board.clone()));
        }
    }

    if let Ok(board) = env::var("AVOCADO_TARGET_BOARD") {
        return Ok(Some(board));
    }

    if let Some(default_board) = root.get("default_target_board") {
        if let Some(board_str) = default_board.as_str() {
            return Ok(Some(board_str.to_string()));
        }
    }

    resolve_target(root, context)
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

/// Resolve a value from the merged extensions section.
///
/// Navigates `root["extensions"][path[0]][path[1]]...` and returns the scalar
/// value as a string. Returns `Ok(None)` if any segment is missing or the leaf
/// is a mapping/sequence (not meaningful for string interpolation).
fn resolve_extensions(path: &[&str], root: &Value) -> Result<Option<String>> {
    if path.is_empty() {
        return Ok(None);
    }

    let mut current = match root.get("extensions") {
        Some(v) => v,
        None => return Ok(None),
    };

    for segment in path {
        match current.get(*segment) {
            Some(v) => current = v,
            None => return Ok(None),
        }
    }

    match current {
        Value::Mapping(_) | Value::Sequence(_) => Ok(None),
        _ => Ok(Some(yaml_value_to_string(current))),
    }
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
    #[serial]
    fn test_resolve_target_board_from_context() {
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("default_target_board: config-board");
        let ctx = AvocadoContext {
            target: Some("imx8mp-evk".to_string()),
            target_board: Some("ctx-board".to_string()),
            runtime: None,
            distro_release: None,
            distro_channel: None,
            kernel_version: None,
        };
        let result = resolve(&["target", "board"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("ctx-board".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_from_env() {
        env::remove_var("AVOCADO_TARGET");
        env::set_var("AVOCADO_TARGET_BOARD", "env-board");
        let config = parse_yaml("default_target_board: config-board");
        let result = resolve(&["target", "board"], &config, None).unwrap();
        assert_eq!(result, Some("env-board".to_string()));
        env::remove_var("AVOCADO_TARGET_BOARD");
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_from_config() {
        env::remove_var("AVOCADO_TARGET");
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("default_target_board: config-board");
        let result = resolve(&["target", "board"], &config, None).unwrap();
        assert_eq!(result, Some("config-board".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_falls_back_to_context_target() {
        // No board source set anywhere; context has a target. The board should
        // resolve to whatever the target resolves to.
        env::remove_var("AVOCADO_TARGET");
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("{}");
        let ctx = AvocadoContext::with_target(Some("imx8mp-evk"));
        let result = resolve(&["target", "board"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("imx8mp-evk".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_falls_back_to_env_target() {
        // Neither AVOCADO_TARGET_BOARD nor default_target_board set; the board
        // should fall through to AVOCADO_TARGET.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::set_var("AVOCADO_TARGET", "env-target");
        let config = parse_yaml("{}");
        let result = resolve(&["target", "board"], &config, None).unwrap();
        assert_eq!(result, Some("env-target".to_string()));
        env::remove_var("AVOCADO_TARGET");
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_falls_back_to_default_target() {
        // No board sources, no env target — falls back to default_target.
        env::remove_var("AVOCADO_TARGET");
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("default_target: config-target");
        let result = resolve(&["target", "board"], &config, None).unwrap();
        assert_eq!(result, Some("config-target".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_unavailable() {
        env::remove_var("AVOCADO_TARGET");
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("{}");
        let result = resolve(&["target", "board"], &config, None).unwrap();
        // No target either — leave template as-is.
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
            target_board: None,
            runtime: None,
            distro_release: Some("2024".to_string()),
            distro_channel: None,
            kernel_version: None,
        };
        let result = resolve(&["distro", "release"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("2024".to_string()));
    }

    #[test]
    fn test_resolve_distro_version_alias() {
        let config = parse_yaml("{}");
        let ctx = AvocadoContext {
            target: None,
            target_board: None,
            runtime: None,
            distro_release: Some("2024".to_string()),
            distro_channel: None,
            kernel_version: None,
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
            target_board: None,
            runtime: None,
            distro_release: None,
            distro_channel: Some("apollo-edge".to_string()),
            kernel_version: None,
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
        let ctx = AvocadoContext::from_main_config(&config, None, None);
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
        let ctx = AvocadoContext::from_main_config(&config, None, None);
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
        let ctx = AvocadoContext::from_main_config(&config, Some("cli-target"), None);
        assert_eq!(ctx.target, Some("cli-target".to_string()));
        assert_eq!(ctx.distro_release, Some("2024".to_string()));
        assert_eq!(ctx.distro_channel, Some("edge".to_string()));
    }

    #[test]
    fn test_avocado_context_missing_distro() {
        let config = parse_yaml("default_target: x86_64");
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target, Some("x86_64".to_string()));
        assert_eq!(ctx.distro_release, None);
        assert_eq!(ctx.distro_channel, None);
    }

    #[test]
    #[serial]
    fn test_avocado_context_from_main_config_with_target_board() {
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: imx8mp-evk-rev3
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target, Some("imx8mp-evk".to_string()));
        assert_eq!(ctx.target_board, Some("imx8mp-evk-rev3".to_string()));
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_env_overrides_config() {
        env::set_var("AVOCADO_TARGET_BOARD", "env-board");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: config-board
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("env-board".to_string()));
        env::remove_var("AVOCADO_TARGET_BOARD");
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_unset_is_none() {
        // When neither env nor config sets the board, the context stores None.
        // The resolver — not the context — handles fallback to target.
        env::remove_var("AVOCADO_TARGET_BOARD");
        let config = parse_yaml("default_target: imx8mp-evk");
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target, Some("imx8mp-evk".to_string()));
        assert_eq!(ctx.target_board, None);
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_from_default_runtime() {
        // `default_runtime` selects the runtime whose `target_board` is used.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_runtime: rev3
runtimes:
  rev3:
    target: imx8mp-evk
    target_board: imx8mp-evk-rev3
  rev1:
    target: imx8mp-evk
    target_board: imx8mp-evk-rev1
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("imx8mp-evk-rev3".to_string()));
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_from_sole_runtime() {
        // No `default_runtime`, but exactly one runtime is defined — it auto-resolves.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
runtimes:
  only:
    target: imx8mp-evk
    target_board: imx8mp-evk-rev3
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("imx8mp-evk-rev3".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_cli_override_beats_env_and_default() {
        // The CLI override wins over both AVOCADO_TARGET_BOARD and a
        // top-level default_target_board.
        env::set_var("AVOCADO_TARGET_BOARD", "env-board");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: config-board
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, Some("flag-board"));
        assert_eq!(ctx.target_board, Some("flag-board".to_string()));
        env::remove_var("AVOCADO_TARGET_BOARD");
    }

    #[test]
    #[serial]
    fn test_resolve_target_board_none_override_preserves_env_chain() {
        // With no CLI override, resolution is unchanged: the env var still wins.
        env::set_var("AVOCADO_TARGET_BOARD", "env-board");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: config-board
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("env-board".to_string()));
        env::remove_var("AVOCADO_TARGET_BOARD");
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_runtime_overrides_default_target_board() {
        // Per-runtime `target_board` wins over top-level `default_target_board`.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: top-level-board
default_runtime: rev3
runtimes:
  rev3:
    target: imx8mp-evk
    target_board: imx8mp-evk-rev3
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("imx8mp-evk-rev3".to_string()));
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_env_runtime_selects_runtime() {
        // `AVOCADO_RUNTIME` selects which runtime's `target_board` is used.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::set_var("AVOCADO_RUNTIME", "rev1");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_runtime: rev3
runtimes:
  rev3:
    target_board: imx8mp-evk-rev3
  rev1:
    target_board: imx8mp-evk-rev1
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("imx8mp-evk-rev1".to_string()));
        env::remove_var("AVOCADO_RUNTIME");
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_env_overrides_runtime() {
        // `AVOCADO_TARGET_BOARD` env wins over per-runtime `target_board`.
        env::set_var("AVOCADO_TARGET_BOARD", "env-board");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_runtime: rev3
runtimes:
  rev3:
    target_board: imx8mp-evk-rev3
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("env-board".to_string()));
        env::remove_var("AVOCADO_TARGET_BOARD");
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_falls_back_to_default_when_runtime_lacks_field() {
        // Resolved runtime has no `target_board` — falls through to top-level.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: top-level-board
default_runtime: rev3
runtimes:
  rev3:
    target: imx8mp-evk
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("top-level-board".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_runtime_from_context() {
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml("default_runtime: cfg-rt");
        let ctx = AvocadoContext {
            target: None,
            target_board: None,
            runtime: Some("ctx-rt".to_string()),
            distro_release: None,
            distro_channel: None,
            kernel_version: None,
        };
        let result = resolve(&["runtime"], &config, Some(&ctx)).unwrap();
        assert_eq!(result, Some("ctx-rt".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_runtime_from_env() {
        env::set_var("AVOCADO_RUNTIME", "env-rt");
        let config = parse_yaml("default_runtime: cfg-rt");
        let result = resolve(&["runtime"], &config, None).unwrap();
        assert_eq!(result, Some("env-rt".to_string()));
        env::remove_var("AVOCADO_RUNTIME");
    }

    #[test]
    #[serial]
    fn test_resolve_runtime_from_default() {
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml("default_runtime: cfg-rt");
        let result = resolve(&["runtime"], &config, None).unwrap();
        assert_eq!(result, Some("cfg-rt".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_runtime_from_sole() {
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
runtimes:
  only:
    target: imx8mp-evk
"#,
        );
        let result = resolve(&["runtime"], &config, None).unwrap();
        assert_eq!(result, Some("only".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_runtime_unavailable_when_ambiguous() {
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
runtimes:
  rev3: {}
  rev1: {}
"#,
        );
        let result = resolve(&["runtime"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    #[serial]
    fn test_avocado_context_runtime_populated_from_default() {
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_runtime: rev3
runtimes:
  rev3:
    target: imx8mp-evk
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.runtime, Some("rev3".to_string()));
    }

    #[test]
    #[serial]
    fn test_avocado_context_target_board_skipped_when_multiple_runtimes_no_default() {
        // Multiple runtimes with no `default_runtime` and no `AVOCADO_RUNTIME`:
        // can't pick one, so per-runtime `target_board` is not consulted.
        env::remove_var("AVOCADO_TARGET_BOARD");
        env::remove_var("AVOCADO_RUNTIME");
        let config = parse_yaml(
            r#"
default_target: imx8mp-evk
default_target_board: top-level-board
runtimes:
  rev3:
    target_board: imx8mp-evk-rev3
  rev1:
    target_board: imx8mp-evk-rev1
"#,
        );
        let ctx = AvocadoContext::from_main_config(&config, None, None);
        assert_eq!(ctx.target_board, Some("top-level-board".to_string()));
    }

    #[test]
    fn test_resolve_extensions_version() {
        let config = parse_yaml(
            r#"
extensions:
  avocado-ext-dev:
    version: "2024.0.0"
    source:
      type: package
"#,
        );
        let result = resolve(&["extensions", "avocado-ext-dev", "version"], &config, None).unwrap();
        assert_eq!(result, Some("2024.0.0".to_string()));
    }

    #[test]
    fn test_resolve_extensions_nested_field() {
        let config = parse_yaml(
            r#"
extensions:
  my-ext:
    image:
      type: kab
"#,
        );
        let result = resolve(&["extensions", "my-ext", "image", "type"], &config, None).unwrap();
        assert_eq!(result, Some("kab".to_string()));
    }

    #[test]
    fn test_resolve_extensions_missing_extension() {
        let config = parse_yaml(
            r#"
extensions:
  some-ext:
    version: "1.0"
"#,
        );
        let result = resolve(&["extensions", "nonexistent", "version"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_extensions_missing_field() {
        let config = parse_yaml(
            r#"
extensions:
  my-ext:
    version: "1.0"
"#,
        );
        let result = resolve(&["extensions", "my-ext", "nonexistent"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_extensions_no_extensions_section() {
        let config = parse_yaml("{}");
        let result = resolve(&["extensions", "any-ext", "version"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_extensions_number_value() {
        let config = parse_yaml(
            r#"
extensions:
  my-ext:
    version: 2024
"#,
        );
        let result = resolve(&["extensions", "my-ext", "version"], &config, None).unwrap();
        assert_eq!(result, Some("2024".to_string()));
    }

    #[test]
    fn test_resolve_extensions_mapping_returns_none() {
        let config = parse_yaml(
            r#"
extensions:
  my-ext:
    image:
      type: kab
      args: "-b"
"#,
        );
        let result = resolve(&["extensions", "my-ext", "image"], &config, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_extensions_bare_keyword() {
        let config = parse_yaml(
            r#"
extensions:
  my-ext:
    version: "1.0"
"#,
        );
        let result = resolve(&["extensions"], &config, None).unwrap();
        assert_eq!(result, None);
    }
}
