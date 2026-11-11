//! Environment variable interpolation context.
//!
//! Provides interpolation for `{{ env.VAR_NAME }}` templates.
//!
//! **Behavior:**
//! - Looks up `--env KEY=VALUE` values first, then the caller's environment
//! - Outputs a warning if the variable is not set
//! - Replaces with empty string when variable is missing

use anyhow::Result;
use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, OnceLock};

use crate::utils::output::{print_warning, OutputLevel};

static FLAG_OVERRIDES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Make a command's `--env KEY=VALUE` values visible to `{{ env.KEY }}`,
/// ahead of the caller's environment. `--env` otherwise reaches only the
/// container, so a run whose shell does not also export the variable (a
/// Rubicon hub replaying a provision with the caller's `--env`) rendered it
/// empty.
pub fn set_flag_overrides(vars: HashMap<String, String>) {
    *FLAG_OVERRIDES.get_or_init(Default::default).lock().unwrap() = vars;
}

/// Resolve an environment variable template.
///
/// # Arguments
/// * `var_name` - The environment variable name
///
/// # Returns
/// Result with Option<String> - Some(value) or Some("") with warning if not found
///
/// # Examples
/// ```
/// # use avocado_cli::utils::interpolation::env::resolve;
/// std::env::set_var("TEST_VAR", "test_value");
/// let result = resolve("TEST_VAR").unwrap();
/// assert_eq!(result, Some("test_value".to_string()));
/// std::env::remove_var("TEST_VAR");
/// ```
pub fn resolve(var_name: &str) -> Result<Option<String>> {
    let flagged = FLAG_OVERRIDES
        .get()
        .and_then(|m| m.lock().unwrap().get(var_name).cloned());
    match flagged.map_or_else(|| env::var(var_name), Ok) {
        Ok(value) => Ok(Some(value)),
        Err(_) => {
            // Route through `print_warning` (not a raw `eprintln!`) so the
            // message is suppressed while a TUI renderer is active. A direct
            // stderr write here lands inside the renderer's cursor region
            // without being counted in `rendered_lines`, so the next redraw's
            // MoveUp/Clear clears one line too few and strands a task line.
            print_warning(
                &format!(
                    "Environment variable '{var_name}' is not set, replacing with empty string"
                ),
                OutputLevel::Normal,
            );
            Ok(Some(String::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_resolve_existing_var() {
        env::set_var("TEST_ENV_VAR", "test_value");
        let result = resolve("TEST_ENV_VAR").unwrap();
        assert_eq!(result, Some("test_value".to_string()));
        env::remove_var("TEST_ENV_VAR");
    }

    #[test]
    #[serial]
    fn a_flag_value_wins_over_the_environment() {
        env::set_var("FLAGGED_ENV_VAR", "shell");
        env::remove_var("FLAG_ONLY_ENV_VAR");
        set_flag_overrides(HashMap::from([
            ("FLAGGED_ENV_VAR".to_string(), "flag".to_string()),
            ("FLAG_ONLY_ENV_VAR".to_string(), "0.2.0".to_string()),
        ]));
        let flagged = resolve("FLAGGED_ENV_VAR").unwrap();
        let flag_only = resolve("FLAG_ONLY_ENV_VAR").unwrap();
        set_flag_overrides(HashMap::new());
        env::remove_var("FLAGGED_ENV_VAR");
        assert_eq!(flagged, Some("flag".to_string()));
        assert_eq!(flag_only, Some("0.2.0".to_string()));
    }

    #[test]
    #[serial]
    fn test_resolve_missing_var() {
        env::remove_var("MISSING_ENV_VAR");
        let result = resolve("MISSING_ENV_VAR").unwrap();
        // Should return empty string
        assert_eq!(result, Some(String::new()));
    }
}
