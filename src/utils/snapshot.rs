//! Reproducible channel snapshots — auto-pinning the lock file to an immutable
//! point-in-time view of a feed channel.
//!
//! Background: every dnf baseurl is composed as `${repo_url}/$releasever/...`
//! where `$releasever` is `{release}/{channel}` (e.g. `2026/edge`). The serving
//! side publishes an immutable copy of each channel's metadata under
//! `{release}/{channel}/snapshots/<id>/...` (sharing the content-addressed
//! `_pkgs` pool) plus a small mutable pointer
//! `{release}/{channel}/target/<machine>/snapshots-latest.json` naming the
//! newest snapshot.
//!
//! Snapshot pinning therefore reduces to injecting one path segment into
//! `releasever`: `2026/edge` -> `2026/edge/snapshots/<id>`. We resolve the pin
//! once per command and expose it via the `AVOCADO_RELEASEVER` env var, which
//! [`Config::get_releasever`] already honors ahead of the derived
//! `{release}/{channel}` — so every downstream sysroot fetch freezes together
//! with no per-call-site plumbing (mirrors [`Config::promote_repo_tls_env`]).
//!
//! Behavior (confirmed in the feature plan):
//! - **Auto-pin on first fetch**: with no pin recorded, resolve the channel's
//!   current `latest` snapshot, record it in the lock file, and fetch against
//!   it. A later `avocado clean` + rebuild reproduces it exactly.
//! - **Reuse on later fetches**: a recorded pin is reused verbatim.
//! - **Feed without snapshots**: if the pointer 404s, fall back to tracking the
//!   live head (pre-snapshot behavior) and record nothing.
//! - **Manual releasever override**: if the user pins `releasever` explicitly
//!   (config or env), we never auto-pin — they own resolution.
//! - **Changed release/channel**: a stale pin (config moved to a different
//!   feed) is ignored with a warning telling the user to run `avocado update`.

use anyhow::{Context, Result};
use std::env;
use std::path::Path;

use crate::utils::config::Config;
use crate::utils::lockfile::{LockFile, RepoSnapshot};
use crate::utils::output::{print_info, OutputLevel};

/// The mutable pointer published per (channel, target) naming the newest
/// immutable snapshot. Served at
/// `{release}/{channel}/target/<machine>/snapshots-latest.json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SnapshotPointer {
    /// Snapshot id — the immutable `snapshots/<id>` path segment.
    pub id: String,
    /// Snapshot mint time (provenance only).
    #[serde(default)]
    pub created: Option<String>,
}

/// Whether a recorded pin still applies to the configured feed.
#[derive(Debug, PartialEq, Eq)]
pub enum PinStatus {
    /// No pin recorded for this target.
    None,
    /// Pin matches the configured release+channel+repo URL — reuse it.
    Matches,
    /// Pin is for a different release/channel/repo URL than the config now names.
    Mismatch,
}

/// Classify a recorded pin against the configured feed. Pure — unit-tested.
/// A pin recorded without a URL (older lock files) matches any URL.
pub fn pin_status(
    pin: Option<&RepoSnapshot>,
    release: &str,
    channel: &str,
    repo_url: &str,
) -> PinStatus {
    let url_matches = |p: &RepoSnapshot| match &p.repo_url {
        None => true,
        Some(recorded) => recorded.trim_end_matches('/') == repo_url.trim_end_matches('/'),
    };
    match pin {
        None => PinStatus::None,
        Some(p) if p.release == release && p.channel == channel && url_matches(p) => {
            PinStatus::Matches
        }
        Some(_) => PinStatus::Mismatch,
    }
}

/// The releasever path segment for a pinned snapshot. Pure — unit-tested.
pub fn effective_releasever(release: &str, channel: &str, snapshot: &str) -> String {
    format!("{release}/{channel}/snapshots/{snapshot}")
}

/// Machine short name as it appears in feed paths (`target/<machine>/...`).
/// Mirrors `avocado-arch-utils.bbclass`: strip a leading `avocado-`.
fn machine_short(target: &str) -> &str {
    target.strip_prefix("avocado-").unwrap_or(target)
}

/// URL of the per-(channel, target) latest-snapshot pointer.
pub fn pointer_url(repo_url: &str, release: &str, channel: &str, target: &str) -> String {
    let base = repo_url.trim_end_matches('/');
    let machine = machine_short(target);
    format!("{base}/{release}/{channel}/target/{machine}/snapshots-latest.json")
}

