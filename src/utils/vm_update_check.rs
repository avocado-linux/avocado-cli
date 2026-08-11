//! 24-hour cached polling of the avocado-vm release channel.
//!
//! Parallel to [`crate::utils::update_check`], which polls the
//! avocado-cli's own GitHub Releases. This module hits the channel
//! pointer at `https://repo.avocadolinux.org/releases/vm/<channel>.json`
//! and tells the caller "is there a newer VM available?" — without
//! downloading the per-arch manifest (that happens in the update flow
//! only when the user opts in).

use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use directories::ProjectDirs;
use reqwest::ClientBuilder;
use serde::{Deserialize, Serialize};

use crate::utils::vm::channel::ChannelPointer;

/// Same 24h window as the CLI self-update poll.
const CHECK_INTERVAL_SECS: u64 = 60 * 60 * 24;
const FETCH_TIMEOUT_SECS: u64 = 5;

/// Default channel-pointer host. Overridable for testing via the
/// `AVOCADO_VM_CHANNEL_URL_BASE` environment variable.
pub const DEFAULT_BASE: &str = "https://repo.avocadolinux.org/releases/vm";

#[derive(Serialize, Deserialize)]
struct UpdateCache {
    last_checked_secs: u64,
    channel: String,
    /// Cached channel pointer JSON (verbatim — re-parsed on read). We
    /// cache the full document, not just the version, so a stale
    /// cached pointer still has the URLs needed if the user runs
    /// `avocado vm update --check` after the network drops.
    pointer_json: String,
}

/// Result of a single check.
pub struct UpdateAvailable {
    pub pointer: ChannelPointer,
    /// The local version we compared against; `None` when there's no
    /// installed manifest yet (first-run case). Carried for callers
    /// that want to format a "X → Y" diff message.
    #[allow(dead_code)]
    pub installed_version: Option<String>,
}

/// Outcome of a channel check.
pub enum VmUpdateStatus {
    /// A newer release exists and this CLI is allowed to install it.
    Available(UpdateAvailable),
    /// A newer release exists but the channel's `min_cli_version` is
    /// above ours. Carries the actionable message from
    /// [`ChannelPointer::check_cli_compatibility`].
    ///
    /// This has to be a distinct variant rather than folding into
    /// `NoUpdate`: `min_cli_version` is the mechanism that stops an old
    /// CLI from *half*-applying a release (see the var-reseed handling
    /// in `vm update`), so a refusal the user can't see is a CLI that
    /// silently never updates its VM again.
    CliTooOld { message: String },
    /// Nothing newer — or the check couldn't run at all (network,
    /// filesystem, unparseable pointer, `AVOCADO_NO_UPDATE_CHECK`).
    /// Deliberately conflated: a check we couldn't perform must never
    /// read as "an update is waiting."
    NoUpdate,
}

/// Ask the channel whether a newer VM is available. Reads
/// `installed_version` from the caller (typically
/// `~/.avocado/vm/manifest.json`'s `.version` field).
///
/// Results are cached for 24 hours in
/// `<project-cache>/vm_update_check.json`. Set
/// `AVOCADO_NO_UPDATE_CHECK` to skip the check entirely.
///
/// Network and filesystem errors degrade to
/// [`VmUpdateStatus::NoUpdate`] — a background poll must not fail a
/// command. A `min_cli_version` refusal is *not* an error of that kind
/// and is reported as [`VmUpdateStatus::CliTooOld`].
pub async fn check_for_vm_update(channel: &str, installed_version: Option<&str>) -> VmUpdateStatus {
    if std::env::var("AVOCADO_NO_UPDATE_CHECK").is_ok() {
        return VmUpdateStatus::NoUpdate;
    }
    poll(channel, installed_version)
        .await
        .unwrap_or(VmUpdateStatus::NoUpdate)
}

