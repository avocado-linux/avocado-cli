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

/// The SDKIMGARCH arch repair logic in the entrypoint setup script ensures that
/// (target_underscored)_(host_arch)_avocadosdk is always first in
/// $AVOCADO_SDK_PREFIX/etc/dnf/vars/arch, overriding any package post-install
/// scripts that register a generic host-arch-only alternative via update-alternatives.
/// The unit-level coverage for this lives in container.rs (test_entrypoint_script_sdkimgarch_repair).
#[test]
fn test_sdk_install_help_sdkimgarch() {
    with_rust_cli(|| {
        // Verify the sdk install subcommand is reachable; this guards against
        // regressions introduced while adding the SDKIMGARCH init changes.
        common::assert_cmd(&["sdk", "install", "--help"], None, None);
    });
}