/// URL of a snapshot's target repomd — used to pre-flight a recorded pin so a
/// GC'd snapshot produces an actionable error rather than a raw dnf failure.
pub fn repomd_url(
    repo_url: &str,
    release: &str,
    channel: &str,
    target: &str,
    snapshot: &str,
) -> String {
    let base = repo_url.trim_end_matches('/');
    let machine = machine_short(target);
    format!("{base}/{release}/{channel}/snapshots/{snapshot}/target/{machine}/repodata/repomd.xml")
}

/// True when the user has taken explicit control of `releasever` (config or
/// env), in which case we must not auto-pin. This also covers the
/// already-applied case: a parent command that set `AVOCADO_RELEASEVER` to a
/// snapshot path makes children no-op.
fn releasever_is_overridden(config: &Config) -> bool {
    if env::var_os("AVOCADO_RELEASEVER").is_some()
        || env::var_os("AVOCADO_SDK_REPO_RELEASE").is_some()
    {
        return true;
    }
    let distro_override = config
        .distro
        .as_ref()
        .and_then(|d| d.repo_inline())
        .and_then(|r| r.releasever.as_ref())
        .or_else(|| config.distro_feed_def().and_then(|d| d.releasever.as_ref()))
        .is_some();
    let sdk_override = config
        .sdk
        .as_ref()
        .and_then(|s| s.repo_release.as_ref())
        .is_some();
    distro_override || sdk_override
}

