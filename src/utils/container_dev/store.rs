//! Per-project content-addressed blob store for Container Dev Mode.
//!
//! Blobs are keyed by their OCI digest (`<algorithm>:<hex>`) and deduplicated
//! on write: a digest that is already present is never stored a second time.
//! Tags map to the digest of the manifest they point at.
//!
//! The store is namespaced per project at
//! `~/.avocado/container-dev/<project>/registry/`, so `prune` in one project
//! can never sweep another project's blobs (design D8, M5). GC/prune semantics
//! land in a later task; this module owns only the on-disk layout, the
//! content-addressed write/dedup path, and tag pointers.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use tempfile::NamedTempFile;
use thiserror::Error;

/// Errors returned by the blob store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The user's home directory could not be resolved.
    #[error("could not resolve the home directory for the container-dev store")]
    NoHome,
    /// A digest was not of the form `<algorithm>:<hex>` with a safe,
    /// non-traversing algorithm and hex component.
    #[error("invalid digest {0:?}: expected `<algorithm>:<hex>`")]
    InvalidDigest(String),
    /// A tag name contained a path separator or traversal component.
    #[error("invalid tag {0:?}: must not contain a path separator or `..`")]
    InvalidTag(String),
    /// An underlying filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A per-project content-addressed blob store.
