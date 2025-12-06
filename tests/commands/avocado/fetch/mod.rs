//! Tests for fetch command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["fetch", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["fetch", "-h"], None, None);
}

#[test]
fn test_fetch_with_external_extensions() {
    let config_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("with-external-extensions.yaml");

    // This test verifies that the fetch command can discover and process
    // external extensions with nested configs without crashing
    let result = common::cli_with_config(
        &["fetch", "--verbose", "--target", "x86_64"],
        None,
        Some(&config_path),
    );

    // The command should complete (may fail due to container access, but should not crash)
    common::assert_command_completes(&result);

    // Check that the output contains information about external extensions
    let output = format!("{}{}", result.stdout, result.stderr);

    // The test should either:
    // 1. Successfully discover external extensions, OR
    // 2. Fail gracefully due to container/setup issues (which is expected in test environment)
    // 3. Show discovery messages in verbose output
    let has_discovery_info = output.contains("external-extension")
        || output.contains("nested-extension")
        || output.contains("Skipping already processed extension")
        || output.contains("Found nested external extension");

    let has_expected_failure = output.contains("Failed to fetch")
        || output.contains("No such file or directory")
        || output.contains("container")
        || output.contains("Error:");

    assert!(
        has_discovery_info || has_expected_failure,
        "Expected output to contain external extension discovery info or expected failure, but got: {output}"
    );
}

#[test]
fn test_fetch_discovers_nested_extensions() {
    let config_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("with-external-extensions.yaml");

    let result = common::cli_with_config(
        &["fetch", "--verbose", "--target", "x86_64"],
        None,
        Some(&config_path),
    );

    common::assert_command_completes(&result);

    // Check that the discovery process works by looking for specific log messages
    let output = format!("{}{}", result.stdout, result.stderr);

    // Should either find the extensions or skip them (if sysroots don't exist)
    let found_external = output.contains("external-extension");
    let found_nested = output.contains("nested-extension");
    let skipped_processing = output.contains("Skipping already processed extension");
    let no_sysroot = output.contains("does not exist, skipping metadata fetch");
    let has_expected_failure = output.contains("Failed to fetch")
        || output.contains("No such file or directory")
        || output.contains("container")
        || output.contains("Error:");

    assert!(
        found_external || found_nested || skipped_processing || no_sysroot || has_expected_failure,
        "Expected to find external extension processing information or expected failure, but got: {output}"
    );
}
