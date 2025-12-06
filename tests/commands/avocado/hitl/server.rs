//! Tests for hitl server command.

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["hitl", "server", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["hitl", "server", "-h"], None, None);
}

#[test]
fn test_server_missing_config() {
    common::refute_cmd(&["hitl", "server", "-C", "nonexistent.yaml"], None, None);
}

#[test]
fn test_server_with_verbose() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(&["hitl", "server", "--verbose"], Some(&temp_dir), None);
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_server_with_target() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &["hitl", "server", "-t", "qemux86-64"],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_server_with_container_args() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &[
            "hitl",
            "server",
            "--container-arg",
            "--privileged",
            "--container-arg",
            "--device=/dev/kvm",
        ],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_server_with_dnf_args() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &["hitl", "server", "--dnf-arg", "--assumeyes"],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_server_with_extension() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &["hitl", "server", "-e", "avocado-dev"],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}

#[test]
fn test_server_with_multiple_extensions() {
    // This will fail without config - tests argument parsing
    let temp_dir = common::create_temp_dir();
    common::refute_cmd(
        &[
            "hitl",
            "server",
            "--extension",
            "avocado-dev",
            "--extension",
            "foo",
        ],
        Some(&temp_dir),
        None,
    );
    common::cleanup_temp_dir(&temp_dir);
}
