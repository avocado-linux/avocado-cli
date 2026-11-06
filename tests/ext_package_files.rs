//! `avocado.yaml`'s `package_files` must stage every file the crate reads at
//! compile time.
//!
//! The avocado-ext-cli extension compiles avocado-cli from the files
//! `ext package` stages, and nothing builds that staged tree in CI. A directory
//! reached by `include_str!` or `include_bytes!` from outside `src/` but missing
//! from `package_files` therefore breaks `avocado-cli-compile` only for whoever
//! installs the published extension, which is how `schemas/` shipped missing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

const MACROS: [&str; 2] = ["include_str!(\"", "include_bytes!(\""];

/// The string-literal path of every `include_str!` / `include_bytes!` in `source`.
fn include_paths(source: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for marker in MACROS {
        let mut rest = source;
        while let Some(start) = rest.find(marker) {
            rest = &rest[start + marker.len()..];
            if let Some(end) = rest.find('"') {
                paths.push(&rest[..end]);
            }
        }
    }
    paths
}

/// Join `relative` onto `base` and fold `.` and `..`, without touching the filesystem.
fn normalize(base: &Path, relative: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in base.join(relative).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Top-level entries outside `src/` that the sources include, relative to `root`.
fn external_includes(root: &Path) -> BTreeSet<String> {
    let src = root.join("src");
    let mut found = BTreeSet::new();
    for entry in WalkDir::new(&src)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let source = fs::read_to_string(entry.path()).expect("read source file");
        let dir = entry.path().parent().expect("source file has a parent");
        for path in include_paths(&source) {
            let resolved = normalize(dir, path);
            let Ok(relative) = resolved.strip_prefix(root) else {
                continue;
            };
            let mut parts = relative.components();
            let Some(Component::Normal(first)) = parts.next() else {
                continue;
            };
            if first != "src" {
                found.insert(first.to_string_lossy().into_owned());
            }
        }
    }
    found
}

fn package_files(root: &Path) -> BTreeSet<String> {
    let yaml = fs::read_to_string(root.join("avocado.yaml")).expect("read avocado.yaml");
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("avocado.yaml parses");
    doc["extensions"]["avocado-ext-cli"]["package_files"]
        .as_sequence()
        .expect("extensions.avocado-ext-cli.package_files is a list")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("package_files entry is a string")
                .to_owned()
        })
        .collect()
}

#[test]
fn package_files_stage_every_directory_the_sources_include() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let listed = package_files(root);
    let missing: Vec<String> = external_includes(root)
        .into_iter()
        .filter(|dir| !listed.contains(dir))
        .collect();

    assert!(
        missing.is_empty(),
        "src/ includes files from {missing:?} at compile time, but avocado.yaml's \
         extensions.avocado-ext-cli.package_files does not stage them, so the \
         published extension cannot compile (listed: {listed:?})"
    );
}

#[test]
fn include_paths_reads_both_macros_and_skips_unrelated_text() {
    let source = r#"
        const A: &str = include_str!("../../schemas/a.json");
        const B: &[u8] = include_bytes!("data/blob.bin");
        // include_str is mentioned here without a path
        let c = include_str!("install.rs");
    "#;

    let mut paths = include_paths(source);
    paths.sort_unstable();

    assert_eq!(
        paths,
        ["../../schemas/a.json", "data/blob.bin", "install.rs"]
    );
}

#[test]
fn normalize_resolves_parent_components_against_the_including_file() {
    let base = Path::new("/repo/src/utils");

    assert_eq!(
        normalize(base, "../../schemas/avocado-config.json"),
        PathBuf::from("/repo/schemas/avocado-config.json")
    );
    assert_eq!(
        normalize(base, "testdata/v7.lock"),
        PathBuf::from("/repo/src/utils/testdata/v7.lock")
    );
}
