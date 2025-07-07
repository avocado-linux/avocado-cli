//! Tests for runtime deps command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "deps", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "deps", "-h"], None, None);
}

#[test]
fn test_runtime_deps_for_target() {
    let result =
        common::run_cli_in_temp_with_config(&["runtime", "deps", "x86_64-unknown-linux-gnu"]);
    // May succeed or fail depending on configuration
    common::assert_command_completes(&result);
}
