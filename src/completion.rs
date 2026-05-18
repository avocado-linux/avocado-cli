//! Dynamic shell completion candidates.
//!
//! Each function returns the completion candidates for one kind of value
//! (extension name, runtime name, target, signing-key name). They are wired
//! into the clap tree in `main::attach_completers` and invoked by the
//! `clap_complete` engine on TAB press.
//!
//! Constraints these functions must satisfy:
//!
//! - **Fast.** They run on every TAB press in an interactive shell.
//!   Target sub-100ms wall time. No docker, no network, no SDK container
//!   spin-up, no expensive config composition.
//! - **Tolerant of missing state.** If `avocado.yaml` is absent or
//!   malformed, return an empty vec — never panic, never print errors.
//!   The shell silently shows no candidates, which is the right UX.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::CompletionCandidate;

/// Walk up from the current directory looking for an `avocado.yaml`.
/// Stops at the first match; returns `None` if we hit the filesystem root
/// without finding one. Mirrors how the rest of the CLI resolves the
/// project file when `-C` isn't passed.
fn find_avocado_yaml() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("avocado.yaml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Parse `avocado.yaml` as a plain YAML map (no include resolution, no
/// interpolation) and return the keys under `section`. This is the cheap
/// path: full `Config::load_composed` would hit the SDK container.
fn read_top_level_keys(yaml_path: &Path, section: &str) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(yaml_path) else {
        return vec![];
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return vec![];
    };
    let Some(map) = value.get(section).and_then(|v| v.as_mapping()) else {
        return vec![];
    };
    map.keys()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect()
}

fn filter(names: Vec<String>, current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    names
        .into_iter()
        .filter(|n| n.starts_with(&*prefix))
        .map(CompletionCandidate::new)
        .collect()
}

pub fn extensions(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(path) = find_avocado_yaml() else {
        return vec![];
    };
    filter(read_top_level_keys(&path, "extensions"), current)
}

pub fn runtimes(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(path) = find_avocado_yaml() else {
        return vec![];
    };
    // `runtime` is the legacy singular alias accepted by Config's deserializer.
    let mut names = read_top_level_keys(&path, "runtimes");
    if names.is_empty() {
        names = read_top_level_keys(&path, "runtime");
    }
    filter(names, current)
}

/// Target completion reads `supported_targets` from `avocado.yaml`. When
/// it's absent or `"*"` (meaning "all"), we can't enumerate without the
/// SDK, so we yield nothing rather than guess.
pub fn targets(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(path) = find_avocado_yaml() else {
        return vec![];
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return vec![];
    };
    let names = match value.get("supported_targets") {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    };
    filter(names, current)
}

pub fn signing_keys(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(registry) = crate::utils::signing_keys::KeysRegistry::load() else {
        return vec![];
    };
    filter(registry.keys.keys().cloned().collect(), current)
}
