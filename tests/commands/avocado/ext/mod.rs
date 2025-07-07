//! Tests for ext command.

pub mod build;
pub mod deps;
pub mod image;
pub mod list;

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["ext", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["ext", "-h"], None, None);
}

#[test]
fn test_invalid_subcommand() {
    common::refute_cmd(&["ext", "invalid"], None, None);
}