async fn poll(channel: &str, installed_version: Option<&str>) -> Result<VmUpdateStatus> {
    let cache_path = cache_path();
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    // Return cached result if still fresh AND for the same channel.
    if let Some(path) = &cache_path {
        if let Ok(data) = fs::read_to_string(path) {
            if let Ok(cache) = serde_json::from_str::<UpdateCache>(&data) {
                if cache.channel == channel
                    && now_secs.saturating_sub(cache.last_checked_secs) < CHECK_INTERVAL_SECS
                {
                    return Ok(decide(&cache.pointer_json, channel, installed_version));
                }
            }
        }
    }

    // Cache miss — fetch fresh.
    let raw = fetch(channel).await?;
    if let Some(path) = &cache_path {
        let _ = fs::create_dir_all(path.parent().unwrap());
        let _ = fs::write(
            path,
            serde_json::to_string(&UpdateCache {
                last_checked_secs: now_secs,
                channel: channel.to_string(),
                pointer_json: raw.clone(),
            })?,
        );
    }
    Ok(decide(&raw, channel, installed_version))
}

fn decide(raw: &str, channel: &str, installed_version: Option<&str>) -> VmUpdateStatus {
    let Ok(pointer) = ChannelPointer::parse(raw, channel) else {
        return VmUpdateStatus::NoUpdate;
    };
    // Newness first, compatibility second. A channel whose
    // `min_cli_version` is above ours but whose release we already have
    // installed is not something to report — the user has nothing to do.
    if !pointer.is_newer_than(installed_version) {
        return VmUpdateStatus::NoUpdate;
    }
    if let Err(err) = pointer.check_cli_compatibility(env!("CARGO_PKG_VERSION")) {
        return VmUpdateStatus::CliTooOld {
            message: err.to_string(),
        };
    }
    VmUpdateStatus::Available(UpdateAvailable {
        pointer,
        installed_version: installed_version.map(str::to_string),
    })
}

async fn fetch(channel: &str) -> Result<String> {
    let base =
        std::env::var("AVOCADO_VM_CHANNEL_URL_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string());
    let url = format!("{}/{}.json", base.trim_end_matches('/'), channel);
    let client = ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

fn cache_path() -> Option<std::path::PathBuf> {
    let dirs = ProjectDirs::from("", "", "avocado")?;
    Some(dirs.cache_dir().join("vm_update_check.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version far above anything this CLI will ever report, so the
    /// test doesn't need updating when the crate version moves.
    const UNREACHABLE_CLI: &str = "999.0.0";

    fn pointer_json(version: &str, min_cli: &str) -> String {
        format!(
            r#"{{
              "channel": "stable",
              "version": "{version}",
              "released_at": "2026-08-14T00:00:00Z",
              "platforms": {{
                "avocado-qemuarm64": {{
                  "manifest_url": "https://example.invalid/{version}/arm64/manifest.json",
                  "base_url": "https://example.invalid/{version}/arm64/"
                }}
              }},
              "min_cli_version": "{min_cli}"
            }}"#
        )
    }

    #[test]
    fn a_newer_release_this_cli_cannot_install_is_reported_not_swallowed() {
        // `min_cli_version` is what stops an old CLI half-applying a
        // release. Collapsing the refusal into NoUpdate — as this used
        // to — leaves the user told they're current, forever, with no
        // hint that upgrading the CLI is what unblocks them.
        let status = decide(
            &pointer_json("0.4.0", UNREACHABLE_CLI),
            "stable",
            Some("0.3.0"),
        );
        let VmUpdateStatus::CliTooOld { message } = status else {
            panic!("expected CliTooOld");
        };
        assert!(message.contains("0.4.0"), "names the release: {message}");
        assert!(
            message.contains("avocado upgrade"),
            "tells the user what to do: {message}",
        );
    }

    #[test]
    fn an_incompatible_release_we_already_have_is_not_worth_reporting() {
        // Newness is checked before compatibility on purpose: there is
        // nothing for the user to act on when the release they'd be
        // refused is the one already installed.
        let status = decide(
            &pointer_json("0.4.0", UNREACHABLE_CLI),
            "stable",
            Some("0.4.0"),
        );
        assert!(matches!(status, VmUpdateStatus::NoUpdate));
    }

    #[test]
    fn a_newer_compatible_release_is_available() {
        let status = decide(&pointer_json("0.4.0", "0.1.0"), "stable", Some("0.3.0"));
        let VmUpdateStatus::Available(avail) = status else {
            panic!("expected Available");
        };
        assert_eq!(avail.pointer.version, "0.4.0");
        assert_eq!(avail.installed_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn an_unparseable_pointer_never_reads_as_an_available_update() {
        let status = decide("{ not json", "stable", Some("0.3.0"));
        assert!(matches!(status, VmUpdateStatus::NoUpdate));
    }
}
