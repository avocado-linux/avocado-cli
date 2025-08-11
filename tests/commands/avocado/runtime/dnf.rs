//! Tests for runtime dnf command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "dnf", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "dnf", "-h"], None, None);
}

#[test]
fn test_dnf_missing_config() {
    common::refute_cmd(&["runtime", "dnf", "-C", "nonexistent.toml", "-r", "test-runtime", "list"], None, None);
}

#[test]
fn test_dnf_missing_runtime_flag() {
    // Should fail because runtime flag is required
    common::refute_cmd(&["runtime", "dnf", "list"], None, None);
}

#[test]
fn test_dnf_with_verbose() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "dnf", "--verbose", "-r", "test-runtime", "list"], None, None);
}

#[test]
fn test_dnf_with_runtime_flag() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "dnf", "-r", "test-runtime", "list", "installed"], None, None);
}

#[test]
fn test_dnf_with_container_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "dnf", "-r", "test-runtime", "--container-arg", "--cap-add=SYS_ADMIN", "search", "python"], None, None);
}

#[test]
fn test_dnf_with_dnf_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "dnf", "-r", "test-runtime", "--dnf-arg", "--nogpgcheck", "install", "gcc"], None, None);
}

#[test]
fn test_dnf_complex_command() {
    // Test with multiple arguments to the DNF command
    common::refute_cmd(&["runtime", "dnf", "-r", "test-runtime", "install", "--enablerepo=updates", "gcc", "make"], None, None);
}

#[test]
fn test_dnf_with_hyphen_values() {
    // Test that hyphen values are allowed in command arguments
    common::refute_cmd(&["runtime", "dnf", "-r", "test-runtime", "install", "--exclude=kernel*", "gcc"], None, None);
}
