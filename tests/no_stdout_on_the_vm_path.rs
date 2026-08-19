//! Guard: nothing under `src/utils/vm` writes to stdout.
//!
//! `ensure_routed_for_process` runs before the command dispatches, and on the
//! auto-start branch it calls `lifecycle::start`, which has its own diagnostics
//! for the non-fatal hiccups (btrfs resize, guest network config, docker socket
//! forward, hibernation supervisor). Whichever command follows may own stdout —
//! `avocado sbom > sbom.json` writes an SPDX document there — so a `[WARNING]`
//! line from any of them leaves a file no consumer can parse.
//!
//! Moving `route.rs`'s own notices to stderr did not close that: the auto-start
//! it kicks off four lines later still printed through `print_warning`, which is
//! `println!`. Every emitter reachable from this path has to be stderr, not just
//! the outermost one — which is why this is a guard over the directory rather
//! than a review note on one file.
//!
//! If this fails: use `print_warning_stderr` / `print_info_stderr`. They route
//! through `print_notice_above`, so an active renderer still gets the notice
//! above its task list instead of having it painted over.

use std::fs;
use std::path::{Path, PathBuf};

/// Spellings that put a line on stdout. `print_plain` and `print_debug` are
/// absent on purpose — both are already `eprintln!`.
///
/// `print_warning_above` belongs here despite the name: its plain arm is
/// `println!`, so on the ordinary no-renderer no-json path it is a stdout
/// emitter. It is also the nearest neighbour of `print_warning_stderr` — the
/// two differ only in that closure — so it is the spelling a "these are
/// duplicates, merge them" refactor would most plausibly land on.
///
/// The macros and the raw handle are here because the guard's claim is about
/// stdout, not about a particular set of helper functions; a check that only
/// knows the helpers is one `print!` away from being wrong while still green.
///
/// `emit_json_event` / `emit_json_object` are the subtle pair. They write to
/// stdout via `stdout().lock()`, so they match none of the spellings above, and
/// they are what `print_stderr_notice`'s doc points a future author toward. The
/// distinction the guard is enforcing is that the JSON-mode check lives *in*
/// `print_stderr_notice`: called directly from here they emit a bare JSON line
/// onto the stdout of a human-output run, which is the same corrupt-`sbom.json`
/// failure in a different costume. Go through the printer.
const NEEDLES: [&str; 9] = [
    "println!",
    "print!(",
    "stdout()",
    "print_warning(",
    "print_warning_above(",
    "print_info(",
    "print_success(",
    "emit_json_event(",
    "emit_json_object(",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_vm_path_never_writes_to_stdout() {
    let vm = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/utils/vm");
    let mut files = Vec::new();
    rust_sources(&vm, &mut files);
    assert!(!files.is_empty(), "found no sources under {}", vm.display());

    let mut violations = Vec::new();

    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            // Comments may legitimately name these functions — this file's own
            // module docs do.
            if line.trim_start().starts_with("//") {
                continue;
            }
            // Substring traps, the first two of which flagged correct code on
            // the first run of this guard: `eprintln!` ends in `println!`,
            // `eprint!` ends in `print!`, and `print_warning_stderr` starts
            // with `print_warning`. Blanking the stderr macros handles the
            // first two; the open paren in the needles handles the third.
            let line = line.replace("eprintln!", "").replace("eprint!", "");
            if let Some(needle) = NEEDLES.into_iter().find(|n| line.contains(n)) {
                violations.push(format!("{}:{}: {needle}", file.display(), i + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "the VM path runs before the command dispatches, and the command that \
         follows may own stdout. Use print_warning_stderr / print_info_stderr. \
         Offending sites:\n  {}",
        violations.join("\n  ")
    );
}
