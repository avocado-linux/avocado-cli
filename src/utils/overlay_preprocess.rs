//! Host-side materialization of preprocessed overlay trees.
//!
//! Overlay files are normally copied verbatim into the rootfs / initramfs /
//! extension sysroot by a shell `cp` that runs inside the SDK container,
//! reading the project tree bind-mounted at `/opt/src`. When an overlay opts
//! into preprocessing (`overlay: { dir, preprocess: ... }`), we must apply
//! `{{ ... }}` substitution to file contents *without* mutating the user's
//! working tree. To do that we materialize a processed copy on the host into a
//! scratch dir under the project root (`.avocado/overlay-staging/<label>/`,
//! already inside the `/opt/src` bind mount) and point the `cp` at that copy.
//!
//! Only local overlays (those living in the project tree on the host) can be
//! preprocessed; remote-extension overlays live inside the SDK volume and are
//! copied verbatim (callers warn + skip).

use anyhow::{Context, Result};
use serde_yaml::Value;
use std::path::Path;

use crate::utils::interpolation::{preprocess_text, AvocadoContext};

/// Scratch directory (relative to the project root) that holds materialized,
/// preprocessed overlay trees. Lives under `.avocado/`, which is gitignored
/// scratch after the lock file moved to the top-level `avocado.lock`.
const STAGING_SUBDIR: &str = ".avocado/overlay-staging";

/// Which overlay files to run the `{{ }}` preprocessor over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocessSpec {
    /// No preprocessing — copy the overlay verbatim (default, today's behavior).
    None,
    /// Preprocess every UTF-8 file in the overlay.
    All,
    /// Preprocess only files whose overlay-relative path matches one of these
    /// globs (`*` within a segment, `**` across segments, `?` a single char).
    Globs(Vec<String>),
}

impl PreprocessSpec {
    /// True when any preprocessing is requested.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, PreprocessSpec::None)
    }

    /// Parse the `preprocess` key from an `overlay:` mapping value. A bare
    /// overlay string (no mapping) or an absent/false `preprocess` yields
    /// [`PreprocessSpec::None`].
    pub fn from_overlay_value(overlay: &Value) -> Self {
        let Some(table) = overlay.as_mapping() else {
            return PreprocessSpec::None;
        };
        match table.get("preprocess") {
            Some(Value::Bool(true)) => PreprocessSpec::All,
            Some(Value::Sequence(seq)) => {
                let globs: Vec<String> = seq
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                if globs.is_empty() {
                    PreprocessSpec::None
                } else {
                    PreprocessSpec::Globs(globs)
                }
            }
            _ => PreprocessSpec::None,
        }
    }

    /// Whether the file at `rel_path` (overlay-relative, forward slashes) should
    /// be preprocessed.
    fn matches(&self, rel_path: &str) -> bool {
        match self {
            PreprocessSpec::None => false,
            PreprocessSpec::All => true,
            PreprocessSpec::Globs(globs) => globs.iter().any(|g| glob_matches(g, rel_path)),
        }
    }
}

/// Minimal glob matcher against a forward-slash relative path.
/// `**` matches across separators, `*` matches within a segment, `?` matches a
/// single non-separator character; all other characters match literally.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    re.push_str(".*");
                    // Swallow an immediately-following '/' so `**/x` also matches `x`.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            c if "\\.+()|[]{}^$".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re)
        .map(|r| r.is_match(path))
        .unwrap_or(false)
}

/// Bytes to emit for one overlay file: preprocessed when the spec selects it and
/// the content is valid UTF-8, otherwise the raw bytes unchanged. Binary files
/// (non-UTF-8) are never templated, so ELF/images/certs pass through intact.
fn process_file_bytes(
    rel_path: &str,
    raw: Vec<u8>,
    spec: &PreprocessSpec,
    root: &Value,
    context: &AvocadoContext,
) -> Result<Vec<u8>> {
    if !spec.matches(rel_path) {
        return Ok(raw);
    }
    match std::str::from_utf8(&raw) {
        Ok(text) => Ok(preprocess_text(text, root, context)
            .with_context(|| format!("Failed to preprocess overlay file '{rel_path}'"))?
            .into_bytes()),
        // Selected but not UTF-8 → copy verbatim (can't safely template).
        Err(_) => Ok(raw),
    }
}

