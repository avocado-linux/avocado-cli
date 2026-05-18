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

/// Returns the channel pointer if a newer VM is available for the
/// given channel, otherwise `None`. Reads `installed_version` from the
/// caller (typically `~/.avocado/vm/manifest.json`'s `.version` field).
///
/// Results are cached for 24 hours in
/// `<project-cache>/vm_update_check.json`. Set
/// `AVOCADO_NO_UPDATE_CHECK` to skip the check entirely.
///
/// Fails silently on network or filesystem errors — returns `None`.
pub async fn check_for_vm_update(
    channel: &str,
    installed_version: Option<&str>,
) -> Option<UpdateAvailable> {
    if std::env::var("AVOCADO_NO_UPDATE_CHECK").is_ok() {
        return None;
    }
    poll(channel, installed_version).await.ok().flatten()
}

async fn poll(channel: &str, installed_version: Option<&str>) -> Result<Option<UpdateAvailable>> {
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

fn decide(raw: &str, channel: &str, installed_version: Option<&str>) -> Option<UpdateAvailable> {
    let pointer = ChannelPointer::parse(raw, channel).ok()?;
    pointer
        .check_cli_compatibility(env!("CARGO_PKG_VERSION"))
        .ok()?;
    if !pointer.is_newer_than(installed_version) {
        return None;
    }
    Some(UpdateAvailable {
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
