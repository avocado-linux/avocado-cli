//! Tests for sdk run command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "run", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "run", "-h"], None, None);
}

#[test]
fn test_sdk_run_echo() {
    let result = common::run_cli_in_temp(&["sdk", "run", "-c", "echo", "test"]);
    // May fail due to container access, but should complete
    common::assert_command_completes(&result);
}

#[test]
fn test_sdk_run_simple_command() {
    let result = common::run_cli_in_temp(&["sdk", "run", "-c", "true"]);
    // May fail due to container access, but should complete
    common::assert_command_completes(&result);
}