/// Build an HTTP client honoring the repo's CA bundle / insecure setting,
/// matching the TLS posture dnf uses for the same endpoint.
pub(crate) fn build_client(config: &Config) -> Result<reqwest::Client> {
    // Snapshot resolution reaches the feed on its own, not through
    // `ResolvedFeedSet`, so the credentials-in-the-URL rule has to be enforced
    // here too or it is enforced only on the path that happens to run first.
    // This one persists: the resolved URL is recorded in the lock as
    // `repo-snapshot.repo_url`, and it reaches error contexts, so a userinfo
    // URL would put a password in a file people commit and in the output.
    if crate::utils::feeds::url_has_userinfo(&config.effective_repo_url()) {
        anyhow::bail!(
            "the distro feed URL carries credentials in the URL — put them in \
             `username`/`password` on a named feed instead. A URL is recorded in \
             the lock's snapshot pin, the canonical document and dnf's logs."
        );
    }
    let mut builder = reqwest::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(crate::utils::feeds::user_agent());
    if config.get_repo_insecure() {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(ca_path) = config.get_repo_ca() {
        let pem = std::fs::read(&ca_path)
            .with_context(|| format!("Failed to read repo CA bundle: {ca_path}"))?;
        // A bundle may carry multiple certs; add each.
        for cert in reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("Failed to parse repo CA bundle: {ca_path}"))?
        {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder.build().context("Failed to build HTTP client")
}

/// Outcome of resolving the channel's latest snapshot.
enum LatestResult {
    /// Pointer present — the named snapshot id (+ provenance).
    Found(SnapshotPointer),
    /// Pointer 404 — the feed does not serve snapshots.
    Unsupported,
}

/// GET the latest-snapshot pointer. 404 -> `Unsupported` (degrade to head);
/// transport/other errors propagate (don't silently lose reproducibility).
async fn fetch_latest(
    client: &reqwest::Client,
    repo_url: &str,
    release: &str,
    channel: &str,
    target: &str,
) -> Result<LatestResult> {
    let url = pointer_url(repo_url, release, channel, target);
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch snapshot pointer: {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(LatestResult::Unsupported);
    }
    let resp = resp
        .error_for_status()
        .with_context(|| format!("Snapshot pointer request failed: {url}"))?;
    let pointer: SnapshotPointer = resp
        .json()
        .await
        .with_context(|| format!("Failed to parse snapshot pointer: {url}"))?;
    Ok(LatestResult::Found(pointer))
}

/// Build a [`RepoSnapshot`] pin from a resolved pointer.
fn build_pin(
    release: &str,
    channel: &str,
    repo_url: &str,
    pointer: &SnapshotPointer,
) -> RepoSnapshot {
    RepoSnapshot {
        release: release.to_string(),
        channel: channel.to_string(),
        snapshot: pointer.id.clone(),
        repo_url: Some(repo_url.to_string()),
        created: pointer.created.clone(),
    }
}

/// Resolve the channel's current latest snapshot into a pin, without touching
/// the lock file or the process env. Used by `avocado update` to advance the
/// pin. Returns `None` when releasever is manually overridden or the feed does
/// not serve snapshots (pointer 404s).
pub async fn resolve_latest(config: &Config, target: &str) -> Result<Option<RepoSnapshot>> {
    if releasever_is_overridden(config) {
        return Ok(None);
    }
    let (Some(release), Some(channel)) = (config.get_distro_release(), config.get_distro_channel())
    else {
        return Ok(None);
    };
    if crate::utils::offline::is_offline() {
        anyhow::bail!(
            "--offline: `avocado update` looks up the channel's latest snapshot over the network; run it without --offline"
        );
    }
    let repo_url = config.effective_repo_url();
    let client = build_client(config)?;
    match fetch_latest(&client, &repo_url, &release, &channel, target).await? {
        LatestResult::Unsupported => Ok(None),
        LatestResult::Found(pointer) => {
            Ok(Some(build_pin(&release, &channel, &repo_url, &pointer)))
        }
    }
}

/// Pre-flight a recorded pin: confirm the snapshot's repomd is still served.
/// A definitive 404 means the snapshot aged out of retention -> actionable
/// error. Transport errors (offline) are tolerated so a cached/offline rebuild
/// against a still-valid pin isn't blocked.
async fn verify_pin_available(
    client: &reqwest::Client,
    repo_url: &str,
    snap: &RepoSnapshot,
    target: &str,
) -> Result<()> {
    let url = repomd_url(
        repo_url,
        &snap.release,
        &snap.channel,
        target,
        &snap.snapshot,
    );
    match client.head(&url).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => anyhow::bail!(
            "Snapshot '{}' for {}/{} is no longer available (retention horizon). \
             Run 'avocado update' to re-pin to the latest snapshot.",
            snap.snapshot,
            snap.release,
            snap.channel
        ),
        // Reachable (2xx/redirect) or non-404 status: proceed.
        Ok(_) => Ok(()),
        // Transport error (offline, DNS, timeout): don't block a pinned rebuild.
        Err(_) => Ok(()),
    }
}

/// Warning for a pin that no longer applies. When release and channel are
/// unchanged the URL is the only difference, so say that rather than claiming
/// the config names a different release/channel.
fn mismatch_warning(snap: &RepoSnapshot, release: &str, channel: &str, repo_url: &str) -> String {
    let tail = "Tracking the live channel head; run 'avocado update' to re-pin.";
    if snap.release == release && snap.channel == channel {
        format!(
            "[WARNING] Lock file is pinned to snapshot '{}' for {}/{} on a different feed URL than \
             the config now names ({}). {tail}",
            snap.snapshot, release, channel, repo_url
        )
    } else {
        format!(
            "[WARNING] Lock file is pinned to snapshot for {}/{} but config now names {}/{}. {tail}",
            snap.release, snap.channel, release, channel
        )
    }
}

/// One line naming the pinned and latest snapshots when they differ. Tolerant:
/// any fetch or parse failure, a feed without snapshots, or an up-to-date pin
/// yields `None`, so an advisory never fails or slows a pinned rebuild.
async fn newer_snapshot_notice(
    client: &reqwest::Client,
    repo_url: &str,
    release: &str,
    channel: &str,
    target: &str,
    pinned: &RepoSnapshot,
) -> Option<String> {
    newer_snapshot_notice_within(
        client,
        repo_url,
        release,
        channel,
        target,
        pinned,
        NEWER_SNAPSHOT_NOTICE_LIMIT,
    )
    .await
}

/// The notice is advisory, so a slow feed must not hold up a pinned rebuild.
const NEWER_SNAPSHOT_NOTICE_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

async fn newer_snapshot_notice_within(
    client: &reqwest::Client,
    repo_url: &str,
    release: &str,
    channel: &str,
    target: &str,
    pinned: &RepoSnapshot,
    limit: std::time::Duration,
) -> Option<String> {
    let latest = tokio::time::timeout(
        limit,
        fetch_latest(client, repo_url, release, channel, target),
    )
    .await
    .ok()?;
    match latest {
        Ok(LatestResult::Found(latest)) if latest.id != pinned.snapshot => Some(format!(
            "Pinned to snapshot '{}'; latest for {release}/{channel} is '{}'. \
             Run 'avocado update' to move the pin.",
            pinned.snapshot, latest.id
        )),
        _ => None,
    }
}

/// Set once the offline pin line has been printed.
static OFFLINE_PIN_PRINTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Print which pin an offline run uses, once per process. The line says no
/// newer-snapshot check was made so a stale pin is never silent.
fn announce_offline_pin(
    printed: &std::sync::atomic::AtomicBool,
    release: &str,
    channel: &str,
    snapshot: &str,
) -> bool {
    if printed.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    print_info(
        &format!(
            "Offline: using locked pin {release}/{channel} snapshot '{snapshot}'; \
             no newer-snapshot check was made."
        ),
        OutputLevel::Normal,
    );
    true
}

/// What avocado itself last exported to `AVOCADO_RELEASEVER` in this process.
/// Exporter and consumer share a process, so a value differing from this one
/// was set by the user.
static EXPORTED_RELEASEVER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Set once the offline "no pin, custom feed" line has been printed.
static OFFLINE_UNPINNED_PRINTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set once the "pin ignored" line has been printed.
static PIN_IGNORED_PRINTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn set_exported_record(value: Option<String>) {
    *EXPORTED_RELEASEVER
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = value;
}

/// The notice for a user-set `AVOCADO_RELEASEVER` that shadows a locked pin.
/// `None` when the variable is unset, equals avocado's own export, or no
/// locked pin applies to this config.
fn pin_ignored_notice(config: &Config, src_dir: &Path, target: &str) -> Option<String> {
    let value = env::var("AVOCADO_RELEASEVER").ok()?;
    let own = EXPORTED_RELEASEVER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if own.as_deref() == Some(value.as_str()) {
        return None;
    }
    let release = config.get_distro_release()?;
    let channel = config.get_distro_channel()?;
    let repo_url = config.effective_repo_url();
    let lock = LockFile::load(src_dir).ok()?;
    let snap = lock.get_repo_snapshot(target);
    if !matches!(
        pin_status(snap, &release, &channel, &repo_url),
        PinStatus::Matches
    ) {
        return None;
    }
    let snap = snap?;
    let pin = effective_releasever(&snap.release, &snap.channel, &snap.snapshot);
    if value == pin {
        return None;
    }
    Some(format!(
        "pin ignored: AVOCADO_RELEASEVER is set to {value}; the locked pin {pin} is not used"
    ))
}

/// Resolve the snapshot pin for `target` and, when one applies, expose it via
/// `AVOCADO_RELEASEVER` so every downstream `get_releasever()` fetches against
/// the frozen snapshot subtree. Auto-pins (and persists) on first fetch.
///
/// Call once at the entry of feed-touching commands; idempotent across the
/// in-process install task graph (children see the parent's env and no-op).
pub async fn resolve_and_apply(config: &Config, src_dir: &Path, target: &str) -> Result<()> {
    if releasever_is_overridden(config) {
        if let Some(msg) = pin_ignored_notice(config, src_dir, target) {
            if !PIN_IGNORED_PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                print_info(&msg, OutputLevel::Normal);
            }
        }
        return Ok(());
    }
    let (Some(release), Some(channel)) = (config.get_distro_release(), config.get_distro_channel())
    else {
        // No release/channel to derive a feed from — nothing to pin.
        return Ok(());
    };
    let repo_url = config.effective_repo_url();

    let mut lock = LockFile::load(src_dir)
        .with_context(|| format!("Failed to load lock file from {}", src_dir.display()))?;
    let client = build_client(config)?;

    let effective = match pin_status(
        lock.get_repo_snapshot(target),
        &release,
        &channel,
        &repo_url,
    ) {
        PinStatus::Matches => {
            let snap = lock.get_repo_snapshot(target).expect("matched");
            if crate::utils::offline::is_offline() {
                announce_offline_pin(
                    &OFFLINE_PIN_PRINTED,
                    &snap.release,
                    &snap.channel,
                    &snap.snapshot,
                );
            } else {
                verify_pin_available(&client, &repo_url, snap, target).await?;
                if let Some(notice) =
                    newer_snapshot_notice(&client, &repo_url, &release, &channel, target, snap)
                        .await
                {
                    print_info(&notice, OutputLevel::Normal);
                }
            }
            effective_releasever(&snap.release, &snap.channel, &snap.snapshot)
        }
        PinStatus::Mismatch => {
            let snap = lock.get_repo_snapshot(target).expect("mismatch");
            print_info(
                &mismatch_warning(snap, &release, &channel, &repo_url),
                OutputLevel::Normal,
            );
            return Ok(());
        }
        PinStatus::None => {
            if crate::utils::offline::is_offline() {
                // Only the default avocado feed is known to publish snapshots;
                // a custom feed that serves none can never have a pin, so
                // erroring would make offline unusable for it.
                if repo_url.trim_end_matches('/') != Config::DEFAULT_REPO_URL {
                    if !OFFLINE_UNPINNED_PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        print_info(
                            &format!(
                                "Offline: no locked pin for {release}/{channel} on {repo_url}; \
                                 using the configured release unpinned"
                            ),
                            OutputLevel::Normal,
                        );
                    }
                    return Ok(());
                }
                anyhow::bail!(
                    "--offline needs a locked snapshot pin for {release}/{channel} on '{target}', \
                     and none is recorded. Run `avocado update` online to create one."
                );
            }
            match fetch_latest(&client, &repo_url, &release, &channel, target).await? {
                LatestResult::Unsupported => return Ok(()),
                LatestResult::Found(pointer) => {
                    let snap = build_pin(&release, &channel, &repo_url, &pointer);
                    let eff = effective_releasever(&snap.release, &snap.channel, &snap.snapshot);
                    lock.set_repo_snapshot(target, snap);
                    lock.save(src_dir)
                        .with_context(|| "Failed to record snapshot pin in lock file")?;
                    print_info(
                        &format!("Pinned {release}/{channel} to snapshot '{}'.", pointer.id),
                        OutputLevel::Normal,
                    );
                    eff
                }
            }
        }
    };

    set_exported_record(Some(effective.clone()));
    env::set_var("AVOCADO_RELEASEVER", effective);
    Ok(())
}

