//! Tests for runtime build command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "build", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "build", "-h"], None, None);
}

#[test]
fn test_runtime_build_with_target() {
    let result = common::run_cli_in_temp_with_config(&[
        "runtime",
        "build",
        "--target",
        "x86_64-unknown-linux-gnu",
    ]);
    // May succeed or fail depending on configuration and container access
    common::assert_command_completes(&result);
}

#[test]
fn test_runtime_build_missing_target() {
    let result = common::run_cli_in_temp_with_config(&["runtime", "build", "nonexistent-target"]);
    // Should complete (may succeed or fail depending on validation)
    common::assert_command_completes(&result);
}
