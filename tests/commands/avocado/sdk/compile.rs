//! Tests for sdk compile command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "compile", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "compile", "-h"], None, None);
}
