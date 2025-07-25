//! Tests for runtime command.

pub mod build;
pub mod deps;
pub mod list;

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["runtime", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["runtime", "-h"], None, None);
}

#[test]
fn test_invalid_subcommand() {
    common::refute_cmd(&["runtime", "invalid"], None, None);
}
