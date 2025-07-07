//! Tests for sdk install command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "install", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "install", "-h"], None, None);
}