///
/// Rooted at `<avocado_dir>/container-dev/<project>/registry/` with a
/// `blobs/<algorithm>/<hex>` layout for content and `manifests/tags/<tag>`
/// pointers holding the digest of the tagged manifest.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open the store for `project` under the user's home directory
    /// (`~/.avocado/container-dev/<project>/registry/`).
    pub fn for_project(project: &str) -> Result<Self, StoreError> {
        let base = BaseDirs::new().ok_or(StoreError::NoHome)?;
        let avocado_dir = base.home_dir().join(".avocado");
        Self::at(&avocado_dir, project)
    }

    /// Open the store for `project` rooted under an explicit `avocado_dir`
    /// (the `~/.avocado` equivalent).
    ///
    /// The per-project namespacing is derived here from `project`, which is
    /// what keeps one project's store isolated from another's.
    pub fn at(avocado_dir: &Path, project: &str) -> Result<Self, StoreError> {
        let root = avocado_dir
            .join("container-dev")
            .join(project)
            .join("registry");
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("manifests").join("tags"))?;
        Ok(Self { root })
    }

    /// The registry root directory backing this store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write `bytes` under `digest`.
    ///
    /// If a blob with this digest is already present the write is skipped and
    /// `Ok(false)` is returned (dedup); otherwise the blob is written
    /// atomically and `Ok(true)` is returned. Because the on-disk path is
    /// derived solely from the digest, a repeated digest can never produce a
    /// second copy.
    pub fn write_blob(&self, digest: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        let path = self.blob_path(digest)?;
        if path.exists() {
            return Ok(false);
        }
        let dir = path
            .parent()
            .expect("blob path always has a parent under the store root");
        fs::create_dir_all(dir)?;
        let mut tmp = NamedTempFile::new_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.flush()?;
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(true)
    }

    /// Report whether a blob with `digest` is present (the registry HEAD path).
    pub fn has_blob(&self, digest: &str) -> Result<bool, StoreError> {
        Ok(self.blob_path(digest)?.exists())
    }

    /// Read the bytes stored under `digest`, or `None` when absent.
    pub fn read_blob(&self, digest: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.blob_path(digest)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Point `tag` at the manifest identified by `manifest_digest`.
    ///
    /// The pointer is written atomically and overwrites any previous target
    /// for the tag.
    pub fn set_tag(&self, tag: &str, manifest_digest: &str) -> Result<(), StoreError> {
        // Validate the digest so a tag never points at a malformed target.
        parse_digest(manifest_digest)?;
        let path = self.tag_path(tag)?;
        let dir = path
            .parent()
            .expect("tag path always has a parent under the store root");
        fs::create_dir_all(dir)?;
        let mut tmp = NamedTempFile::new_in(dir)?;
        tmp.write_all(manifest_digest.as_bytes())?;
        tmp.flush()?;
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Resolve `tag` to the digest of the manifest it points at, or `None`
    /// when the tag is unknown.
    pub fn resolve_tag(&self, tag: &str) -> Result<Option<String>, StoreError> {
        let path = self.tag_path(tag)?;
        match fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, StoreError> {
        let (algorithm, hex) = parse_digest(digest)?;
        Ok(self.root.join("blobs").join(algorithm).join(hex))
    }

    fn tag_path(&self, tag: &str) -> Result<PathBuf, StoreError> {
        if tag.is_empty() || tag.contains('/') || tag.contains('\\') || tag.contains("..") {
            return Err(StoreError::InvalidTag(tag.to_string()));
        }
        Ok(self.root.join("manifests").join("tags").join(tag))
    }
}

/// Split an OCI digest into its `(algorithm, hex)` components, rejecting
/// anything that could traverse the filesystem.
fn parse_digest(digest: &str) -> Result<(&str, &str), StoreError> {
    let invalid = || StoreError::InvalidDigest(digest.to_string());
    let (algorithm, hex) = digest.split_once(':').ok_or_else(invalid)?;
    if algorithm.is_empty() || hex.is_empty() {
        return Err(invalid());
    }
    if !algorithm.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok((algorithm, hex))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn store_in(dir: &TempDir, project: &str) -> BlobStore {
        BlobStore::at(dir.path(), project).expect("store opens")
    }

    /// Count regular files under the store's `blobs/` tree.
    fn blob_file_count(store: &BlobStore) -> usize {
        walkdir::WalkDir::new(store.root().join("blobs"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .count()
    }

    #[test]
    fn store_path_is_per_project_not_global() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        let expected = dir
            .path()
            .join("container-dev")
            .join("alpha")
            .join("registry");
        assert_eq!(store.root(), expected.as_path());
        // The project name must appear in the path so two projects cannot
        // collide on one directory.
        assert!(store.root().components().any(|c| c.as_os_str() == "alpha"));
    }

    #[test]
    fn writing_the_same_digest_twice_stores_one_copy() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        let first = store.write_blob(DIGEST_A, b"layer-bytes").unwrap();
        assert!(first, "first write of a new digest stores the blob");

        let second = store.write_blob(DIGEST_A, b"layer-bytes").unwrap();
        assert!(!second, "a repeated digest write must be deduplicated");

        assert_eq!(
            blob_file_count(&store),
            1,
            "an existing-digest write must not store a second copy"
        );
        assert_eq!(
            store.read_blob(DIGEST_A).unwrap().as_deref(),
            Some(&b"layer-bytes"[..])
        );
    }

    #[test]
    fn dedup_does_not_clobber_existing_bytes_on_a_racing_rewrite() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        assert!(store.write_blob(DIGEST_A, b"original").unwrap());
        // A second write for the same digest is a no-op even if the caller
        // passes different bytes; the stored content is unchanged.
        assert!(!store.write_blob(DIGEST_A, b"different").unwrap());
        assert_eq!(
            store.read_blob(DIGEST_A).unwrap().as_deref(),
            Some(&b"original"[..])
        );
    }

    #[test]
    fn head_reports_present_only_for_written_digests() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        assert!(
            !store.has_blob(DIGEST_A).unwrap(),
            "an unwritten digest must report absent"
        );
        store.write_blob(DIGEST_A, b"data").unwrap();
        assert!(
            store.has_blob(DIGEST_A).unwrap(),
            "HEAD for an existing digest must report present"
        );
        assert!(
            !store.has_blob(DIGEST_B).unwrap(),
            "a different, unwritten digest must still report absent"
        );
    }

    #[test]
    fn one_projects_blobs_are_invisible_to_another_project() {
        let dir = TempDir::new().unwrap();
        let alpha = store_in(&dir, "alpha");
        let beta = store_in(&dir, "beta");

        alpha.write_blob(DIGEST_A, b"alpha-only").unwrap();

        assert!(
            alpha.has_blob(DIGEST_A).unwrap(),
            "alpha stored its own blob"
        );
        assert!(
            !beta.has_blob(DIGEST_A).unwrap(),
            "beta must not see alpha's blob (per-project namespacing)"
        );
        assert_eq!(blob_file_count(&beta), 0);
        assert_ne!(alpha.root(), beta.root());
    }

    #[test]
    fn tag_points_at_a_manifest_digest() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        assert_eq!(store.resolve_tag("dev").unwrap(), None);
        store.set_tag("dev", DIGEST_A).unwrap();
        assert_eq!(store.resolve_tag("dev").unwrap().as_deref(), Some(DIGEST_A));

        // Retagging overwrites the pointer, it does not append.
        store.set_tag("dev", DIGEST_B).unwrap();
        assert_eq!(store.resolve_tag("dev").unwrap().as_deref(), Some(DIGEST_B));
    }

    #[test]
    fn tags_are_isolated_per_project() {
        let dir = TempDir::new().unwrap();
        let alpha = store_in(&dir, "alpha");
        let beta = store_in(&dir, "beta");

        alpha.set_tag("dev", DIGEST_A).unwrap();
        assert_eq!(beta.resolve_tag("dev").unwrap(), None);
    }

    #[test]
    fn malformed_digests_are_rejected() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        for bad in [
            "noscheme",
            "sha256:",
            ":abcd",
            "sha256:zzzz",
            "sha256:aa/bb",
        ] {
            assert!(
                matches!(
                    store.write_blob(bad, b"x"),
                    Err(StoreError::InvalidDigest(_))
                ),
                "digest {bad:?} must be rejected"
            );
            assert!(matches!(
                store.has_blob(bad),
                Err(StoreError::InvalidDigest(_))
            ));
        }
    }

    #[test]
    fn digest_with_path_traversal_cannot_escape_the_store() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        // A traversal attempt in the hex component is rejected outright.
        assert!(matches!(
            store.write_blob("sha256:../../etc/passwd", b"x"),
            Err(StoreError::InvalidDigest(_))
        ));
    }

    #[test]
    fn tag_names_with_separators_are_rejected() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        for bad in ["../escape", "a/b", "..", ""] {
            assert!(
                matches!(store.set_tag(bad, DIGEST_A), Err(StoreError::InvalidTag(_))),
                "tag {bad:?} must be rejected"
            );
        }
    }
}
