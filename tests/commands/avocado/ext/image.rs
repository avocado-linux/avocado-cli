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
    let config_path = std::env::current_dir()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("with-sysext.yaml");
    let result =
        common::cli_with_config(&["ext", "image", "test-sysext"], None, Some(&config_path));
    // Should complete regardless of Docker availability
    common::assert_command_completes(&result);
}
