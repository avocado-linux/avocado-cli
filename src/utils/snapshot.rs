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
    /// Pin matches the configured release+channel — reuse it.
    Matches,
    /// Pin is for a different release/channel than the config now names.
    Mismatch,
}

/// Classify a recorded pin against the configured feed. Pure — unit-tested.
pub fn pin_status(pin: Option<&RepoSnapshot>, release: &str, channel: &str) -> PinStatus {
    match pin {
        None => PinStatus::None,
        Some(p) if p.release == release && p.channel == channel => PinStatus::Matches,
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
        .and_then(|d| d.repo.as_ref())
        .and_then(|r| r.releasever.as_ref())
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
fn build_client(config: &Config) -> Result<reqwest::Client> {
    let mut builder = reqwest::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("avocado-cli/", env!("CARGO_PKG_VERSION")));
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

/// Resolve the snapshot pin for `target` and, when one applies, expose it via
/// `AVOCADO_RELEASEVER` so every downstream `get_releasever()` fetches against
/// the frozen snapshot subtree. Auto-pins (and persists) on first fetch.
///
/// Call once at the entry of feed-touching commands; idempotent across the
/// in-process install task graph (children see the parent's env and no-op).
pub async fn resolve_and_apply(config: &Config, src_dir: &Path, target: &str) -> Result<()> {
    if releasever_is_overridden(config) {
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

    let effective = match pin_status(lock.get_repo_snapshot(target), &release, &channel) {
        PinStatus::Matches => {
            let snap = lock.get_repo_snapshot(target).expect("matched");
            verify_pin_available(&client, &repo_url, snap, target).await?;
            effective_releasever(&snap.release, &snap.channel, &snap.snapshot)
        }
        PinStatus::Mismatch => {
            let snap = lock.get_repo_snapshot(target).expect("mismatch");
            print_info(
                &format!(
                    "[WARNING] Lock file is pinned to snapshot for {}/{} but config now names {}/{}. \
                     Tracking the live channel head; run 'avocado update' to re-pin.",
                    snap.release, snap.channel, release, channel
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }
        PinStatus::None => {
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

    env::set_var("AVOCADO_RELEASEVER", effective);
    Ok(())
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
    use super::*;

    fn snap(release: &str, channel: &str, id: &str) -> RepoSnapshot {
        RepoSnapshot {
            release: release.to_string(),
            channel: channel.to_string(),
            snapshot: id.to_string(),
            repo_url: None,
            created: None,
        }
    }

    #[test]
    fn effective_releasever_injects_snapshot_segment() {
        assert_eq!(
            effective_releasever("2026", "edge", "20260531T120000Z-qemux86-64"),
            "2026/edge/snapshots/20260531T120000Z-qemux86-64"
        );
    }

    #[test]
    fn pin_status_none_when_unpinned() {
        assert_eq!(pin_status(None, "2026", "edge"), PinStatus::None);
    }

    #[test]
    fn pin_status_matches_same_feed() {
        let p = snap("2026", "edge", "X");
        assert_eq!(pin_status(Some(&p), "2026", "edge"), PinStatus::Matches);
    }

    #[test]
    fn pin_status_mismatch_on_channel_change() {
        let p = snap("2026", "edge", "X");
        assert_eq!(pin_status(Some(&p), "2026", "stable"), PinStatus::Mismatch);
        assert_eq!(pin_status(Some(&p), "2027", "edge"), PinStatus::Mismatch);
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
}
