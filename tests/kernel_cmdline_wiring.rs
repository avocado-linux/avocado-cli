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

/// ...and the two arguments that make it read the config as resolved FOR THIS
/// TARGET: the merged runtime section, and the raw composed document the
/// top-level `kernel:` block is resolved from. Each is an `Option`, so `None`
/// still compiles and still exports a line, just the unresolved one, and a
/// `target-<name>:` kernel override at that level goes back to vanishing with
/// every test green -- the same shape of silence this file exists to catch.
const RESOLVED_ARGS: [&str; 2] = ["merged_runtime.as_ref()", "Some(parsed)"];

fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The argument text of the one `inject_kernel_cmdline(` call in `relative`.
/// Scoped to the call: both needles are ordinary spellings elsewhere in these
/// files, so a whole-file `contains` would hold with the call passing `None`.
fn injection_args(relative: &str, must: &str) -> String {
    let src = source(relative);
    let start = src.find(INJECTION).unwrap_or_else(|| panic!("{must}"));
    let rest = &src[start + INJECTION.len()..];
    let end = rest
        .find(")?;")
        .expect("the injection call is terminated with `)?;`");
    rest[..end].to_string()
}

#[test]
fn runtime_build_injects_kernel_cmdline() {
    let args = injection_args(
        "src/commands/runtime/build.rs",
        "`avocado build` must inject the kernel cmdline: the UKI is assembled by the \
         build hook, before `stone bundle`, so a provision-only export never reaches it",
    );
    for arg in RESOLVED_ARGS {
        assert!(
            args.contains(arg),
            "`avocado build` must pass {arg}, or a `target-<name>:` kernel override at \
             that level is dropped and the UKI bakes the platform default"
        );
    }
}

#[test]
fn runtime_provision_injects_kernel_cmdline() {
    let args = injection_args(
        "src/commands/runtime/provision.rs",
        "`avocado provision` must inject the kernel cmdline for targets that assemble \
         their boot image at provision time",
    );
    for arg in RESOLVED_ARGS {
        assert!(
            args.contains(arg),
            "`avocado provision` must pass {arg}, so a per-target kernel override at \
             that level reaches the provision hook too"
        );
    }
}
