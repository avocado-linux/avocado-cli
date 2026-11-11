//! GNU `tar` and `sed` for tests that run a generated script on the host.
//!
//! The scripts are written for the GNU tools in the SDK container. A macOS
//! host has bsdtar, which has no `--no-overwrite-dir`, and BSD sed, which
//! rejects `\{` in an `-E` pattern. On macOS these tests link Homebrew's
//! `gtar` and `gsed` into their stub `bin` dir as `tar` and `sed`, and skip
//! when those are not installed. Other hosts use the system tools. The stamps
//! tests shim BSD sed instead: see `host_shell_prelude` in `stamps.rs`.

use std::path::{Path, PathBuf};

/// (name the script calls, GNU program on a macOS host)
const TOOLS: [(&str, &str); 2] = [("tar", "gtar"), ("sed", "gsed")];

/// Absolute path of `program` on `PATH`. A relative `PATH` entry would give a
/// link in the stub `bin` dir that points nowhere, so the result is resolved.
fn find_on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
}

/// True on a macOS host without `gtar` or `gsed`, after printing how to
/// install them; the test then returns early. Always false elsewhere.
pub(crate) fn missing() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let absent: Vec<&str> = TOOLS
        .iter()
        .map(|(_, gnu)| *gnu)
        .filter(|gnu| find_on_path(gnu).is_none())
        .collect();
    if absent.is_empty() {
        return false;
    }
    eprintln!(
        "{} not on PATH, skipping (brew install gnu-tar gnu-sed)",
        absent.join(" and ")
    );
    true
}

/// On macOS, link `gtar` and `gsed` into `bin` as `tar` and `sed`, so a
/// script run with `bin` first on `PATH` gets the GNU tools. Does nothing
/// elsewhere. A test checks [`missing`] before it gets here.
pub(crate) fn link_into(bin: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    std::fs::create_dir_all(bin).unwrap();
    for (name, gnu) in TOOLS {
        let program = find_on_path(gnu).unwrap_or_else(|| panic!("{gnu} not on PATH"));
        let link = bin.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(program, link).unwrap();
    }
}
