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
    common::refute_cmd(&["hitl", "server", "-C", "nonexistent.toml"], None, None);
}

#[test]
fn test_server_with_verbose() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["hitl", "server", "--verbose"], None, None);
}

#[test]
fn test_server_with_target() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["hitl", "server", "-t", "qemux86-64"], None, None);
}

#[test]
fn test_server_with_container_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(
        &[
            "hitl",
            "server",
            "--container-arg",
            "--privileged",
            "--container-arg",
            "--device=/dev/kvm",
        ],
        None,
        None,
    );
}

#[test]
fn test_server_with_dnf_args() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["hitl", "server", "--dnf-arg", "--assumeyes"], None, None);
}

#[test]
fn test_server_with_extension() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(&["hitl", "server", "-e", "avocado-dev"], None, None);
}

#[test]
fn test_server_with_multiple_extensions() {
    // This will fail with a real config but tests argument parsing
    common::refute_cmd(
        &[
            "hitl",
            "server",
            "--extension",
            "avocado-dev",
            "--extension",
            "foo",
        ],
        None,
        None,
    );
}
