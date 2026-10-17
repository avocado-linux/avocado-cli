//! Tests for avocado command.

pub mod clean;
pub mod ext;
pub mod fetch;
pub mod hitl;
pub mod init;
pub mod runtime;
pub mod sbom;
pub mod sdk;

use crate::common;

#[test]
fn test_no_args_shows_help() {
    let result = common::run_cli(&[]);
    common::assert_command_completes(&result);
}

#[test]
fn test_long_help() {
    common::assert_cmd(&["--help"], None, None);
}

#[test]
fn test_no_vm_auto_start_help_states_the_auto_start_condition() {
    let result = common::run_cli(&["--help"]);
    assert!(
        result
            .stdout
            .contains("starts a stopped one only when `AVOCADO_VM_DIR` is set"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("AVOCADO_VM_AUTO_START=0"),
        "{}",
        result.stdout
    );
}

#[test]
fn test_vm_start_help_says_it_fails_when_already_running() {
    let result = common::run_cli(&["vm", "start", "--help"]);
    assert!(
        result
            .stdout
            .contains("Boot the avocado-vm (fails if it is already running)"),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("no-op"), "{}", result.stdout);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["-h"], None, None);
}

#[test]
fn test_invalid_subcommand() {
    common::refute_cmd(&["invalid"], None, None);
}
