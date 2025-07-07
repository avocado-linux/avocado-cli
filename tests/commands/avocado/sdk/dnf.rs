//! Tests for sdk dnf command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "dnf", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "dnf", "-h"], None, None);
}
