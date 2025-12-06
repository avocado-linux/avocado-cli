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
    common::refute_cmd(
        &["runtime", "install", "-C", "nonexistent.yaml"],
        None,
        None,
    );
}

#[test]
fn test_install_with_verbose() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(&["runtime", "install", "--verbose"], Some(&temp_dir), None);
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_install_with_runtime_flag() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &["runtime", "install", "-r", "test-runtime"],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_install_with_force() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(&["runtime", "install", "--force"], Some(&temp_dir), None);
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_install_with_container_args() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &[
            "runtime",
            "install",
            "--container-arg",
            "--cap-add=SYS_ADMIN",
        ],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_install_with_dnf_args() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &["runtime", "install", "--dnf-arg", "--nogpgcheck"],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_install_all_runtimes() {
    // Install for all runtimes (no -r flag) - will fail without config
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(&["runtime", "install"], Some(&temp_dir), None);
    common::cleanup_temp_dir(&temp_dir);
}
