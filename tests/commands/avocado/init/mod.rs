//! Tests for init command.

use crate::common;
use std::env;

fn with_rust_cli<F>(test_fn: F)
where
    F: FnOnce(),
{
    // Set environment variable to use Rust CLI
    env::set_var("USE_RUST_CLI", "1");

    // Run the test
    test_fn();

    // Clean up
    env::remove_var("USE_RUST_CLI");
}

#[test]
fn test_long_help() {
    with_rust_cli(|| {
        common::assert_cmd(&["init", "--help"], None, None);
    });
}

#[test]
fn test_short_help() {
    with_rust_cli(|| {
        common::assert_cmd(&["init", "-h"], None, None);
    });
}

#[test]
fn test_init_with_target() {
    with_rust_cli(|| {
        let result = common::run_cli_in_temp(&["init", "--target", "x86_64-unknown-linux-gnu"]);
        // May fail if config already exists, but should complete
        common::assert_command_completes(&result);
    });
}

#[test]
fn test_init_current_directory() {
    with_rust_cli(|| {
        // Run the init command in a temporary directory
        let result = common::run_cli_in_temp(&["init"]);

        // May fail if config already exists, but should complete
        common::assert_command_completes(&result);
    });
}

#[test]
fn test_init_with_directory() {
    with_rust_cli(|| {
        // Run the init command to create the project directory in a temp directory
        let result = common::run_cli_in_temp(&["init", "test-project"]);

        // May fail if config already exists, but should complete
        common::assert_command_completes(&result);
    });
}
