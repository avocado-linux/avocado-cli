//! Per-project content-addressed blob store for Container Dev Mode.
//!
//! Blobs are keyed by their OCI digest (`<algorithm>:<hex>`) and deduplicated
//! on write: a digest that is already present is never stored a second time.
//! Tags map to the digest of the manifest they point at.
//!
//! The store is namespaced per project at
//! `~/.avocado/container-dev/<project>/registry/`, so `prune` in one project
//! can never sweep another project's blobs (design D8, M5). Garbage collection
//! runs only on `prune`/`down` (never mid-push, never on a timer), retains any
//! blob referenced by a currently-tagged manifest, and `prune` refuses while a
//! device is mid-pull (design D8, threat-model M2).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sha2::{Digest as _, Sha256};

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
    /// `prune` was invoked while a device pull was still in flight.
    #[error("prune refused: a device is mid-pull")]
    PruneWhilePulling,
    /// A streamed blob grew past [`MAX_BLOB_BYTES`].
    #[error("blob exceeds the {limit}-byte ceiling (reached {attempted} bytes)")]
    BlobTooLarge { limit: u64, attempted: u64 },
    /// An underlying filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Ceiling on a single streamed blob.
///
/// Not a memory bound - blobs stream to disk and are never held whole. This
/// bounds DISK, which streaming otherwise left completely unbounded: without it
/// a write-token holder can PATCH forever, or an accidental oversized layer can
/// fill the filesystem and take down every process on the host. 32 GiB is far
/// above any layer a dev loop produces and far below a disk-filling one.
///
/// It also bounds the read side as a side effect: `serve_blob` still loads a blob
/// whole to serve it, so a blob that cannot be stored cannot later OOM the pull.
pub const MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// A per-project content-addressed blob store.
///
/// Rooted at `<avocado_dir>/container-dev/<project>/registry/` with a
/// `blobs/<algorithm>/<hex>` layout for content and `manifests/tags/<tag>`
/// pointers holding the digest of the tagged manifest.
pub struct BlobStore {
    root: PathBuf,
    /// Count of device pulls currently in flight; `prune` refuses while it is
    /// non-zero so a blob a pull still needs is never swept mid-transfer.
    in_flight_pulls: Arc<AtomicUsize>,
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
        Ok(Self {
            root,
            in_flight_pulls: Arc::new(AtomicUsize::new(0)),
        })
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

    /// Begin a streaming blob upload.
    ///
    /// The returned [`BlobUpload`] writes straight to a temp file under the
    /// store and hashes as it goes, so a layer never has to exist in memory. It
    /// is what lets the write path accept a multi-gigabyte layer without a body
    /// limit standing in for a memory bound - the OCI upload protocol is
    /// chunked, so the same handle spans the `POST`/`PATCH`/`PUT` sequence.
    pub fn begin_blob_upload(&self) -> Result<BlobUpload, StoreError> {
        let dir = self.root.join("uploads");
        fs::create_dir_all(&dir)?;
        Ok(BlobUpload {
            file: NamedTempFile::new_in(&dir)?,
            hasher: Sha256::new(),
            written: 0,
            blobs_root: self.root.clone(),
        })
    }

    /// Report whether a blob with `digest` is present (the registry HEAD path).
    pub fn has_blob(&self, digest: &str) -> Result<bool, StoreError> {
        Ok(self.blob_path(digest)?.exists())
    }

