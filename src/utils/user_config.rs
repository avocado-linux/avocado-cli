//! `~/.avocado/config.yaml` — user-level CLI configuration.
//!
//! This is intentionally distinct from the project-level [`crate::utils::config`]
//! that parses `avocado.yaml`. The project config describes a build; this
//! one describes the user's preferences for the CLI itself (which channel
//! they want VM updates from, whether to opt out of auto-updates, where to
//! point at a custom dev VM).
//!
//! The file is optional. A missing file means "all defaults".

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Default channel name when neither flag nor config overrides.
pub const DEFAULT_VM_CHANNEL: &str = "stable";

/// Whole `~/.avocado/config.toml` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub vm: VmSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VmSection {
    /// VM update channel — `"stable"`, `"beta"`, or future names.
    #[serde(default)]
    pub channel: Option<String>,
    /// Override the install dir from the default `~/.avocado/vm/`. When
    /// set, the auto-updater is a no-op for this user (they're pointing
    /// at a dev VM and don't want their work clobbered).
    #[serde(default)]
    #[allow(dead_code)] // wired in a follow-up
    pub dir: Option<PathBuf>,
    /// Explicit opt-out of `avocado vm update` even when `dir` is not set.
    #[serde(default)]
    #[allow(dead_code)]
    pub auto_update: Option<bool>,
}

impl UserConfig {
    /// Default path: `$HOME/.avocado/config.yaml`. Returns `None` when
    /// HOME is unavailable (extremely rare).
    pub fn default_path() -> Option<PathBuf> {
        let dirs = directories::BaseDirs::new()?;
        Some(dirs.home_dir().join(".avocado").join("config.yaml"))
    }

    /// Load from the default path. A missing file is treated as
    /// "all defaults" — not an error. Returns `Err` only on a present
    /// file that fails to parse.
    pub fn load() -> Result<Self> {
        let Some(path) = Self::default_path() else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Resolved VM channel — explicit `flag`, then `[vm].channel`,
    /// then [`DEFAULT_VM_CHANNEL`].
    pub fn vm_channel(&self, flag: Option<&str>) -> String {
        flag.map(str::to_string)
            .or_else(|| self.vm.channel.clone())
            .unwrap_or_else(|| DEFAULT_VM_CHANNEL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_all_none() {
        let c = UserConfig::default();
        assert_eq!(c.vm_channel(None), "stable");
    }

    #[test]
    fn flag_overrides_config() {
        let c = UserConfig {
            vm: VmSection {
                channel: Some("beta".into()),
                ..Default::default()
            },
        };
        assert_eq!(c.vm_channel(Some("nightly")), "nightly");
        assert_eq!(c.vm_channel(None), "beta");
    }
}
