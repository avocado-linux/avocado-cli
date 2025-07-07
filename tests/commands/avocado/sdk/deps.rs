//! Tests for sdk deps command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "deps", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "deps", "-h"], None, None);
}
