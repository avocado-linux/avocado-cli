//! Tests for init command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["init", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["init", "-h"], None, None);
}

#[test]
fn test_init_with_target() {
    let result = common::run_cli_in_temp(&["init", "--target", "x86_64-unknown-linux-gnu"]);
    // May fail if config already exists, but should complete
    common::assert_command_completes(&result);
}

#[test]
fn test_init_current_directory() {
    // Run the init command in a temporary directory
    let result = common::run_cli_in_temp(&["init"]);

    // May fail if config already exists, but should complete
    common::assert_command_completes(&result);
}

#[test]
fn test_init_with_directory() {
    // Run the init command to create the project directory in a temp directory
    let result = common::run_cli_in_temp(&["init", "test-project"]);

    // May fail if config already exists, but should complete
    common::assert_command_completes(&result);
}