/// The releasever [`resolve_and_apply`] would export for `target`, read from
/// the recorded pin — no network, no lock write. For callers that must
/// reproduce an install's inputs without performing one.
///
/// `None` in exactly the branches where [`resolve_and_apply`] leaves
/// `AVOCADO_RELEASEVER` alone, so the caller falls back to the plain
/// configured value as the install did.
pub fn pinned_releasever(config: &Config, lock: &LockFile, target: &str) -> Option<String> {
    if releasever_is_overridden(config) {
        return None;
    }
    let (release, channel) = (config.get_distro_release()?, config.get_distro_channel()?);
    let snap = lock.get_repo_snapshot(target)?;
    match pin_status(Some(snap), &release, &channel, &config.effective_repo_url()) {
        PinStatus::Matches => Some(effective_releasever(
            &snap.release,
            &snap.channel,
            &snap.snapshot,
        )),
        PinStatus::None | PinStatus::Mismatch => None,
    }
}

/// Convenience entry for commands that hold a `config_path` string: resolves
/// `src_dir` the same way the install/clean commands do, then delegates to
/// [`resolve_and_apply`]. One line per command call site.
pub async fn resolve_and_apply_for(config: &Config, config_path: &str, target: &str) -> Result<()> {
    let src_dir = config.get_resolved_src_dir(config_path).unwrap_or_else(|| {
        Path::new(config_path)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    });
    resolve_and_apply(config, &src_dir, target).await
}

