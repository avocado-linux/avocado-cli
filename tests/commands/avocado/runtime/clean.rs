//! Tests for runtime clean command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "clean", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "clean", "-h"], None, None);
}

#[test]
fn test_clean_missing_config() {
    common::refute_cmd(
        &[
            "runtime",
            "clean",
            "-C",
            "nonexistent.toml",
            "-r",
            "test-runtime",
        ],
        None,
        None,
    );
}

#[test]
fn test_clean_missing_runtime_flag() {
    // Should fail because runtime flag is required
    common::refute_cmd(&["runtime", "clean"], None, None);
}

#[test]
fn test_clean_with_verbose() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(
        &["runtime", "clean", "--verbose", "-r", "test-runtime"],
        None,
        None,
    );
}

#[test]
fn test_clean_with_runtime_flag() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "clean", "-r", "test-runtime"], None, None);
}

#[test]
fn test_clean_with_container_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(
        &[
            "runtime",
            "clean",
            "-r",
            "test-runtime",
            "--container-arg",
            "--cap-add=SYS_ADMIN",
        ],
        None,
        None,
    );
}

#[test]
fn test_clean_with_dnf_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(
        &[
            "runtime",
            "clean",
            "-r",
            "test-runtime",
            "--dnf-arg",
            "--nogpgcheck",
        ],
        None,
        None,
    );
}

#[test]
fn test_clean_with_all_options() {
    // Test with all available options
    common::refute_cmd(
        &[
            "runtime",
            "clean",
            "-C",
            "custom.toml",
            "--verbose",
            "-r",
            "test-runtime",
            "--container-arg",
            "--cap-add=SYS_ADMIN",
            "--dnf-arg",
            "--nogpgcheck",
        ],
        None,
        None,
    );
}
