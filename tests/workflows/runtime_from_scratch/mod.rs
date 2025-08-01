//! Tests for running qemu from an empty project.

use crate::common::*;

#[test]
fn qemux86_64() {
    let workflow_dir = std::env::current_dir()
        .unwrap()
        .join("tests")
        .join("workflows")
        .join("runtime_from_scratch");

    assert_cmds(
        &[
            &["sdk", "run", "-v", "--", ":"],
            // &["sdk", "install", "-f"],
            // &["sdk", "compile"],
            // &["ext", "install", "ext-1"],
            // &["ext", "build", "ext-1"],
            // &["ext", "image", "ext-1"],
            // &["runtime", "build", "-f"],
        ],
        Some(&workflow_dir),
        None,
    );
}