/// Walk `overlay_src`, invoking `visit(rel_path, entry)` for every file and
/// symlink in a deterministic (sorted) order. Directories are visited via their
/// contained entries only.
fn sorted_entries(overlay_src: &Path) -> Vec<walkdir::DirEntry> {
    let mut entries: Vec<_> = walkdir::WalkDir::new(overlay_src)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    entries
}

/// Overlay-relative, forward-slash path. `Ok(None)` for the overlay root
/// itself. Errors on a non-UTF-8 path: staging and the digest are string-based,
/// so `to_string_lossy` would mangle a non-UTF-8 name to U+FFFD (and two such
/// names could collide / hash equal), unlike the byte-preserving verbatim `cp -a`
/// path. Reject explicitly rather than silently corrupt.
fn rel_str(overlay_src: &Path, path: &Path) -> Result<Option<String>> {
    let rel = match path.strip_prefix(overlay_src) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    if rel.as_os_str().is_empty() {
        return Ok(None);
    }
    match rel.to_str() {
        Some(s) => Ok(Some(s.replace('\\', "/"))),
        None => anyhow::bail!(
            "Overlay contains a non-UTF-8 path ({}); overlay preprocessing requires UTF-8 \
             filenames. Scope `preprocess` to specific globs or disable it for this overlay.",
            path.display()
        ),
    }
}

/// Compute a deterministic digest of the overlay tree *after* preprocessing,
/// without materializing it to disk. Returns `None` when preprocessing is
/// disabled or the overlay dir does not exist (so stamps are unchanged for the
/// verbatim path). Folded into the rootfs/initramfs/ext build input hashes so a
/// changed template value (e.g. a new claim token) or an edited overlay file
/// forces a rebuild. Only the SHA-256 is retained — never the plaintext.
pub fn overlay_content_digest(
    project_root: &Path,
    overlay_rel_dir: &str,
    spec: &PreprocessSpec,
    root: &Value,
    context: &AvocadoContext,
) -> Result<Option<String>> {
    if !spec.is_enabled() {
        return Ok(None);
    }
    let overlay_src = project_root.join(overlay_rel_dir);
    if !overlay_src.is_dir() {
        return Ok(None);
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for entry in sorted_entries(&overlay_src) {
        let Some(rel) = rel_str(&overlay_src, entry.path())? else {
            continue;
        };
        let ft = entry.file_type();
        if ft.is_dir() {
            // Fold directory modes so a permission drift (e.g. 0700 .ssh widened
            // to 0755) invalidates the stamp — materialize preserves dir modes.
            let mode = file_mode(entry.path());
            hasher.update(format!("D\0{rel}\0{mode:o}\0").as_bytes());
        } else if ft.is_symlink() {
            // Fail the same way `materialize` does on an unreadable link, so the
            // stamp check and the build can't disagree.
            let target = std::fs::read_link(entry.path()).with_context(|| {
                format!("Failed to read overlay symlink: {}", entry.path().display())
            })?;
            hasher.update(format!("L\0{rel}\0{}\0", target.to_string_lossy()).as_bytes());
        } else if ft.is_file() {
            // Propagate read/preprocess errors rather than silently dropping the
            // digest, which would let a broken overlay skip rebuild invalidation.
            let raw = std::fs::read(entry.path()).with_context(|| {
                format!("Failed to read overlay file: {}", entry.path().display())
            })?;
            let mode = file_mode(entry.path());
            let bytes = process_file_bytes(&rel, raw, spec, root, context)?;
            let file_hash = Sha256::digest(&bytes);
            hasher.update(format!("F\0{rel}\0{mode:o}\0").as_bytes());
            hasher.update(file_hash);
        }
    }
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in out.iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(Some(format!("sha256:{hex}")))
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

/// Materialize a preprocessed copy of `overlay_rel_dir` under
/// `.avocado/overlay-staging/<label>/` in the project root, returning the
/// staging dir *relative to the project root* (forward slashes) to hand the
/// in-container copy — joined onto `/opt/src`.
///
/// Returns `Ok(None)` when preprocessing is disabled or the overlay dir is
/// absent — callers then fall back to copying the original overlay verbatim.
/// The staging dir is recreated fresh on every call (previous contents are
/// removed), bounding secret residency to the most recent build; `.avocado/` is
/// gitignored scratch and cleared by `avocado clean`.
pub fn materialize_preprocessed_overlay(
    project_root: &Path,
    overlay_rel_dir: &str,
    label: &str,
    spec: &PreprocessSpec,
    root: &Value,
    context: &AvocadoContext,
) -> Result<Option<String>> {
    if !spec.is_enabled() {
        return Ok(None);
    }
    let overlay_src = project_root.join(overlay_rel_dir);
    if !overlay_src.is_dir() {
        // Missing overlay dir is reported by the copy script itself; nothing to stage.
        return Ok(None);
    }

    // Sanitize the label to a single safe path component (defense-in-depth):
    // the staging dir is later removed with `remove_dir_all`, so never let a
    // label containing path separators, `..`, or an absolute path escape the
    // staging root.
    let safe_label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let rel_dir = format!("{STAGING_SUBDIR}/{safe_label}");
    let staging_dir = project_root.join(&rel_dir);

    // Fresh staging tree each build.
    if staging_dir.exists() {
        std::fs::remove_dir_all(&staging_dir).with_context(|| {
            format!(
                "Failed to clear overlay staging dir: {}",
                staging_dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&staging_dir).with_context(|| {
        format!(
            "Failed to create overlay staging dir: {}",
            staging_dir.display()
        )
    })?;

    for entry in sorted_entries(&overlay_src) {
        let Some(rel) = rel_str(&overlay_src, entry.path())? else {
            continue;
        };
        let dest = staging_dir.join(&rel);
        let ft = entry.file_type();

        if ft.is_dir() {
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("Failed to create staging dir: {}", dest.display()))?;
            // Preserve the source directory mode (bare create_dir_all uses the
            // process umask, which would silently widen e.g. a 0700 dir to 0755).
            copy_mode(entry.path(), &dest);
        } else if ft.is_symlink() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create staging dir: {}", parent.display())
                })?;
            }
            let target = std::fs::read_link(entry.path()).with_context(|| {
                format!("Failed to read overlay symlink: {}", entry.path().display())
            })?;
            recreate_symlink(&target, &dest)?;
        } else if ft.is_file() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create staging dir: {}", parent.display())
                })?;
            }
            let raw = std::fs::read(entry.path()).with_context(|| {
                format!("Failed to read overlay file: {}", entry.path().display())
            })?;
            let bytes = process_file_bytes(&rel, raw, spec, root, context)?;
            std::fs::write(&dest, &bytes)
                .with_context(|| format!("Failed to write staged file: {}", dest.display()))?;
            copy_mode(entry.path(), &dest);
        }
    }

    Ok(Some(rel_dir))
}

