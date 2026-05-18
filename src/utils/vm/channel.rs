//! Channel pointer (`stable.json`, `beta.json`) parsing.
//!
//! The channel pointer is a small JSON document published at
//! `https://repo.avocadolinux.org/releases/vm/<channel>.json` that says
//! "as of today, this channel is at version X.Y.Z, and you can find the
//! per-platform manifests at the URLs below". The updater polls this on
//! a 24h cadence; the per-release [`super::manifest::Manifest`] is
//! fetched lazily once an update is actually requested.
//!
//! Shape matches `avocado-vm/release/stable.json` (which is the
//! source-of-truth file uploaded by the avocado-vm release workflow).

use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use std::collections::HashMap;

/// Top-level channel pointer.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPointer {
    /// Echoes the channel name. Sanity check the URL → file mapping.
    pub channel: String,
    /// Current version this channel advertises.
    pub version: String,
    /// ISO-8601 UTC. Display-only.
    #[allow(dead_code)]
    pub released_at: String,
    /// Per-platform discovery info, keyed by manifest's `.platform`
    /// string (e.g. `"avocado-qemuarm64"`).
    pub platforms: HashMap<String, PlatformEntry>,
    /// Refuse to install if Avocado.app is older than this. Optional —
    /// not all releases set it. (Future hook; CLI doesn't enforce yet.)
    #[serde(default)]
    #[allow(dead_code)]
    pub min_app_version: Option<String>,
    /// Refuse to install if the running CLI is older than this. The
    /// CLI checks this against `env!("CARGO_PKG_VERSION")` and refuses
    /// to advertise the update — users see "VM X.Y.Z available; run
    /// `avocado upgrade` first".
    #[serde(default)]
    pub min_cli_version: Option<String>,
}

/// Per-platform pointer.
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformEntry {
    /// Full URL to the per-arch `manifest.json`.
    pub manifest_url: String,
    /// Trailing-slash URL the updater prepends to each artifact's
    /// `file` field when downloading.
    pub base_url: String,
}

impl ChannelPointer {
    /// Parse a channel pointer JSON string with shape checks. Rejects
    /// pointers whose self-declared `channel` doesn't match the
    /// `expected_channel` (defends against accidental wrong-URL fetches).
    pub fn parse(raw: &str, expected_channel: &str) -> Result<Self> {
        let p: Self = serde_json::from_str(raw).context("parsing channel pointer")?;
        if p.channel != expected_channel {
            bail!(
                "channel pointer declares channel='{}', expected '{}'",
                p.channel,
                expected_channel,
            );
        }
        Ok(p)
    }

    /// Lookup the per-platform pointer for a given manifest platform
    /// string. Returns `None` when the channel doesn't advertise this
    /// platform.
    pub fn platform(&self, platform: &str) -> Option<&PlatformEntry> {
        self.platforms.get(platform)
    }

    /// True if `self.version` is strictly newer than `installed`. An
    /// `installed` value of `None` means "we don't know what's installed";
    /// treated as out-of-date so the user always sees the available
    /// version. Non-semver versions on either side are compared lexically
    /// — strictly worse than semver but matches what a user would expect
    /// for the "0.0.0-dev" / pre-release prefix case.
    pub fn is_newer_than(&self, installed: Option<&str>) -> bool {
        match installed {
            None => true,
            Some(local) => match (Version::parse(&self.version), Version::parse(local)) {
                (Ok(remote), Ok(local)) => remote > local,
                _ => self.version.as_str() > local,
            },
        }
    }

    /// Refuse to advertise this release if it requires a newer CLI than
    /// what's running. Returns `Err` with a user-actionable message;
    /// `Ok(())` means the running CLI is sufficient (or the field is
    /// unset).
    pub fn check_cli_compatibility(&self, current_cli: &str) -> Result<()> {
        let Some(min) = &self.min_cli_version else {
            return Ok(());
        };
        let (Ok(min), Ok(cur)) = (Version::parse(min), Version::parse(current_cli)) else {
            // If either side is unparseable, don't block — the user can
            // file an issue and we can tighten later. Better to advertise
            // than to silently freeze.
            return Ok(());
        };
        if cur < min {
            bail!(
                "VM release {} requires avocado-cli >= {} (you have {}). \
                 Run `avocado upgrade` to update the CLI first.",
                self.version,
                min,
                current_cli,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> &'static str {
        r#"{
          "channel": "stable",
          "version": "0.2.0",
          "released_at": "2026-05-18T20:30:00Z",
          "platforms": {
            "avocado-qemuarm64": {
              "manifest_url": "https://repo.avocadolinux.org/releases/vm/0.2.0/arm64/manifest.json",
              "base_url":     "https://repo.avocadolinux.org/releases/vm/0.2.0/arm64/"
            }
          },
          "min_cli_version": "0.39.0"
        }"#
    }

    #[test]
    fn parses_a_stable_pointer() {
        let p = ChannelPointer::parse(fixture(), "stable").unwrap();
        assert_eq!(p.version, "0.2.0");
        assert!(p.platform("avocado-qemuarm64").is_some());
    }

    #[test]
    fn rejects_wrong_channel() {
        let err = ChannelPointer::parse(fixture(), "beta").unwrap_err();
        assert!(err.to_string().contains("expected 'beta'"));
    }

    #[test]
    fn is_newer_than() {
        let p = ChannelPointer::parse(fixture(), "stable").unwrap();
        assert!(p.is_newer_than(Some("0.1.0")));
        assert!(!p.is_newer_than(Some("0.2.0")));
        assert!(!p.is_newer_than(Some("0.3.0")));
        assert!(p.is_newer_than(None));
    }

    #[test]
    fn refuses_when_cli_too_old() {
        let p = ChannelPointer::parse(fixture(), "stable").unwrap();
        assert!(p.check_cli_compatibility("0.38.0").is_err());
        assert!(p.check_cli_compatibility("0.39.0").is_ok());
        assert!(p.check_cli_compatibility("1.0.0").is_ok());
    }
}
