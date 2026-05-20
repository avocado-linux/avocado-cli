//! Per-VM persistent configuration at `~/.avocado/vm/config.yaml`.
//!
//! Read by both avocado-cli and avocado-desktop. The CLI surface
//! (`avocado vm config get/set/list/unset`) is the source of truth; the
//! desktop UI edits this same file so the two stay in lockstep.
//!
//! Schema is intentionally small to start — network DNS only — and grows
//! by adding keys. Unknown keys round-trip via `serde(flatten)` on a
//! `BTreeMap<String, serde_yaml::Value>` so an older CLI doesn't drop
//! settings a newer desktop wrote.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::state::VmPaths;

/// Top-level config. Every section is optional so a partial file is valid
/// and a freshly-written one only contains keys the user actually set.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,

    /// Forward-compat bucket for keys this CLI version doesn't know about.
    /// Preserved verbatim on save so a newer desktop's settings survive an
    /// older CLI's round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Override the guest's DNS resolvers. Applied post-boot via
    /// `resolvectl dns eth0 …` so it survives `vm stop` / `vm start`.
    /// `None` keeps slirp's DHCP-provided 10.0.2.3 (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<Vec<String>>,

    /// DNS search domains. Applied as `resolvectl domain eth0 …`. When
    /// `dns` is set without `dns_search`, the apply step also installs
    /// `~.` so the public resolvers handle all suffixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_search: Option<Vec<String>>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl VmConfig {
    /// Load the config from `paths.config_file()`. A missing file is *not*
    /// an error — returns `Self::default()` so callers can treat "no config"
    /// and "empty config" identically.
    pub fn load(paths: &VmPaths) -> Result<Self> {
        Self::load_from(&paths.config_file())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Save atomically via a sibling tempfile + rename. The directory is
    /// created if missing so first-write doesn't require a prior `vm start`.
    pub fn save(&self, paths: &VmPaths) -> Result<()> {
        paths.ensure()?;
        self.save_to(&paths.config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("config path {} has no parent", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let yaml = serde_yaml::to_string(self).context("serializing vm config")?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).context("temp file for vm config")?;
        use std::io::Write;
        tmp.write_all(yaml.as_bytes())
            .context("write vm config temp file")?;
        tmp.flush().context("flush vm config temp file")?;
        tmp.persist(path)
            .with_context(|| format!("persist {}", path.display()))?;
        Ok(())
    }

    /// Read a dotted key (e.g. `network.dns`) as a YAML value. Unknown
    /// sections / keys return `Ok(None)`.
    pub fn get(&self, key: &str) -> Result<Option<serde_yaml::Value>> {
        let value = serde_yaml::to_value(self).context("snapshot config as yaml")?;
        Ok(walk(&value, key))
    }

    /// Set a dotted key. Strings that parse as YAML scalars (numbers, bools,
    /// lists) are coerced; everything else is stored as a string. If the
    /// resulting config fails to deserialize because the schema expects a
    /// sequence at this key (e.g. `network.dns` with a single value), the
    /// scalar is automatically promoted to a single-element list — so
    /// `vm config set network.dns 1.1.1.1` does the obvious thing without
    /// the caller having to know the schema.
    pub fn set(&mut self, key: &str, raw_values: &[String]) -> Result<()> {
        let value = parse_value(raw_values)?;
        let base = serde_yaml::to_value(&*self).context("snapshot config as yaml")?;

        let try_value = |v: serde_yaml::Value| -> Result<VmConfig, anyhow::Error> {
            let mut snap = base.clone();
            place(&mut snap, key, v)?;
            serde_yaml::from_value(snap).map_err(anyhow::Error::from)
        };

        match try_value(value.clone()) {
            Ok(updated) => {
                *self = updated;
                Ok(())
            }
            Err(first) => {
                // Retry by wrapping in a one-element list — covers the
                // common case `set network.dns 1.1.1.1` where the schema
                // is `Vec<String>` but the user only passed one value.
                if !matches!(value, serde_yaml::Value::Sequence(_)) {
                    let wrapped = serde_yaml::Value::Sequence(vec![value]);
                    if let Ok(updated) = try_value(wrapped) {
                        *self = updated;
                        return Ok(());
                    }
                }
                Err(first).with_context(|| format!("invalid value for {key}"))
            }
        }
    }

    /// Remove a dotted key. No-op if it doesn't exist.
    pub fn unset(&mut self, key: &str) -> Result<()> {
        let mut snap = serde_yaml::to_value(&*self).context("snapshot config as yaml")?;
        remove(&mut snap, key)?;
        let updated: VmConfig = serde_yaml::from_value(snap)
            .with_context(|| format!("invalid config after unset {key}"))?;
        *self = updated;
        Ok(())
    }
}

/// Parse one-or-many strings into a YAML value. Single value → scalar (or
/// list/bool/number if it parses as YAML); multiple values → sequence of
/// strings. Empty input is an error so `vm config set k` with no value
/// doesn't silently clear the key (use `unset` for that).
fn parse_value(raw: &[String]) -> Result<serde_yaml::Value> {
    match raw.len() {
        0 => bail!("missing value (use `vm config unset` to remove)"),
        1 => {
            // Accept YAML scalars (numbers, bools, JSON-ish lists). Fall back
            // to plain string if the parse is ambiguous (e.g. "8.8.8.8" parses
            // as a string already, but "1.1.1.1" should remain a string too).
            let s = &raw[0];
            match serde_yaml::from_str::<serde_yaml::Value>(s) {
                Ok(serde_yaml::Value::Bool(b)) => Ok(serde_yaml::Value::Bool(b)),
                Ok(serde_yaml::Value::Number(n)) => Ok(serde_yaml::Value::Number(n)),
                Ok(serde_yaml::Value::Sequence(seq)) => Ok(serde_yaml::Value::Sequence(seq)),
                _ => Ok(serde_yaml::Value::String(s.clone())),
            }
        }
        _ => Ok(serde_yaml::Value::Sequence(
            raw.iter()
                .map(|s| serde_yaml::Value::String(s.clone()))
                .collect(),
        )),
    }
}

fn walk(value: &serde_yaml::Value, key: &str) -> Option<serde_yaml::Value> {
    let mut cur = value;
    for part in key.split('.') {
        let map = cur.as_mapping()?;
        cur = map.get(serde_yaml::Value::String(part.into()))?;
    }
    Some(cur.clone())
}

fn place(root: &mut serde_yaml::Value, key: &str, new_value: serde_yaml::Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!("invalid key {key:?}: empty path segment");
    }
    if !root.is_mapping() {
        *root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        let map = cur
            .as_mapping_mut()
            .ok_or_else(|| anyhow!("cannot descend into non-mapping at {part}"))?;
        let entry = map
            .entry(serde_yaml::Value::String((*part).into()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        if !entry.is_mapping() {
            *entry = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        cur = entry;
    }
    let last = parts[parts.len() - 1];
    let map = cur
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("cannot set {key}: not a mapping"))?;
    map.insert(serde_yaml::Value::String(last.into()), new_value);
    Ok(())
}

fn remove(root: &mut serde_yaml::Value, key: &str) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!("invalid key {key:?}: empty path segment");
    }
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        let Some(map) = cur.as_mapping_mut() else {
            return Ok(());
        };
        let Some(next) = map.get_mut(serde_yaml::Value::String((*part).into())) else {
            return Ok(());
        };
        cur = next;
    }
    if let Some(map) = cur.as_mapping_mut() {
        map.remove(serde_yaml::Value::String(parts[parts.len() - 1].into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_is_default() {
        let tmp = tempdir().unwrap();
        let cfg = VmConfig::load_from(&tmp.path().join("config.yaml")).unwrap();
        assert_eq!(cfg, VmConfig::default());
    }

    #[test]
    fn round_trip_through_yaml() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let cfg = VmConfig {
            network: Some(NetworkConfig {
                dns: Some(vec!["1.1.1.1".into(), "8.8.8.8".into()]),
                dns_search: None,
                extra: Default::default(),
            }),
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = VmConfig::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn set_then_get_dns() {
        let mut cfg = VmConfig::default();
        cfg.set("network.dns", &["1.1.1.1".into(), "8.8.8.8".into()])
            .unwrap();
        let got = cfg.get("network.dns").unwrap().unwrap();
        let seq = got.as_sequence().unwrap();
        assert_eq!(seq.len(), 2);
        assert_eq!(seq[0].as_str(), Some("1.1.1.1"));
        assert_eq!(seq[1].as_str(), Some("8.8.8.8"));
    }

    #[test]
    fn unset_removes_key() {
        let mut cfg = VmConfig::default();
        cfg.set("network.dns", &["1.1.1.1".into()]).unwrap();
        cfg.unset("network.dns").unwrap();
        assert!(cfg.get("network.dns").unwrap().is_none());
    }

    #[test]
    fn unknown_keys_round_trip() {
        // Simulate a newer desktop writing a key this CLI doesn't model.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let yaml =
            "network:\n  dns:\n    - 1.1.1.1\n  future_knob: 42\nother_section:\n  foo: bar\n";
        std::fs::write(&path, yaml).unwrap();
        let loaded = VmConfig::load_from(&path).unwrap();
        loaded.save_to(&path).unwrap();
        let reread = std::fs::read_to_string(&path).unwrap();
        assert!(
            reread.contains("future_knob"),
            "lost forward-compat key: {reread}"
        );
        assert!(
            reread.contains("other_section"),
            "lost unknown section: {reread}"
        );
    }

    #[test]
    fn set_rejects_empty_value() {
        let mut cfg = VmConfig::default();
        let err = cfg.set("network.dns", &[]).unwrap_err();
        assert!(err.to_string().contains("missing value"));
    }

    #[test]
    fn empty_segment_in_key_rejected() {
        let mut cfg = VmConfig::default();
        let err = cfg.set("network..dns", &["1.1.1.1".into()]).unwrap_err();
        // Use alternate-format ({:#}) so the underlying `place` error
        // (which `set` wraps in "invalid value for …" context) is included.
        let full = format!("{err:#}");
        assert!(
            full.contains("empty path segment"),
            "unexpected error chain: {full}"
        );
    }
}