    /// Report the size in bytes of the blob under `digest`, or `None` when
    /// absent.
    ///
    /// The registry's HEAD dedup probe needs only the length, and `docker push`
    /// issues one HEAD per layer before uploading anything. Answering that from
    /// the directory entry keeps a multi-hundred-MB layer off the heap on the
    /// hot push path, which `read_blob` could not.
    pub fn blob_size(&self, digest: &str) -> Result<Option<u64>, StoreError> {
        let path = self.blob_path(digest)?;
        match fs::metadata(&path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
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

    /// Register the start of a device pull.
    ///
    /// The returned [`PullGuard`] keeps the pull counted as in-flight until it
    /// is dropped; [`prune`](Self::prune) refuses while any guard is alive so a
    /// blob the pull still needs is never swept out from under it.
    pub fn begin_pull(&self) -> PullGuard {
        self.in_flight_pulls.fetch_add(1, Ordering::SeqCst);
        PullGuard {
            counter: Arc::clone(&self.in_flight_pulls),
        }
    }

    /// The number of device pulls currently in flight.
    pub fn pulls_in_flight(&self) -> usize {
        self.in_flight_pulls.load(Ordering::SeqCst)
    }

    /// Garbage-collect blobs unreferenced by any currently-tagged manifest.
    ///
    /// This is the ONLY sweep path in the store; it is invoked from `down`
    /// (and, via [`prune`](Self::prune), from `prune`) — never from a
    /// push/sync and never on a timer. Every blob reachable from a
    /// currently-set tag (the manifest, its config, its layers, and, for a
    /// multi-arch index, each sub-manifest transitively) is retained; all
    /// other blobs are removed. Returns the digests that were swept.
    pub fn collect_garbage(&self) -> Result<Vec<String>, StoreError> {
        let reachable = self.reachable_digests()?;
        let mut swept = Vec::new();
        for digest in self.present_blob_digests()? {
            if reachable.contains(&digest) {
                continue;
            }
            let path = self.blob_path(&digest)?;
            match fs::remove_file(&path) {
                Ok(()) => swept.push(digest),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        swept.sort();
        Ok(swept)
    }

    /// `prune`: garbage-collect the per-project store, refusing while a device
    /// is mid-pull.
    ///
    /// Single policy (design D8, threat-model M2): GC runs only on
    /// `prune`/`down`, retains any blob referenced by a currently-tagged
    /// manifest, and `prune` refuses (rather than sweeping a blob the pull
    /// still needs) while a device is mid-pull.
    pub fn prune(&self) -> Result<Vec<String>, StoreError> {
        if self.pulls_in_flight() > 0 {
            return Err(StoreError::PruneWhilePulling);
        }
        // Sweep abandoned staging files too. `collect_garbage` walks `blobs/`
        // only, so nothing in the tree ever looked at `uploads/`. A `NamedTempFile`
        // unlinks itself on drop, which covers a clean exit and nothing else: an
        // `up` SIGKILLed mid-push (OOM reaper, power loss) leaves its partial
        // layer there permanently, and `prune` used to report "swept 0" while
        // gigabytes sat in a directory the user had to find by hand.
        // Callers that want to report the reclaimed count call `sweep_uploads`
        // directly; `prune`'s return stays a digest list, since a staging file was
        // never content-addressed and has no digest to name.
        self.sweep_uploads()?;
        self.collect_garbage()
    }

    /// Remove every staged upload file, returning how many were reclaimed.
    ///
    /// Safe to call from `prune` because `prune` already refused to run with a
    /// pull in flight, and a staging file belonging to a live upload is held by a
    /// session in the write router's map - which only exists while `up` is
    /// running, the same process that would be serving that pull.
    pub fn sweep_uploads(&self) -> Result<usize, StoreError> {
        let dir = self.root.join("uploads");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // Never opened an upload in this project: nothing to sweep.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The set of blob digests reachable from any currently-set tag.
    fn reachable_digests(&self) -> Result<HashSet<String>, StoreError> {
        let mut reachable: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = Vec::new();
        for tag in self.list_tags()? {
            if let Some(manifest_digest) = self.resolve_tag(&tag)? {
                stack.push(manifest_digest);
            }
        }
        while let Some(digest) = stack.pop() {
            if !reachable.insert(digest.clone()) {
                continue;
            }
            // A manifest is itself stored as a blob; read it and, when it
            // parses as a manifest or index, follow its references. An ordinary
            // layer blob is not JSON and yields no children.
            let bytes = match self.read_blob(&digest) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(StoreError::InvalidDigest(_)) => continue,
                Err(e) => return Err(e),
            };
            for child in manifest_child_digests(&bytes) {
                if !reachable.contains(&child) {
                    stack.push(child);
                }
            }
        }
        Ok(reachable)
    }

    /// All blob digests (`<algorithm>:<hex>`) currently present on disk.
    fn present_blob_digests(&self) -> Result<Vec<String>, StoreError> {
        let blobs_root = self.root.join("blobs");
        let mut digests = Vec::new();
        for entry in walkdir::WalkDir::new(&blobs_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            // Layout is blobs/<algorithm>/<hex>; reconstruct `<algorithm>:<hex>`.
            let hex = entry.file_name().to_string_lossy().into_owned();
            let algorithm = entry
                .path()
                .parent()
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned());
            if let Some(algorithm) = algorithm {
                digests.push(format!("{algorithm}:{hex}"));
            }
        }
        Ok(digests)
    }

    /// The tag names currently present in the store.
    fn list_tags(&self) -> Result<Vec<String>, StoreError> {
        let tags_dir = self.root.join("manifests").join("tags");
        let mut tags = Vec::new();
        match fs::read_dir(&tags_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        tags.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(tags)
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
/// An in-progress blob upload, streamed to disk and hashed as it arrives.
///
/// Spans one OCI upload session: `POST` opens it, each `PATCH` appends, and
/// `PUT` finishes it. Nothing is buffered - the bytes go to a temp file under
/// the store and the digest is computed incrementally, so the peak memory of a
/// push is a chunk rather than a layer.
///
/// Dropping without [`BlobUpload::finish`] discards the temp file, so an
/// abandoned upload leaves nothing behind.
pub struct BlobUpload {
    file: NamedTempFile,
    hasher: Sha256,
    written: u64,
    blobs_root: PathBuf,
}

impl BlobUpload {
    /// Append a chunk, refusing to grow the staged blob past
    /// [`MAX_BLOB_BYTES`].
    ///
    /// Streaming removed the accidental ceiling the old buffered path had - a
    /// request over the body limit was rejected with nothing written - and
    /// replaced it with none at all. Without a cap, a holder of the write token
    /// (on the VM push path, the QEMU guest) can PATCH indefinitely across as
    /// many sessions as it likes and fill the filesystem, taking every process
    /// on the host down with it. An honest oversized layer from a bad `COPY`
    /// reaches the same place by accident.
    ///
    /// Enforced here rather than in the handler because this is the one funnel
    /// every write goes through: monolithic POST, chunked PATCH, and the final
    /// PUT chunk all land on `append`.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        let would_be = self.written.saturating_add(bytes.len() as u64);
        if would_be > MAX_BLOB_BYTES {
            return Err(StoreError::BlobTooLarge {
                limit: MAX_BLOB_BYTES,
                attempted: would_be,
            });
        }
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Bytes accepted so far (the OCI `Range` header the client expects).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Verify the streamed content hashes to `expected` and move it into place.
    ///
    /// The digest is checked against what was actually written rather than
    /// trusted from the client, exactly as the buffered path did - the
    /// difference is only where the bytes lived while it was computed. A
    /// mismatch discards the temp file and reports `false`.
    pub fn finish(mut self, expected: &str) -> Result<bool, StoreError> {
        self.file.flush()?;
        let hex: String = self
            .hasher
            .clone()
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if format!("sha256:{hex}") != expected {
            return Ok(false);
        }
        let (algorithm, hex) = parse_digest(expected)?;
        let path = self.blobs_root.join("blobs").join(algorithm).join(hex);
        if path.exists() {
            // Already stored: dedup, and let the temp file drop.
            return Ok(true);
        }
        let dir = path
            .parent()
            .expect("blob path always has a parent under the store root");
        fs::create_dir_all(dir)?;
        self.file.persist(&path).map_err(|e| e.error)?;
        Ok(true)
    }
}

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

/// Extract the child blob digests a manifest or image index references: for a
/// multi-arch index, each sub-manifest; for a single-platform image manifest,
/// its config and layers. A body that is not a recognizable manifest (an
/// ordinary layer blob) yields no children.
fn manifest_child_digests(bytes: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    // Multi-arch index / Docker manifest list.
    if let Some(manifests) = value.get("manifests").and_then(|m| m.as_array()) {
        for m in manifests {
            if let Some(digest) = m.get("digest").and_then(|v| v.as_str()) {
                children.push(digest.to_string());
            }
        }
    }
    // Single-platform image manifest: config + layers.
    if let Some(digest) = value
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|v| v.as_str())
    {
        children.push(digest.to_string());
    }
    if let Some(layers) = value.get("layers").and_then(|l| l.as_array()) {
        for layer in layers {
            if let Some(digest) = layer.get("digest").and_then(|v| v.as_str()) {
                children.push(digest.to_string());
            }
        }
    }
    children
}

/// An RAII guard marking a device pull as in flight.
///
/// While at least one guard is alive, [`BlobStore::prune`] refuses so a blob
/// the pull still needs cannot be swept mid-transfer. The pull is uncounted
/// again when the guard drops.
#[must_use = "the pull is only counted while the guard is held"]
pub struct PullGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for PullGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
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

    // Streaming removed the accidental size ceiling the buffered path had and
    // replaced it with none at all, so a write-token holder - or an accidental
    // oversized layer - could fill the filesystem. Fails if the cap in `append`
    // is removed.
    #[test]
    fn append_refuses_to_grow_a_blob_past_the_ceiling() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "proj");
        let mut upload = store.begin_blob_upload().expect("upload opens");

        // Pretend most of the ceiling is already staged so one ordinary chunk
        // crosses it, asserting the boundary without writing 32 GiB.
        upload.written = MAX_BLOB_BYTES - 8;
        upload
            .append(&[0u8; 8])
            .expect("landing exactly on the ceiling is allowed");
        assert_eq!(upload.written(), MAX_BLOB_BYTES);

        let err = upload
            .append(&[0u8; 1])
            .expect_err("one byte past the ceiling must be refused");
        assert!(
            matches!(err, StoreError::BlobTooLarge { .. }),
            "expected BlobTooLarge, got {err:?}"
        );
    }

    // Nothing in the tree ever looked at `uploads/`, so an `up` killed mid-push
    // left its partial layer on disk permanently while `prune` reported sweeping
    // nothing. Fails if the sweep is removed from `prune`.
    #[test]
    fn prune_reclaims_a_staging_file_left_by_a_killed_upload() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "proj");

        // An upload that never finished and never dropped cleanly - leaking the
        // handle stops `NamedTempFile`'s unlink-on-drop, which is what SIGKILL
        // does.
        let mut upload = store.begin_blob_upload().expect("upload opens");
        upload.append(&[0xabu8; 4096]).unwrap();
        std::mem::forget(upload);

        let uploads = store.root().join("uploads");
        let staged = std::fs::read_dir(&uploads).unwrap().count();
        assert_eq!(staged, 1, "the staging file should be on disk");

        store.prune().expect("prune succeeds");

        let left = std::fs::read_dir(&uploads).unwrap().count();
        assert_eq!(
            left, 0,
            "prune must reclaim abandoned staging files, {left} left"
        );
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

#[cfg(test)]
mod gc {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const MANIFEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const CONFIG: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const LAYER1: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
    const LAYER2: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";
    const ORPHAN: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";
    const INDEX: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";
    const SUBMANIFEST: &str =
        "sha256:7777777777777777777777777777777777777777777777777777777777777777";

    fn store_in(dir: &TempDir, project: &str) -> BlobStore {
        BlobStore::at(dir.path(), project).expect("store opens")
    }

    /// Bytes of a single-platform image manifest referencing `config` + `layers`.
    fn image_manifest(config: &str, layers: &[&str]) -> Vec<u8> {
        let layers: Vec<_> = layers
            .iter()
            .map(|l| json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": l}))
            .collect();
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
            "layers": layers,
        })
        .to_string()
        .into_bytes()
    }

    /// Bytes of a multi-arch image index referencing sub-manifest digests.
    fn image_index(submanifests: &[&str]) -> Vec<u8> {
        let manifests: Vec<_> = submanifests
            .iter()
            .map(
                |m| json!({"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": m}),
            )
            .collect();
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": manifests,
        })
        .to_string()
        .into_bytes()
    }

    /// Populate a tagged single-platform image (manifest + config + one layer)
    /// plus one unreferenced orphan layer.
    fn tagged_image_with_orphan(store: &BlobStore) {
        store.write_blob(CONFIG, b"config-bytes").unwrap();
        store.write_blob(LAYER1, b"layer-1-bytes").unwrap();
        store
            .write_blob(MANIFEST, &image_manifest(CONFIG, &[LAYER1]))
            .unwrap();
        store.set_tag("dev", MANIFEST).unwrap();
        store.write_blob(ORPHAN, b"unreferenced").unwrap();
    }

    #[test]
    fn gc_retains_blobs_referenced_by_a_currently_tagged_manifest() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        tagged_image_with_orphan(&store);

        let swept = store.collect_garbage().unwrap();

        assert_eq!(
            swept,
            vec![ORPHAN.to_string()],
            "only the unreferenced orphan is swept"
        );
        assert!(
            store.has_blob(MANIFEST).unwrap(),
            "the tagged manifest survives GC"
        );
        assert!(
            store.has_blob(CONFIG).unwrap(),
            "the manifest's config blob survives GC"
        );
        assert!(
            store.has_blob(LAYER1).unwrap(),
            "a layer referenced by the tagged manifest survives GC"
        );
        assert!(
            !store.has_blob(ORPHAN).unwrap(),
            "a blob no tagged manifest references is swept"
        );
    }

    #[test]
    fn gc_follows_a_multi_arch_index_to_its_sub_manifests() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        // dev -> index -> sub-manifest -> {config, layer1}. layer2 is an orphan.
        store.write_blob(CONFIG, b"config").unwrap();
        store.write_blob(LAYER1, b"layer-1").unwrap();
        store
            .write_blob(SUBMANIFEST, &image_manifest(CONFIG, &[LAYER1]))
            .unwrap();
        store
            .write_blob(INDEX, &image_index(&[SUBMANIFEST]))
            .unwrap();
        store.set_tag("dev", INDEX).unwrap();
        store.write_blob(LAYER2, b"orphan-layer").unwrap();

        let swept = store.collect_garbage().unwrap();

        assert_eq!(swept, vec![LAYER2.to_string()]);
        for kept in [INDEX, SUBMANIFEST, CONFIG, LAYER1] {
            assert!(
                store.has_blob(kept).unwrap(),
                "{kept} is reachable through the index and must survive"
            );
        }
        assert!(!store.has_blob(LAYER2).unwrap());
    }

    #[test]
    fn a_writing_push_never_sweeps_an_unreferenced_blob() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");

        // An orphan left from an earlier push.
        store.write_blob(ORPHAN, b"unreferenced").unwrap();

        // A fresh push: new blobs + a retag. GC must NOT run implicitly here.
        store.write_blob(CONFIG, b"config").unwrap();
        store.write_blob(LAYER1, b"layer-1").unwrap();
        store
            .write_blob(MANIFEST, &image_manifest(CONFIG, &[LAYER1]))
            .unwrap();
        store.set_tag("dev", MANIFEST).unwrap();

        assert!(
            store.has_blob(ORPHAN).unwrap(),
            "a push/sync must never sweep blobs; only prune/down GC does"
        );

        // The explicit GC path is what removes it.
        let swept = store.collect_garbage().unwrap();
        assert_eq!(swept, vec![ORPHAN.to_string()]);
        assert!(!store.has_blob(ORPHAN).unwrap());
    }

    #[test]
    fn prune_refuses_while_a_device_is_mid_pull() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        tagged_image_with_orphan(&store);

        let guard = store.begin_pull();
        assert_eq!(store.pulls_in_flight(), 1);

        let result = store.prune();
        assert!(
            matches!(result, Err(StoreError::PruneWhilePulling)),
            "prune must refuse while a device is mid-pull, got {result:?}"
        );
        assert!(
            store.has_blob(ORPHAN).unwrap(),
            "a refused prune must not sweep anything"
        );

        // Once the pull drains, prune proceeds and sweeps the orphan.
        drop(guard);
        assert_eq!(store.pulls_in_flight(), 0);
        let swept = store.prune().unwrap();
        assert_eq!(swept, vec![ORPHAN.to_string()]);
        assert!(!store.has_blob(ORPHAN).unwrap());
    }

    #[test]
    fn concurrent_pulls_all_block_prune_until_the_last_drains() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        tagged_image_with_orphan(&store);

        let g1 = store.begin_pull();
        let g2 = store.begin_pull();
        assert_eq!(store.pulls_in_flight(), 2);

        drop(g1);
        assert!(
            matches!(store.prune(), Err(StoreError::PruneWhilePulling)),
            "one pull still in flight keeps prune refused"
        );
        assert!(store.has_blob(ORPHAN).unwrap());

        drop(g2);
        assert!(
            store.prune().is_ok(),
            "prune proceeds after the last pull drains"
        );
        assert!(!store.has_blob(ORPHAN).unwrap());
    }

    #[test]
    fn down_path_gc_ignores_in_flight_pulls() {
        // `down` tears the listeners down, so its GC is unconditional; the
        // mid-pull refusal is a `prune`-only guarantee.
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir, "alpha");
        tagged_image_with_orphan(&store);

        let _guard = store.begin_pull();
        let swept = store.collect_garbage().unwrap();
        assert_eq!(swept, vec![ORPHAN.to_string()]);
    }
}
