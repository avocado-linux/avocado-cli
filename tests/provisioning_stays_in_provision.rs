//! Guard: `avocado build` produces the OTA set only; the provisioning tail
//! (var image rendering, Docker priming, the stone provisioning hook, and
//! bundle packing/replay) belongs to `avocado provision`.
//!
//! Other commands may reach that tail only by running the provision command
//! itself (as `connect upload --provisionable` does), never by calling its
//! pieces. This keeps a build from quietly growing provisioning work and a
//! provision from depending on build-only state.
//!
//! If this fails: call `ProvisionCommand` (or `RuntimeProvisionCommand`)
//! instead of the function named below. Don't add an exemption.

use std::fs;
use std::path::{Path, PathBuf};

/// (function, files allowed to call it), paths relative to src/.
const PROVISION_ONLY: &[(&str, &[&str])] = &[
    // var_image.rs defines these and its own tests call them.
    (
        "render_var_image(",
        &[
            "commands/runtime/provision.rs",
            "commands/runtime/var_image.rs",
        ],
    ),
    (
        "render_var_image_from_staging(",
        &[
            "commands/runtime/provision.rs",
            "commands/runtime/var_image.rs",
        ],
    ),
    (
        "pack_tail(",
        &[
            "commands/runtime/provision.rs",
            "commands/provision_bundle.rs",
        ],
    ),
    (
        "hook_call(",
        &[
            "commands/runtime/provision.rs",
            "commands/provision_bundle.rs",
        ],
    ),
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
fn provisioning_tail_is_only_called_from_provision() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    let mut offenders = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for (func, allowed) in PROVISION_ONLY {
                // The definition itself (`fn pack_tail(`) is not a call.
                let calls =
                    code.matches(func).count() - code.matches(&format!("fn {func}")).count();
                if calls > 0 && !allowed.contains(&rel.as_str()) {
                    offenders.push(format!("src/{rel}:{}: calls {func}..)", n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "provisioning-tail functions called outside `avocado provision`:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn build_does_not_reference_provisioning_modules() {
    let build = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/runtime/build.rs");
    let text = fs::read_to_string(build).unwrap();
    // `var_image::render_ota_tail` is the OTA half and belongs to build.
    for forbidden in ["render_var_image", "provision_bundle", "ProvisionCommand"] {
        assert!(
            !text.contains(forbidden),
            "src/commands/runtime/build.rs references {forbidden}: provisioning belongs to `avocado provision`"
        );
    }
}
