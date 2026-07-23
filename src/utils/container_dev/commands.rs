//! Testable core of `container dev sync` and `container dev prune` (task 5.3,
//! design M4).
//!
//! Both subcommands are thin passes over primitives that already exist:
//!
//! - **`sync`** is a ONE-SHOT re-push + notify of the current watched tag, NOT a
//!   long-running watcher. [`run_one_shot_sync`] drives the exact same
//!   [`Syncer`]/[`Notifier`] seams the watcher (task 4.2) uses per rebuild — the
//!   topology-selected PUSH/INGEST transfer followed by a control-WS notify —
//!   but exactly once, then returns. It never enters the `run_watcher` receive
//!   loop, so a `sync` invocation performs one transfer and one notification and
//!   is done.
//! - **`prune`** garbage-collects the per-project store ONLY, via the group-3.5
//!   GC ([`BlobStore::prune`]). [`prune_store`] reuses that policy verbatim: it
//!   sweeps blobs no currently-tagged manifest references and refuses while a
//!   device is mid-pull. It touches nothing but blobs under the store's
//!   `registry/` tree — never the per-session token or the CA material (which
//!   live in memory for the session and, where persisted, sit OUTSIDE the
//!   `registry/` tree the GC walks).

use anyhow::{Context, Result};

use super::engine::TagEvent;
use super::store::{BlobStore, StoreError};
use super::watcher::{Notifier, SyncMode, Syncer};

/// Perform ONE re-push + notify of a watched tag and return — the `container dev
/// sync` core (design M4).
///
/// This reuses the group-4 sync pipeline (`Syncer` then `Notifier`), the same
/// two seams the watcher drives on every rebuild, run exactly once: transfer the
/// image's changed layers (PUSH into the embedded registry, or the INGEST
/// fallback, per `mode`), then notify the device over the control WS. Unlike
/// [`super::watcher::run_watcher`] there is no receive loop — a single pass, then
/// this returns, so a manual `sync` is one transfer + one notification, never a
/// persistent watch.
///
/// A failed re-push short-circuits before the notify (propagated as `Err`), so a
/// device is never told an image is ready when the push did not land — mirroring
/// the watcher's push-then-notify ordering, but surfacing the failure to the CLI
/// caller rather than swallowing it as a warning.
pub async fn run_one_shot_sync(
    mode: SyncMode,
    syncer: &dyn Syncer,
    notifier: &dyn Notifier,
    event: &TagEvent,
) -> Result<()> {
    syncer
        .sync(mode, event)
        .await
        .with_context(|| format!("re-pushing `{}`", event.image))?;
    notifier
        .notify(event)
        .await
        .with_context(|| format!("notifying the device that `{}` is ready", event.image))?;
    Ok(())
}