#[cfg(test)]
mod tests {

    /// Snapshot resolution reaches the feed without going through
    /// `ResolvedFeedSet`, so it needs its own copy of the no-credentials-in-the-URL
    /// rule. This one is the worse leak of the two: the resolved URL is written
    /// into the lock as `repo-snapshot.repo_url`, a file people commit.
    #[test]
    fn the_snapshot_client_refuses_credentials_in_the_repo_url() {
        let load =
            |yaml: &str| -> super::Config { serde_yaml::from_str(yaml).expect("yaml parses") };
        let with = load(
            "distro:\n  release: 2026\n  channel: next\n  repo:\n    url: https://u:p@repo.example\n",
        );
        let err = super::build_client(&with).unwrap_err().to_string();
        assert!(err.contains("credentials in the URL"), "{err}");

        let without = load(
            "distro:\n  release: 2026\n  channel: next\n  repo:\n    url: https://repo.example\n",
        );
        assert!(super::build_client(&without).is_ok());
    }
    use super::*;

    fn snap(release: &str, channel: &str, id: &str, url: Option<&str>) -> RepoSnapshot {
        RepoSnapshot {
            release: release.to_string(),
            channel: channel.to_string(),
            snapshot: id.to_string(),
            repo_url: url.map(str::to_string),
            created: None,
        }
    }

    const URL: &str = "https://repo.example.com";

    #[test]
    fn effective_releasever_injects_snapshot_segment() {
        assert_eq!(
            effective_releasever("2026", "edge", "20260531T120000Z-qemux86-64"),
            "2026/edge/snapshots/20260531T120000Z-qemux86-64"
        );
    }

    #[test]
    fn pin_status_none_when_unpinned() {
        assert_eq!(pin_status(None, "2026", "edge", URL), PinStatus::None);
    }

    /// The pin that `Config::load_composed` reads from the lock is not an
    /// explicit releasever. If it were, install would skip the check that the
    /// snapshot is still served, and `avocado update` could not advance it.
    #[test]
    #[serial_test::serial]
    fn a_pin_read_at_load_is_not_an_override() {
        env::remove_var("AVOCADO_RELEASEVER");
        env::remove_var("AVOCADO_SDK_REPO_RELEASE");
        let mut config: Config =
            serde_yaml::from_str("distro:\n  release: 2026\n  channel: next\n").unwrap();
        config.locked_releasever = Some(effective_releasever("2026", "next", "320"));
        assert!(!releasever_is_overridden(&config));
    }

