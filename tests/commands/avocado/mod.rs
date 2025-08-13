//! Tests for avocado command.

pub mod clean;
pub mod ext;
pub mod hitl;
pub mod init;
pub mod runtime;
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
fn test_short_help() {
    common::assert_cmd(&["-h"], None, None);
}

#[test]
fn test_invalid_subcommand() {
    common::refute_cmd(&["invalid"], None, None);
}
