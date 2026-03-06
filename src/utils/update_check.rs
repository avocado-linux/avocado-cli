use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use reqwest::ClientBuilder;
use semver::Version;
use serde::{Deserialize, Serialize};

const CHECK_INTERVAL_SECS: u64 = 60 * 60 * 24; // 24 hours
const FETCH_TIMEOUT_SECS: u64 = 5;

#[derive(Serialize, Deserialize)]
struct UpdateCache {
    last_checked_secs: u64,
    latest_version: String,
}

#[derive(Deserialize)]
struct GithubResponse {
    tag_name: String,
}

/// Returns the latest version string if a newer version is available, otherwise `None`.
///
/// Results are cached for 24 hours to avoid unnecessary API calls. Set the
/// `AVOCADO_NO_UPDATE_CHECK` environment variable to any value to skip the check entirely.
/// Fails silently on network or filesystem errors.
pub async fn check_for_update() -> Option<String> {
    if std::env::var("AVOCADO_NO_UPDATE_CHECK").is_ok() {
        return None;
    }

    let proj_dirs = ProjectDirs::from("", "", "avocado")?;
    let cache_path = proj_dirs.cache_dir().join("update_check.json");

    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();

    // Return cached result if still fresh.
    if let Ok(data) = fs::read_to_string(&cache_path) {
        if let Ok(cache) = serde_json::from_str::<UpdateCache>(&data) {
            if now_secs.saturating_sub(cache.last_checked_secs) < CHECK_INTERVAL_SECS {
                return is_newer(&cache.latest_version);
            }
        }
    }

    // Fetch latest release from GitHub.
    let latest = fetch_latest_version().await?;

    // Persist to cache (fail silently).
    let cache = UpdateCache {
        last_checked_secs: now_secs,
        latest_version: latest.clone(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = fs::create_dir_all(proj_dirs.cache_dir());
        let _ = fs::write(&cache_path, json);
    }

    is_newer(&latest)
}

fn is_newer(latest: &str) -> Option<String> {
    let latest_str = latest.trim_start_matches('v');
    let current_str = env!("CARGO_PKG_VERSION");

    let latest_ver = Version::parse(latest_str).ok()?;
    let current_ver = Version::parse(current_str).ok()?;

    if latest_ver > current_ver {
        Some(latest_str.to_owned())
    } else {
        None
    }
}

async fn fetch_latest_version() -> Option<String> {
    let client = ClientBuilder::new()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .ok()?;

    let resp = client
        .get("https://api.github.com/repos/avocado-linux/avocado-cli/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "avocado-linux/avocado-cli")
        .send()
        .await
        .ok()?;

    let resp = resp.error_for_status().ok()?;
    let github: GithubResponse = resp.json().await.ok()?;
    Some(github.tag_name)
}
