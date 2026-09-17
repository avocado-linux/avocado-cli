//! Guard: both lifecycle runs actually inject the project's kernel cmdline.
//!
//! `kernel.cmdline` / `kernel.cmdline_extra` are only useful if they reach the
//! SDK's platform hook, and on a UKI target it is the *build* hook that bakes
//! the line into the image. That build-side call was deleted by an unrelated
//! merge (#264, on top of a base predating #252) and the whole suite stayed
//! green for three weeks: `effective_kernel_cmdline` is unit-tested, the
//! provision call site survived, and nothing covered the connection.
//!
//! The check lives out here rather than in a `mod tests` inside those files
//! because the needle would then appear in the file it scans and would hold
//! with the real call site deleted -- same reason `source_date_epoch_wiring.rs`
//! sits here.
//!
//! ponytail: pins the call's spelling, not its effect. Testing the effect means
//! lifting env-map construction out of the async run path in both commands;
//! worth doing when a third command wants the same value.

use std::fs;
use std::path::PathBuf;

/// The call every run that can shape the boot image has to make.
const INJECTION: &str = "inject_kernel_cmdline(";

fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn runtime_build_injects_kernel_cmdline() {
    assert!(
        source("src/commands/runtime/build.rs").contains(INJECTION),
        "`avocado build` must inject the kernel cmdline: the UKI is assembled by the \
         build hook, before `stone bundle`, so a provision-only export never reaches it"
    );
}

#[test]
fn runtime_provision_injects_kernel_cmdline() {
    assert!(
        source("src/commands/runtime/provision.rs").contains(INJECTION),
        "`avocado provision` must inject the kernel cmdline for targets that assemble \
         their boot image at provision time"
    );
}