    #[test]
    fn pin_status_matches_same_feed() {
        let p = snap("2026", "edge", "X", Some(URL));
        assert_eq!(
            pin_status(Some(&p), "2026", "edge", URL),
            PinStatus::Matches
        );
        let slashed = format!("{URL}/");
        assert_eq!(
            pin_status(Some(&p), "2026", "edge", &slashed),
            PinStatus::Matches
        );
        let unrecorded = snap("2026", "edge", "X", None);
        assert_eq!(
            pin_status(Some(&unrecorded), "2026", "edge", URL),
            PinStatus::Matches
        );
    }

    #[test]
    fn pin_with_other_url_is_not_matching() {
        let p = snap("2026", "edge", "X", Some(URL));
        assert_eq!(
            pin_status(Some(&p), "2026", "edge", "https://mirror.example.com"),
            PinStatus::Mismatch
        );
    }

    #[test]
    fn pin_status_mismatch_on_channel_change() {
        let p = snap("2026", "edge", "X", Some(URL));
        assert_eq!(
            pin_status(Some(&p), "2026", "stable", URL),
            PinStatus::Mismatch
        );
        assert_eq!(
            pin_status(Some(&p), "2027", "edge", URL),
            PinStatus::Mismatch
        );
    }

    #[test]
    fn pointer_url_strips_avocado_prefix_and_trailing_slash() {
        assert_eq!(
            pointer_url(
                "https://repo.example.com/",
                "2026",
                "edge",
                "avocado-qemux86-64"
            ),
            "https://repo.example.com/2026/edge/target/qemux86-64/snapshots-latest.json"
        );
    }

    #[test]
    fn repomd_url_points_into_snapshot_subtree() {
        assert_eq!(
            repomd_url("https://r.io", "2026", "edge", "qemux86-64", "SNAP"),
            "https://r.io/2026/edge/snapshots/SNAP/target/qemux86-64/repodata/repomd.xml"
        );
    }

    #[test]
    fn url_only_change_does_not_claim_release_or_channel_changed() {
        let p = snap("2026", "edge", "X", Some(URL));
        let msg = mismatch_warning(&p, "2026", "edge", "https://mirror.example.com");
        assert!(msg.contains("different feed URL"), "{msg}");
        assert!(msg.contains("https://mirror.example.com"), "{msg}");
        assert!(!msg.contains("config now names 2026/edge"), "{msg}");
    }

    #[test]
    fn channel_change_still_names_both_feeds() {
        let p = snap("2026", "edge", "X", Some(URL));
        let msg = mismatch_warning(&p, "2026", "stable", URL);
        assert!(
            msg.contains("pinned to snapshot for 2026/edge but config now names 2026/stable"),
            "{msg}"
        );
    }

