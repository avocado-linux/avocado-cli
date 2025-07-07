//! Tests for ext image command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["ext", "image", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["ext", "image", "-h"], None, None);
}

#[test]
fn test_ext_image_missing_extension() {
    // Should fail because extension doesn't exist
    common::refute_cmd(&["ext", "image", "nonexistent"], None, None);
}

#[test]
fn test_ext_image_with_fixture_extension() {
    common::assert_cmd(&["ext", "image", "test-sysext"], None, None);
}
