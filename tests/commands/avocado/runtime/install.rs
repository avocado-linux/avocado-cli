//! Tests for runtime install command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "install", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "install", "-h"], None, None);
}

#[test]
fn test_install_missing_config() {
    common::refute_cmd(&["runtime", "install", "-C", "nonexistent.toml"], None, None);
}

#[test]
fn test_install_with_verbose() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "install", "--verbose"], None, None);
}

#[test]
fn test_install_with_runtime_flag() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "install", "-r", "test-runtime"], None, None);
}

#[test]
fn test_install_with_force() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "install", "--force"], None, None);
}

#[test]
fn test_install_with_container_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "install", "--container-arg", "--cap-add=SYS_ADMIN"], None, None);
}

#[test]
fn test_install_with_dnf_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["runtime", "install", "--dnf-arg", "--nogpgcheck"], None, None);
}

#[test]
fn test_install_all_runtimes() {
    // Install for all runtimes (no -r flag)
    common::refute_cmd(&["runtime", "install"], None, None);
}
