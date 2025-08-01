//! Tests for sdk install command.

use crate::common;
use std::env;

fn with_rust_cli<F>(test_fn: F)
where
    F: FnOnce(),
{
    // Set environment variable to use Rust CLI
    env::set_var("USE_RUST_CLI", "1");

    // Run the test
    test_fn();

    // Clean up
    env::remove_var("USE_RUST_CLI");
}

#[test]
fn test_long_help() {
    with_rust_cli(|| {
        common::assert_cmd(&["sdk", "install", "--help"], None, None);
    });
}

#[test]
fn test_short_help() {
    with_rust_cli(|| {
        common::assert_cmd(&["sdk", "install", "-h"], None, None);
    });
}