/// Garbage-collect the per-project store — the `container dev prune` core (design
/// M4, task 3.5).
///
/// This delegates to [`BlobStore::prune`], reusing the single GC policy verbatim:
/// it retains every blob a currently-tagged manifest references, sweeps the rest,
/// and refuses (rather than sweeping a blob a pull still needs) while a device is
/// mid-pull. It operates ONLY on blobs under the store's `registry/` tree, so it
/// never removes the per-session read/control or write token, nor the per-project
/// CA material — those are session state, not store blobs, and prune has no path
/// to them.
pub fn prune_store(store: &BlobStore) -> Result<Vec<String>, StoreError> {
    store.prune()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    // ---- sync: a recording double for the Syncer + Notifier seams ----

    /// Records every `sync`/`notify` call so a test can assert the one-shot
    /// pipeline runs each exactly once, in order, and stops.
    #[derive(Default)]
    struct Recorder {
        /// Ordered log of `sync:<image>:<mode>` / `notify:<image>`.
        log: Mutex<Vec<String>>,
        sync_calls: AtomicUsize,
        notify_calls: AtomicUsize,
        /// When true, the push fails so the notify must be skipped.
        fail_sync: bool,
    }

    impl Syncer for Recorder {
        fn sync<'a>(
            &'a self,
            mode: SyncMode,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.sync_calls.fetch_add(1, Ordering::SeqCst);
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("sync:{}:{mode:?}", event.image));
                if self.fail_sync {
                    anyhow::bail!("push to the embedded registry failed");
                }
                Ok(())
            })
        }
    }

    impl Notifier for Recorder {
        fn notify<'a>(
            &'a self,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.notify_calls.fetch_add(1, Ordering::SeqCst);
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("notify:{}", event.image));
                Ok(())
            })
        }
    }

    fn ev(image: &str) -> TagEvent {
        TagEvent {
            image: image.to_string(),
            image_id: None,
        }
    }

    #[tokio::test]
    async fn sync_re_pushes_then_notifies_exactly_once() {
        let rec = Recorder::default();
        run_one_shot_sync(SyncMode::Push, &rec, &rec, &ev("my-app:dev"))
            .await
            .expect("a one-shot sync succeeds");

        assert_eq!(
            rec.sync_calls.load(Ordering::SeqCst),
            1,
            "sync must re-push exactly once"
        );
        assert_eq!(
            rec.notify_calls.load(Ordering::SeqCst),
            1,
            "sync must notify exactly once"
        );
        assert_eq!(
            *rec.log.lock().unwrap(),
            vec![
                "sync:my-app:dev:Push".to_string(),
                "notify:my-app:dev".to_string(),
            ],
            "sync must re-push (delta) THEN notify, in that order"
        );
    }

    #[tokio::test]
    async fn sync_is_one_shot_not_a_persistent_watch_loop() {
        // A watcher loop would block awaiting further tag events; a one-shot sync
        // returns after a single pass. A generous timeout that still resolves
        // proves it is not a persistent watch, and the counts prove it did not
        // repeat.
        let rec = Recorder::default();
        tokio::time::timeout(
            Duration::from_secs(2),
            run_one_shot_sync(SyncMode::Push, &rec, &rec, &ev("my-app:dev")),
        )
        .await
        .expect("a one-shot sync returns promptly; it is not a persistent watch loop")
        .expect("the sync succeeds");

        assert_eq!(
            rec.sync_calls.load(Ordering::SeqCst),
            1,
            "a one-shot sync re-pushes once, not repeatedly like a watcher"
        );
        assert_eq!(rec.notify_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_re_push_propagates_and_skips_the_notify() {
        let rec = Recorder {
            fail_sync: true,
            ..Default::default()
        };
        let err = run_one_shot_sync(SyncMode::Push, &rec, &rec, &ev("my-app:dev"))
            .await
            .expect_err("a failed re-push must surface as an error");

        assert!(
            err.to_string().contains("re-pushing"),
            "the error must name the failed re-push: {err:#}"
        );
        assert_eq!(
            rec.sync_calls.load(Ordering::SeqCst),
            1,
            "the push was attempted once"
        );
        assert_eq!(
            rec.notify_calls.load(Ordering::SeqCst),
            0,
            "a failed re-push must NOT notify the device that an image is ready"
        );
    }

    // ---- prune: GC the per-project store ONLY, never the token/CA ----

    const MANIFEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const CONFIG: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const LAYER: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
    const ORPHAN: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";

    /// Bytes of a single-platform image manifest referencing `config` + `layer`.
    fn image_manifest(config: &str, layer: &str) -> Vec<u8> {
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
            "layers": [
                {"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer}
            ],
        })
        .to_string()
        .into_bytes()
    }

    /// A store with a tagged image (manifest + config + layer) plus one orphan.
    fn store_with_tagged_image_and_orphan(dir: &TempDir) -> BlobStore {
        let store = BlobStore::at(dir.path(), "alpha").expect("store opens");
        store.write_blob(CONFIG, b"config-bytes").unwrap();
        store.write_blob(LAYER, b"layer-bytes").unwrap();
        store
            .write_blob(MANIFEST, &image_manifest(CONFIG, LAYER))
            .unwrap();
        store.set_tag("dev", MANIFEST).unwrap();
        store.write_blob(ORPHAN, b"unreferenced").unwrap();
        store
    }

    #[test]
    fn prune_sweeps_orphan_store_blobs_but_retains_tagged_ones() {
        let dir = TempDir::new().unwrap();
        let store = store_with_tagged_image_and_orphan(&dir);

        let swept = prune_store(&store).expect("prune succeeds with no pull in flight");

        assert_eq!(
            swept,
            vec![ORPHAN.to_string()],
            "prune must sweep exactly the unreferenced orphan blob"
        );
        assert!(!store.has_blob(ORPHAN).unwrap(), "the orphan is gone");
        for kept in [MANIFEST, CONFIG, LAYER] {
            assert!(
                store.has_blob(kept).unwrap(),
                "a blob referenced by the tagged manifest must survive prune: {kept}"
            );
        }
    }

    #[test]
    fn prune_never_touches_the_token_or_ca_material() {
        let dir = TempDir::new().unwrap();
        let store = store_with_tagged_image_and_orphan(&dir);

        // The per-project dir is the store root's parent
        // (`<avocado>/container-dev/<project>/`); the session's token and CA
        // material are siblings of the `registry/` tree prune walks. Stand in
        // for them with files prune must leave untouched.
        let project_dir = store
            .root()
            .parent()
            .expect("the store root sits under the per-project dir")
            .to_path_buf();
        let ca = project_dir.join("ca.pem");
        let read_token = project_dir.join("read-token");
        let write_token = project_dir.join("write-token");
        let ca_pem = "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----";
        std::fs::write(&ca, ca_pem).unwrap();
        std::fs::write(&read_token, "read-secret").unwrap();
        std::fs::write(&write_token, "write-secret").unwrap();

        let swept = prune_store(&store).expect("prune succeeds");
        assert_eq!(swept, vec![ORPHAN.to_string()], "prune only sweeps blobs");

        // The token and CA material must be byte-for-byte intact after prune.
        assert!(ca.exists(), "prune must NOT delete the CA material");
        assert!(read_token.exists(), "prune must NOT delete the read token");
        assert!(
            write_token.exists(),
            "prune must NOT delete the write token"
        );
        assert_eq!(
            std::fs::read_to_string(&ca).unwrap(),
            ca_pem,
            "the CA material must be unchanged"
        );
        assert_eq!(std::fs::read_to_string(&read_token).unwrap(), "read-secret");
        assert_eq!(
            std::fs::read_to_string(&write_token).unwrap(),
            "write-secret"
        );
    }

    #[test]
    fn prune_refuses_while_a_device_is_mid_pull() {
        let dir = TempDir::new().unwrap();
        let store = store_with_tagged_image_and_orphan(&dir);

        let guard = store.begin_pull();
        let result = prune_store(&store);
        assert!(
            matches!(result, Err(StoreError::PruneWhilePulling)),
            "prune must refuse while a device is mid-pull, got {result:?}"
        );
        assert!(
            store.has_blob(ORPHAN).unwrap(),
            "a refused prune must not sweep anything"
        );

        drop(guard);
        let swept = prune_store(&store).expect("prune proceeds once the pull drains");
        assert_eq!(swept, vec![ORPHAN.to_string()]);
    }
}
