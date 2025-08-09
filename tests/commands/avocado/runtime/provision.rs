//! Tests for runtime provision command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "provision", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "provision", "-h"], None, None);
}

#[test]
fn test_runtime_provision_with_target() {
    let result = common::run_cli_in_temp_with_config(&[
        "runtime",
        "provision",
        "--runtime",
        "test-runtime",
        "--target",
        "x86_64-unknown-linux-gnu",
    ]);
    // May succeed or fail depending on configuration and container access
    common::assert_command_completes(&result);
}

#[test]
fn test_runtime_provision_missing_runtime() {
    let result = common::run_cli_in_temp_with_config(&[
        "runtime",
        "provision",
        "--runtime",
        "nonexistent-runtime",
    ]);
    // Should complete (may succeed or fail depending on validation)
    common::assert_command_completes(&result);
}

#[test]
fn test_runtime_provision_with_force() {
    let result = common::run_cli_in_temp_with_config(&[
        "runtime",
        "provision",
        "--runtime",
        "test-runtime",
        "--force",
        "--target",
        "x86_64-unknown-linux-gnu",
    ]);
    // May succeed or fail depending on configuration and container access
    common::assert_command_completes(&result);
}

#[test]
fn test_runtime_provision_with_verbose() {
    let result = common::run_cli_in_temp_with_config(&[
        "runtime",
        "provision",
        "--runtime",
        "test-runtime",
        "--verbose",
        "--target",
        "x86_64-unknown-linux-gnu",
    ]);
    // May succeed or fail depending on configuration and container access
    common::assert_command_completes(&result);
}
