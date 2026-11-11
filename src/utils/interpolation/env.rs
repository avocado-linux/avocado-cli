//! Environment variable interpolation context.
//!
//! Provides interpolation for `{{ env.VAR_NAME }}` templates.
//!
//! **Behavior:**
//! - Looks up environment variables from the caller's environment
//! - Outputs a warning if the variable is not set
//! - Replaces with empty string when variable is missing

use anyhow::Result;
use std::cell::RefCell;
use std::env;

use crate::utils::output::{print_warning, OutputLevel};

thread_local! {
    static READS: RefCell<Option<Vec<(String, String)>>> = const { RefCell::new(None) };
}

/// Run `f` and return, with its result, each env var that `resolve` read
/// while it ran, as `(name, value)`. This includes a var whose template a
/// substitution only built on a later pass. `config validate` uses it to
/// hide those values in what it prints.
pub fn record_reads<T>(f: impl FnOnce() -> T) -> (T, Vec<(String, String)>) {
    READS.with(|reads| *reads.borrow_mut() = Some(Vec::new()));
    let out = f();
    let reads = READS.with(|reads| reads.borrow_mut().take());
    (out, reads.unwrap_or_default())
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
    match env::var(var_name) {
        Ok(value) => {
            READS.with(|reads| {
                if let Some(reads) = reads.borrow_mut().as_mut() {
                    reads.push((var_name.to_string(), value.clone()));
                }
            });
            Ok(Some(value))
        }
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
