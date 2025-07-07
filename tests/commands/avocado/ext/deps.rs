//! Tests for ext deps command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["ext", "deps", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["ext", "deps", "-h"], None, None);
}
