//! Tests for cve command.

pub mod report;

use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["cve", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["cve", "-h"], None, None);
}

#[test]
fn test_no_subcommand_fails() {
    common::refute_cmd(&["cve"], None, None);
}
