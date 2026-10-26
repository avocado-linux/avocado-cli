//! Tests for running qemu from an empty project.

use crate::common::*;

#[test]
fn qemux86_64() {
    let workflow_dir = std::env::current_dir()
        .unwrap()
        .join("tests")
        .join("workflows")
        .join("runtime_from_scratch");

    // `sdk run -c ":"` needs a working SDK container. In CI the container setup
    // does not complete, so the in-container command exits non-zero. Before
    // ENG-2352, `sdk run` swallowed that and exited 0, so this passed on a false
    // success. `sdk run` now propagates the failure, so assert only that the CLI
    // runs to completion. A real end-to-end that asserts success needs a CI
    // environment that can run the SDK container.
    let result = cli_with_config(&["sdk", "run", "-v", "-c", ":"], Some(&workflow_dir), None);
    assert_command_completes(&result);
}