    /// Serve one canned HTTP response on a loopback port, then return its base URL.
    fn serve_once(status: &str, body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn newer_latest_names_pinned_latest_and_update_command() {
        let url = serve_once("200 OK", r#"{"id":"NEW"}"#);
        let p = snap("2026", "edge", "OLD", Some(&url));
        let msg = newer_snapshot_notice(
            &reqwest::Client::new(),
            &url,
            "2026",
            "edge",
            "qemux86-64",
            &p,
        )
        .await
        .expect("a differing latest yields a notice");
        assert!(msg.contains("'OLD'") && msg.contains("'NEW'"), "{msg}");
        assert!(msg.contains("avocado update"), "{msg}");
    }

    #[tokio::test]
    async fn latest_equal_to_pin_prints_nothing() {
        let url = serve_once("200 OK", r#"{"id":"SAME"}"#);
        let p = snap("2026", "edge", "SAME", Some(&url));
        let msg = newer_snapshot_notice(
            &reqwest::Client::new(),
            &url,
            "2026",
            "edge",
            "qemux86-64",
            &p,
        )
        .await;
        assert_eq!(msg, None);
    }

    #[tokio::test]
    async fn unreachable_latest_prints_nothing() {
        // Bind then drop, so the port is closed when the client connects.
        let url = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let p = snap("2026", "edge", "OLD", Some(&url));
        let msg = newer_snapshot_notice(
            &reqwest::Client::new(),
            &url,
            "2026",
            "edge",
            "qemux86-64",
            &p,
        )
        .await;
        assert_eq!(msg, None);
    }

    #[tokio::test]
    async fn hung_latest_endpoint_is_abandoned_within_the_limit() {
        // Accepts the connection and never answers.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let p = snap("2026", "edge", "OLD", Some(&url));
        let started = std::time::Instant::now();
        let msg = newer_snapshot_notice_within(
            &reqwest::Client::new(),
            &url,
            "2026",
            "edge",
            "qemux86-64",
            &p,
            std::time::Duration::from_millis(300),
        )
        .await;
        assert_eq!(msg, None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "advisory held the command for {:?}",
            started.elapsed()
        );
        drop(l);
    }

    /// A listener that records connections without answering. A connection made
    /// before this is polled sits in the accept backlog, so a nonblocking
    /// `accept` afterwards proves whether anything dialed it.
    fn silent_listener() -> (std::net::TcpListener, String) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        (l, url)
    }

    fn offline_config(url: &str) -> Config {
        serde_yaml::from_str(&format!(
            "distro:\n  release: 2026\n  channel: edge\n  repo:\n    url: {url}\n"
        ))
        .unwrap()
    }

    /// Runs `resolve_and_apply` offline and always restores the global flag and
    /// the env var it exports, so a failing assertion cannot leak either.
    async fn run_offline(config: &Config, dir: &Path) -> (Result<()>, Option<String>) {
        env::remove_var("AVOCADO_RELEASEVER");
        env::remove_var("AVOCADO_SDK_REPO_RELEASE");
        crate::utils::offline::set_offline(true);
        let res = resolve_and_apply(config, dir, "qemux86-64").await;
        crate::utils::offline::set_offline(false);
        let release = env::var("AVOCADO_RELEASEVER").ok();
        env::remove_var("AVOCADO_RELEASEVER");
        (res, release)
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn resolve_and_apply_offline_uses_lock_pin() {
        let (listener, url) = silent_listener();
        let dir = tempfile::tempdir().unwrap();
        let mut lock = LockFile::load(dir.path()).unwrap();
        lock.set_repo_snapshot("qemux86-64", snap("2026", "edge", "PINNED", Some(&url)));
        lock.save(dir.path()).unwrap();

        let (res, release) = run_offline(&offline_config(&url), dir.path()).await;

        res.expect("an offline run with a pin succeeds");
        assert_eq!(
            release.as_deref(),
            Some("2026/edge/snapshots/PINNED"),
            "the locked pin is the release in use"
        );
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "offline made a request"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn update_lookup_refuses_offline_before_any_request() {
        let (listener, url) = silent_listener();
        crate::utils::offline::set_offline(true);
        let res = resolve_latest(&offline_config(&url), "qemux86-64").await;
        crate::utils::offline::set_offline(false);
        let err = res.expect_err("update needs the network").to_string();
        assert!(err.contains("--offline"), "{err}");
        assert!(err.contains("avocado update"), "{err}");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "offline made a request"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn update_lookup_offline_with_overridden_releasever_needs_no_network() {
        // An override means there is nothing to look up, so offline is irrelevant.
        let config: Config = serde_yaml::from_str(
            "distro:\n  release: 2026\n  channel: edge\n  repo:\n    releasever: 2026/next\n",
        )
        .unwrap();
        crate::utils::offline::set_offline(true);
        let res = resolve_latest(&config, "qemux86-64").await;
        crate::utils::offline::set_offline(false);
        assert!(res.expect("override needs no lookup").is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn offline_without_pin_names_the_pin() {
        // The default avocado repo always publishes snapshots, so a missing pin
        // is a real gap that `avocado update` can fill.
        let config: Config =
            serde_yaml::from_str("distro:\n  release: 2026\n  channel: edge\n").unwrap();
        let dir = tempfile::tempdir().unwrap();

        let (res, release) = run_offline(&config, dir.path()).await;

        let err = res.expect_err("no pin offline must fail").to_string();
        assert!(err.contains("pin"), "{err}");
        assert!(err.contains("avocado update"), "{err}");
        assert_eq!(release, None);
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn offline_without_pin_on_custom_feed_proceeds() {
        // A custom feed may publish no snapshots, so no pin can ever exist.
        let (listener, url) = silent_listener();
        let dir = tempfile::tempdir().unwrap();

        let (res, release) = run_offline(&offline_config(&url), dir.path()).await;

        res.expect("a custom feed without a pin proceeds offline");
        assert_eq!(release, None, "no pin is exported");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "offline made a request"
        );
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    fn release_equal_to_pin_prints_no_notice() {
        reset_notice_state();
        let url = "http://127.0.0.1:1";
        let dir = pinned_dir(url);
        env::set_var("AVOCADO_RELEASEVER", "2026/edge/snapshots/PINNED");
        let msg = pin_ignored_notice(&offline_config(url), dir.path(), "qemux86-64");
        env::remove_var("AVOCADO_RELEASEVER");
        assert_eq!(msg, None);
    }

    /// Lock with a pin for `qemux86-64` in a fresh dir.
    fn pinned_dir(url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut lock = LockFile::load(dir.path()).unwrap();
        lock.set_repo_snapshot("qemux86-64", snap("2026", "edge", "PINNED", Some(url)));
        lock.save(dir.path()).unwrap();
        dir
    }

    fn reset_notice_state() {
        env::remove_var("AVOCADO_RELEASEVER");
        env::remove_var("AVOCADO_SDK_REPO_RELEASE");
        set_exported_record(None);
        PIN_IGNORED_PRINTED.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    fn user_releasever_prints_pin_ignored_notice() {
        reset_notice_state();
        let url = "http://127.0.0.1:1";
        let dir = pinned_dir(url);
        env::set_var("AVOCADO_RELEASEVER", "custom/rel");
        let msg = pin_ignored_notice(&offline_config(url), dir.path(), "qemux86-64");
        env::remove_var("AVOCADO_RELEASEVER");
        assert_eq!(
            msg.as_deref(),
            Some(
                "pin ignored: AVOCADO_RELEASEVER is set to custom/rel; the locked pin \
                 2026/edge/snapshots/PINNED is not used"
            )
        );
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    fn own_export_prints_no_notice() {
        reset_notice_state();
        let url = "http://127.0.0.1:1";
        let dir = pinned_dir(url);
        set_exported_record(Some("2026/edge/snapshots/PINNED".into()));
        env::set_var("AVOCADO_RELEASEVER", "2026/edge/snapshots/PINNED");
        let msg = pin_ignored_notice(&offline_config(url), dir.path(), "qemux86-64");
        env::remove_var("AVOCADO_RELEASEVER");
        set_exported_record(None);
        assert_eq!(msg, None);
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    fn no_pin_no_notice() {
        reset_notice_state();
        let url = "http://127.0.0.1:1";
        let dir = tempfile::tempdir().unwrap();
        env::set_var("AVOCADO_RELEASEVER", "custom/rel");
        let msg = pin_ignored_notice(&offline_config(url), dir.path(), "qemux86-64");
        env::remove_var("AVOCADO_RELEASEVER");
        assert_eq!(msg, None);
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn override_release_unchanged_by_notice() {
        let url = "http://127.0.0.1:1";
        let dir = pinned_dir(url);
        let config = offline_config(url);
        let mut seen = Vec::new();
        for _ in 0..2 {
            // First iteration fires the notice, second finds it already printed.
            reset_notice_state();
            env::set_var("AVOCADO_RELEASEVER", "custom/rel");
            resolve_and_apply(&config, dir.path(), "qemux86-64")
                .await
                .expect("override never refuses");
            seen.push(env::var("AVOCADO_RELEASEVER").unwrap());
        }
        // Same call again with the once-guard already set: no notice path.
        env::set_var("AVOCADO_RELEASEVER", "custom/rel");
        resolve_and_apply(&config, dir.path(), "qemux86-64")
            .await
            .unwrap();
        seen.push(env::var("AVOCADO_RELEASEVER").unwrap());
        env::remove_var("AVOCADO_RELEASEVER");
        assert!(PIN_IGNORED_PRINTED.load(std::sync::atomic::Ordering::SeqCst));
        assert!(seen.iter().all(|r| r == "custom/rel"), "{seen:?}");
    }

    /// The pin avocado exports itself must be recorded, or the next command in
    /// the same process would read it back as a user override.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(offline)]
    async fn own_pin_export_is_not_reported_as_override() {
        reset_notice_state();
        let (_listener, url) = silent_listener();
        let dir = pinned_dir(&url);
        let config = offline_config(&url);
        crate::utils::offline::set_offline(true);
        let res = resolve_and_apply(&config, dir.path(), "qemux86-64").await;
        crate::utils::offline::set_offline(false);
        res.unwrap();
        let exported = env::var("AVOCADO_RELEASEVER").ok();
        // Shift the pin so only the export record, not value == pin, can
        // silence the notice.
        let mut lock = LockFile::load(dir.path()).unwrap();
        lock.set_repo_snapshot("qemux86-64", snap("2026", "edge", "NEWER", Some(&url)));
        lock.save(dir.path()).unwrap();
        let msg = pin_ignored_notice(&config, dir.path(), "qemux86-64");
        reset_notice_state();
        assert_eq!(exported.as_deref(), Some("2026/edge/snapshots/PINNED"));
        assert_eq!(msg, None);
    }

    #[test]
    fn offline_pin_line_prints_once() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(announce_offline_pin(&flag, "2026", "edge", "PINNED"));
        assert!(!announce_offline_pin(&flag, "2026", "edge", "PINNED"));
    }
}
