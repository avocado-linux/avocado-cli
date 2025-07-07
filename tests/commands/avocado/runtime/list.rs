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
    common::assert_cmd(&["runtime", "list"], None, None);
}
