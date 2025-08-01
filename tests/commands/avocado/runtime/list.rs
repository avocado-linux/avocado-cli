//! Tests for runtime list command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "list", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "list", "-h"], None, None);
}

#[test]
fn test_runtime_list() {
    let result = common::run_cli_in_temp_with_config(&["runtime", "list"]);
    // Should complete regardless of whether runtimes exist
    common::assert_command_completes(&result);
}
