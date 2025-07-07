//! Tests for ext list command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["ext", "list", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["ext", "list", "-h"], None, None);
}

#[test]
fn test_ext_list() {
    let result = common::run_cli_in_temp_with_config(&["ext", "list"]);
    // Should complete regardless of whether extensions exist
    common::assert_command_completes(&result);
}
