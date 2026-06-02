//! Environment variable interpolation context.
//!
//! Provides interpolation for `{{ env.VAR_NAME }}` templates.
//!
//! **Behavior:**
//! - Looks up environment variables from the caller's environment
//! - Outputs a warning if the variable is not set
//! - Replaces with empty string when variable is missing

use anyhow::Result;
use std::env;

use crate::utils::output::{print_warning, OutputLevel};

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
    match env::var(var_name) {
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
    fn test_resolve_missing_var() {
        env::remove_var("MISSING_ENV_VAR");
        let result = resolve("MISSING_ENV_VAR").unwrap();
        // Should return empty string
        assert_eq!(result, Some(String::new()));
    }
}
