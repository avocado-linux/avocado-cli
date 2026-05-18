//! Staging-dir discipline for `avocado vm update` and `vm reset-var`.
//!
//! The flow is invariant across both commands:
//!
//! 1. Stage new artifact bytes into `~/.avocado/vm/staging/<version>/`.
//! 2. Verify sha256 against the remote manifest as soon as each file
//!    finishes downloading. Fail-fast — never leave a partial file
//!    where a later step might consume it.
//! 3. Once every artifact in the batch is verified, atomic-rename each
//!    one into `~/.avocado/vm/` (the install dir).
//! 4. Update `~/.avocado/vm/manifest.json` last — that's the marker
//!    that says "this install is complete at this version."
//!
//! If any step before (3) fails, the staging dir is left in place so a
//! retry can resume rather than re-downloading. If step (3) fails
//! mid-way the install is half-swapped — the cli refuses to start the
//! VM and instructs the user to rerun the update.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// `<install_dir>/staging/<version>/`. Construct once per operation.
pub struct StagingDir {
    pub root: PathBuf,
    pub install_dir: PathBuf,
}

impl StagingDir {
    /// Resolve the staging dir for a given install dir + release
    /// version. Creates the directory.
    pub fn create(install_dir: &Path, version: &str) -> Result<Self> {
        let root = install_dir.join("staging").join(version);
        fs::create_dir_all(&root)
            .with_context(|| format!("creating staging dir at {}", root.display()))?;
        Ok(Self {
            root,
            install_dir: install_dir.to_path_buf(),
        })
    }

    /// Path to a per-file staging slot.
    pub fn slot(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }

    /// Verify the file at `slot(file)` matches `expected_sha256`
    /// (lowercase hex). Errors with a precise message including both
    /// values when it doesn't.
    pub fn verify_sha256(&self, file: &str, expected_sha256: &str) -> Result<()> {
        let path = self.slot(file);
        let got = sha256_file(&path)
            .with_context(|| format!("hashing staged file {}", path.display()))?;
        if !got.eq_ignore_ascii_case(expected_sha256) {
            bail!(
                "sha256 mismatch for staged {}: expected={}, got={}",
                file,
                expected_sha256,
                got,
            );
        }
        Ok(())
    }

    /// Atomic-rename a staged file into the install dir, overwriting
    /// any existing copy. POSIX `rename(2)` semantics — single
    /// filesystem operation, no half-state visible to readers.
    pub fn commit(&self, file: &str) -> Result<()> {
        let from = self.slot(file);
        let to = self.install_dir.join(file);
        fs::rename(&from, &to)
            .with_context(|| format!("renaming {} -> {}", from.display(), to.display(),))?;
        Ok(())
    }

    /// Best-effort cleanup of the staging dir after a successful
    /// commit. Removes the per-version dir; leaves the parent
    /// `staging/` dir alone so concurrent operations (none today,
    /// but cheap to be careful) don't trip.
    pub fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }

    /// Write a marker file recording whether the VM was running when
    /// the update started. Read on completion to decide whether to
    /// auto-restart the VM. Survives a crash mid-update so a retry
    /// preserves the intent.
    pub fn record_was_running(&self, was_running: bool) -> Result<()> {
        fs::write(
            self.root.join(".was-running"),
            if was_running { "1" } else { "0" },
        )
        .context("writing .was-running marker")
    }

    /// Read the marker. `false` if the file is missing. Consumed by a
    /// crash-resume retry to recover the restart intent set by the
    /// original (interrupted) operation.
    #[allow(dead_code)]
    pub fn was_running(&self) -> bool {
        fs::read_to_string(self.root.join(".was-running"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }
}

/// Compute lowercase-hex sha256 of `path`.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        out.push_str(&format!("{:02x}", b));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_replaces_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().to_path_buf();

        // Pre-existing artifact in the install dir.
        fs::write(install.join("Image"), b"old-bytes").unwrap();

        let stage = StagingDir::create(&install, "0.2.0").unwrap();
        fs::write(stage.slot("Image"), b"new-bytes").unwrap();
        stage.commit("Image").unwrap();

        let after = fs::read_to_string(install.join("Image")).unwrap();
        assert_eq!(after, "new-bytes");
        assert!(
            !stage.slot("Image").exists(),
            "staged file should be moved, not copied"
        );
    }

    #[test]
    fn verify_sha256_catches_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = StagingDir::create(tmp.path(), "0.2.0").unwrap();
        fs::write(stage.slot("blob"), b"hello").unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert!(stage
            .verify_sha256(
                "blob",
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            )
            .is_ok());
        assert!(stage
            .verify_sha256(
                "blob",
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err());
    }

    #[test]
    fn was_running_marker_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = StagingDir::create(tmp.path(), "0.2.0").unwrap();
        assert!(!stage.was_running()); // missing -> false
        stage.record_was_running(true).unwrap();
        assert!(stage.was_running());
        stage.record_was_running(false).unwrap();
        assert!(!stage.was_running());
    }
}
