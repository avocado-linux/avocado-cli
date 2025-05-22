//! Tests for sdk command.

pub mod compile;
pub mod deps;
pub mod dnf;
pub mod install;
pub mod run;

use crate::common;

#[test]
fn test_no_args_shows_help() {
    let result = common::run_cli(&["sdk"]);
    common::assert_command_completes(&result);
}

#[test]
fn test_long_help() {
    common::assert_cmd(&["sdk", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sdk", "-h"], None, None);
}

#[test]
fn test_invalid_subcommand() {
    common::refute_cmd(&["sdk", "invalid"], None, None);
}
