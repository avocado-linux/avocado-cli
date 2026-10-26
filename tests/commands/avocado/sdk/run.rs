//! Tests for sdk run command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "run", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "run", "-h"], None, None);
}

#[test]
fn test_sdk_run_echo() {
    let config_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("minimal.yaml");
    // The fixture points at a placeholder SDK image, so the container cannot run
    // in CI. Before ENG-2352, `sdk run` swallowed a failed container command and
    // exited 0, so asserting success passed falsely. `sdk run` now exits non-zero
    // on a failed command, so assert only that the CLI runs to completion.
    let result =
        common::cli_with_config(&["sdk", "run", "--", "echo", "test"], None, Some(&config_path));
    common::assert_command_completes(&result);
}

#[test]
fn test_sdk_run_simple_command() {
    let result = common::run_cli_in_temp(&["sdk", "run", "-c", "true"]);
    // May fail due to container access, but should complete
    common::assert_command_completes(&result);
}

/// A typo'd `-r` must be rejected through the CLI entry path, before any
/// container work, so a mistyped runtime cannot fall through (ENG-2352).
#[test]
fn test_sdk_run_rejects_unknown_runtime() {
    let config_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("minimal.yaml");
    let result = common::cli_with_config(
        &["sdk", "run", "-r", "bogus", "--", "true"],
        None,
        Some(&config_path),
    );
    assert!(!result.success, "unknown runtime must fail");
    assert!(
        result.stderr.contains("not found in configuration"),
        "stderr: {}",
        result.stderr
    );
}

/// A runtime defined through the singular `runtime:` alias must be accepted.
/// The old on-disk check read only the literal `runtimes:` key and rejected it
/// before `sdk run` could validate against the composed config (ENG-2352).
#[test]
fn test_sdk_run_accepts_aliased_runtime() {
    let config_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("runtime_alias.yaml");
    let result = common::cli_with_config(
        &["sdk", "run", "-r", "aliased", "--", "true"],
        None,
        Some(&config_path),
    );
    // It may still fail for container reasons, but it must not be rejected as an
    // unknown runtime.
    assert!(
        !result.stderr.contains("not found in configuration"),
        "aliased runtime was wrongly rejected. stderr: {}",
        result.stderr
    );
}
