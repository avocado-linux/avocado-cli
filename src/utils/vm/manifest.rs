//! `direct`-profile manifest parsing + sha256 verification.
//!
//! The `direct` provisioning profile (see avocado-os
//! `meta-avocado-qemu/stone/qemu*/stone-provision-direct.sh`) writes a
//! `manifest.json` alongside the artifacts. This module parses that file
//! and verifies the artifacts' sha256 before QEMU is launched against them.
//!
//! The contract is intentionally small: every field the CLI consumes is
//! read here; anything not understood is ignored. New optional fields can
//! be added to the script without touching the CLI.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Whole `manifest.json` document.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Always `"avocado-direct"` today.
    pub format: String,
    pub format_version: u32,
    /// Release version this manifest describes (e.g. `"0.1.0"`). Added
    /// in the 0.1.0 release contract; absent on pre-release installs.
    #[serde(default)]
    pub version: Option<String>,
    /// ISO-8601 UTC of when this release was built. Display-only.
    #[serde(default)]
    #[allow(dead_code)]
    pub released_at: Option<String>,
    pub platform: String,
    pub architecture: String,
    pub artifacts: HashMap<String, Artifact>,
    /// Default kernel cmdline (CLI may override or extend). Consumed by
    /// the qemu arg builder on Linux; unused on macOS where the app
    /// owns the spawn.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub cmdline_default: String,
    /// Hints the CLI may consult; non-binding. Reserved for Phase 4+.
    #[serde(default)]
    #[allow(dead_code)]
    pub qemu_hint: Option<serde_json::Value>,
}

/// Per-artifact policy for what `avocado vm update` does with this
/// entry when the local sha doesn't match the remote sha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    /// Download + atomic-swap on update. Used for kernel, initramfs,
    /// rootfs — stateless, regenerable from the release artifacts.
    Replace,
    /// Install on first run only; skip on subsequent updates so user
    /// state in the artifact (Docker volumes, container caches, project
    /// work in /data) survives image bumps. Used for `var`. Refreshed
    /// only via the explicit `avocado vm reset-var` command.
    SeedOnly,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        // Absent field on legacy manifests → treat as replaceable. The
        // remote (post-0.1.0) manifests always carry the field; this
        // default only matters for installed manifests written by an
        // older CLI.
        Self::Replace
    }
}

/// One staged artifact: `kernel`, `initramfs`, `rootfs`, `var`, etc.
#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    /// Filename relative to the manifest directory.
    pub file: String,
    /// sha256 of the file (lowercase hex).
    pub sha256: String,
    /// File size in bytes. Added in the 0.1.0 release contract for
    /// display + progress-bar use; absent on pre-release installs.
    #[serde(default)]
    #[allow(dead_code)]
    pub size: Option<u64>,
    /// Per-artifact update policy. See [`UpdatePolicy`]. Defaults to
    /// `Replace` if absent (legacy manifests).
    #[serde(default)]
    pub update_policy: UpdatePolicy,
    /// Kind hint — `kernel`, `initramfs-cpio-zst`, `erofs-lz4`, `btrfs`, …
    /// Carried for forward-compat — CLI doesn't dispatch on it yet.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: String,
    /// Optional symlink alias with the canonical role name (e.g. `kernel`).
    #[serde(default)]
    #[allow(dead_code)]
    pub role_link: Option<String>,
}

impl Manifest {
    /// Parse a `manifest.json` from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest at {}", path.display()))?;
        let m: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse manifest at {}", path.display()))?;
        if m.format != "avocado-direct" {
            bail!(
                "manifest format is '{}', expected 'avocado-direct'",
                m.format
            );
        }
        if m.format_version != 1 {
            bail!(
                "manifest format_version is {}, this CLI only understands 1",
                m.format_version
            );
        }
        Ok(m)
    }

    /// Verify every artifact's sha256 against the file on disk.
    /// `artifact_dir` is where the artifacts live (typically same dir as the
    /// manifest); each `artifact.file` is resolved relative to it.
    pub fn verify_all(&self, artifact_dir: &Path) -> Result<()> {
        for (role, art) in &self.artifacts {
            let path = artifact_dir.join(&art.file);
            let computed = sha256_file(&path)
                .with_context(|| format!("hashing {} for role '{role}'", path.display()))?;
            if !computed.eq_ignore_ascii_case(&art.sha256) {
                bail!(
                    "sha256 mismatch for role '{role}' ({}): manifest={}, computed={}",
                    art.file,
                    art.sha256,
                    computed,
                );
            }
        }
        Ok(())
    }

    /// Lookup an artifact by role name (e.g. `"kernel"`, `"rootfs"`).
    pub fn artifact(&self, role: &str) -> Option<&Artifact> {
        self.artifacts.get(role)
    }

    /// Resolve the on-disk path to an artifact's file.
    pub fn artifact_path(&self, role: &str, artifact_dir: &Path) -> Option<PathBuf> {
        self.artifact(role).map(|a| artifact_dir.join(&a.file))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "format": "avocado-direct",
            "format_version": 1,
            "platform": "avocado-qemuarm64",
            "architecture": "arm64",
            "artifacts": {
                "kernel": { "file": "Image", "sha256": "deadbeef", "type": "kernel" }
            },
            "cmdline_default": "console=ttyAMA0 root=/dev/vda rw"
        }"#;
        let path = tmp.path().join("manifest.json");
        std::fs::write(&path, raw).unwrap();
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.architecture, "arm64");
        assert!(m.artifact("kernel").is_some());
        assert_eq!(m.artifact("kernel").unwrap().sha256, "deadbeef");
    }

    #[test]
    fn rejects_wrong_format_version() {
        let raw = r#"{
            "format": "avocado-direct",
            "format_version": 99,
            "platform": "p",
            "architecture": "arm64",
            "artifacts": {},
            "cmdline_default": ""
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        std::fs::write(&path, raw).unwrap();
        let err = Manifest::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("format_version"));
    }

    #[test]
    fn verify_all_catches_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a file
        let kernel_path = tmp.path().join("Image");
        std::fs::write(&kernel_path, b"hello world").unwrap();

        // Manifest claiming the wrong sha
        let raw = r#"{
            "format": "avocado-direct",
            "format_version": 1,
            "platform": "p",
            "architecture": "arm64",
            "artifacts": {
                "kernel": { "file": "Image", "sha256": "0000000000000000000000000000000000000000000000000000000000000000", "type": "kernel" }
            },
            "cmdline_default": ""
        }"#;
        let mp = tmp.path().join("manifest.json");
        std::fs::write(&mp, raw).unwrap();
        let m = Manifest::load(&mp).unwrap();
        let err = m.verify_all(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"));
    }

    #[test]
    fn verify_all_accepts_correct_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let kernel_path = tmp.path().join("Image");
        let payload = b"hello world";
        std::fs::write(&kernel_path, payload).unwrap();

        // Compute the real sha
        let real = {
            let mut h = Sha256::new();
            h.update(payload);
            hex_lower(&h.finalize())
        };

        let raw = format!(
            r#"{{
            "format": "avocado-direct",
            "format_version": 1,
            "platform": "p",
            "architecture": "arm64",
            "artifacts": {{
                "kernel": {{ "file": "Image", "sha256": "{real}", "type": "kernel" }}
            }},
            "cmdline_default": ""
        }}"#
        );
        let mp = tmp.path().join("manifest.json");
        std::fs::write(&mp, raw).unwrap();
        let m = Manifest::load(&mp).unwrap();
        m.verify_all(tmp.path()).unwrap();
    }
}