#[cfg(unix)]
fn recreate_symlink(target: &Path, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, dest)
        .with_context(|| format!("Failed to create staged symlink: {}", dest.display()))
}

#[cfg(not(unix))]
fn recreate_symlink(_target: &Path, _dest: &Path) -> Result<()> {
    anyhow::bail!("Overlay preprocessing with symlinks is only supported on unix")
}

#[cfg(unix)]
fn copy_mode(src: &Path, dest: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(src) {
        let _ = std::fs::set_permissions(
            dest,
            std::fs::Permissions::from_mode(meta.permissions().mode()),
        );
    }
}

#[cfg(not(unix))]
fn copy_mode(_src: &Path, _dest: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AvocadoContext {
        AvocadoContext::from_main_config(&Value::Null, Some("qemux86-64"))
    }

    #[test]
    fn spec_parsing() {
        let all: Value = serde_yaml::from_str("dir: overlay\npreprocess: true").unwrap();
        assert_eq!(
            PreprocessSpec::from_overlay_value(&all),
            PreprocessSpec::All
        );

        let globs: Value =
            serde_yaml::from_str("dir: overlay\npreprocess:\n  - etc/x.toml").unwrap();
        assert_eq!(
            PreprocessSpec::from_overlay_value(&globs),
            PreprocessSpec::Globs(vec!["etc/x.toml".to_string()])
        );

        let off: Value = serde_yaml::from_str("dir: overlay").unwrap();
        assert_eq!(
            PreprocessSpec::from_overlay_value(&off),
            PreprocessSpec::None
        );

        let bare: Value = serde_yaml::from_str("\"overlay\"").unwrap();
        assert_eq!(
            PreprocessSpec::from_overlay_value(&bare),
            PreprocessSpec::None
        );
    }

    #[test]
    fn glob_matching() {
        assert!(glob_matches(
            "etc/avocado-conn/config.toml",
            "etc/avocado-conn/config.toml"
        ));
        assert!(glob_matches("etc/*.toml", "etc/config.toml"));
        assert!(!glob_matches("etc/*.toml", "etc/sub/config.toml"));
        assert!(glob_matches("**/*.toml", "etc/sub/config.toml"));
        assert!(glob_matches("**/config.toml", "config.toml"));
        assert!(!glob_matches("etc/*.toml", "etc/config.yaml"));
    }

    #[test]
    fn materialize_templates_selected_utf8_only_and_preserves_binary() {
        std::env::set_var("OVL_TEST_TOKEN", "s3cret");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let overlay = root.join("overlay/etc");
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::write(
            overlay.join("config.toml"),
            "token = \"{{ env.OVL_TEST_TOKEN }}\"\n",
        )
        .unwrap();
        // A non-selected file keeps its literal braces.
        std::fs::write(
            overlay.join("keep.txt"),
            "literal {{ env.OVL_TEST_TOKEN }}\n",
        )
        .unwrap();
        // A "binary" (invalid UTF-8) file, even if selected, is copied verbatim.
        std::fs::write(overlay.join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let spec = PreprocessSpec::Globs(vec!["etc/config.toml".into(), "etc/blob.bin".into()]);
        let out = materialize_preprocessed_overlay(
            root,
            "overlay",
            "rootfs",
            &spec,
            &Value::Null,
            &ctx(),
        )
        .unwrap()
        .expect("staging produced");

        let staged = root.join(&out);
        assert_eq!(
            std::fs::read_to_string(staged.join("etc/config.toml")).unwrap(),
            "token = \"s3cret\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("etc/keep.txt")).unwrap(),
            "literal {{ env.OVL_TEST_TOKEN }}\n"
        );
        assert_eq!(
            std::fs::read(staged.join("etc/blob.bin")).unwrap(),
            vec![0xff, 0xfe, 0x00, 0x01]
        );
    }

    #[test]
    fn foreign_templates_are_left_literal_not_aborted() {
        // A `preprocess: true` overlay may contain foreign `{{ }}` (Go/Jinja/
        // Helm) that isn't our env/config/avocado context. Those must pass
        // through untouched rather than aborting the build; only our templates
        // are substituted.
        std::env::set_var("OVL_KEEP_TOKEN", "resolved");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("overlay/etc")).unwrap();
        std::fs::write(
            root.join("overlay/etc/tmpl.conf"),
            "a={{ .Values.name }} b={{ range .x }} c={{ env.OVL_KEEP_TOKEN }}\n",
        )
        .unwrap();

        let spec = PreprocessSpec::All;
        let out = materialize_preprocessed_overlay(
            root,
            "overlay",
            "rootfs",
            &spec,
            &Value::Null,
            &ctx(),
        )
        .unwrap()
        .expect("staging produced");

        // Foreign templates preserved verbatim; our `{{ env.* }}` substituted.
        assert_eq!(
            std::fs::read_to_string(root.join(&out).join("etc/tmpl.conf")).unwrap(),
            "a={{ .Values.name }} b={{ range .x }} c=resolved\n"
        );
    }

    #[test]
    fn digest_changes_with_template_value() {
        std::env::set_var("OVL_DIGEST_TOKEN", "aaa");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("overlay/etc")).unwrap();
        std::fs::write(
            root.join("overlay/etc/config.toml"),
            "token = \"{{ env.OVL_DIGEST_TOKEN }}\"\n",
        )
        .unwrap();
        let spec = PreprocessSpec::Globs(vec!["etc/config.toml".into()]);

        let h1 = overlay_content_digest(root, "overlay", &spec, &Value::Null, &ctx())
            .unwrap()
            .unwrap();
        std::env::set_var("OVL_DIGEST_TOKEN", "bbb");
        let h2 = overlay_content_digest(root, "overlay", &spec, &Value::Null, &ctx())
            .unwrap()
            .unwrap();
        assert_ne!(h1, h2, "digest must change when a templated value changes");

        // Disabled spec yields no digest (verbatim path unchanged).
        assert!(overlay_content_digest(
            root,
            "overlay",
            &PreprocessSpec::None,
            &Value::Null,
            &ctx()
        )
        .unwrap()
        .is_none());
    }
}
