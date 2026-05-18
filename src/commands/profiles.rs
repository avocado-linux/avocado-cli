//! `avocado profiles list` — discover provisioning profiles available
//! for a target by reading the stone manifest from the installed SDK
//! volume.
//!
//! Profiles aren't statically declared anywhere the desktop app can
//! parse — they're populated into the SDK container's
//! `$AVOCADO_SDK_PREFIX/stone/stone-<target>.json` file during
//! `avocado install`. This command shells out to a one-shot
//! `docker run --rm` against the SDK volume to cat that file and
//! return the parsed profiles in a stable JSON shape.
//!
//! Inspector-style long-running containers (per-project) would let us
//! amortize the docker startup; we'll layer that on later once the UI
//! has more uses for it.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tokio::process::Command as AsyncCommand;

use crate::utils::config::Config;
use crate::utils::output::{print_info, OutputLevel};
use crate::utils::output_format::{emit_json_object, OutputFormat};
use crate::utils::volume::VolumeState;

pub struct ProfilesListCommand {
    pub config_path: String,
    pub target: Option<String>,
    pub output: OutputFormat,
}

impl ProfilesListCommand {
    pub async fn execute(&self) -> Result<()> {
        // Resolve the target the same way the rest of the CLI does:
        // explicit flag wins, then the config's `default_target`, then
        // bail with a helpful message.
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load config at {}", self.config_path))?;
        let config = &composed.config;

        let target = match self
            .target
            .clone()
            .or_else(|| config.default_target.clone())
        {
            Some(t) => t,
            None => {
                return self.bail_unavailable(
                    None,
                    "No target specified and no `default_target` in avocado.yaml.",
                );
            }
        };

        // SDK image is templated in the config; the composed config has
        // already done {{ avocado.* }} interpolation against our target.
        let Some(sdk_image) = config.get_sdk_image() else {
            return self.bail_unavailable(Some(&target), "avocado.yaml has no `sdk.image` set.");
        };

        // Volume lookup: .avocado-state lives next to avocado.yaml; if
        // it's missing the project hasn't been installed yet, so there's
        // no SDK volume to inspect.
        let project_dir = Path::new(&self.config_path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let volume = match VolumeState::load_from_dir(&project_dir)? {
            Some(v) => v,
            None => {
                return self.bail_unavailable(
                    Some(&target),
                    "SDK not yet installed for this project. Run `avocado install` first.",
                );
            }
        };

        // Read the stone manifest. The path uses `uname -m` inside the
        // SDK container so the architecture-specific subdir resolves
        // automatically (x86_64 vs aarch64). Glob covers both layouts.
        let raw = match Self::read_stone_manifest(
            &volume.container_tool,
            &volume.volume_name,
            sdk_image,
            &target,
        )
        .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                return self.bail_unavailable(
                    Some(&target),
                    "Stone manifest not found in the SDK volume. The SDK install may not have completed for this target.",
                );
            }
            Err(e) => {
                return self.bail_unavailable(
                    Some(&target),
                    &format!("Could not read stone manifest from the SDK volume: {e:#}"),
                );
            }
        };

        let manifest: StoneManifest =
            serde_json::from_str(&raw).with_context(|| "Failed to parse stone manifest JSON")?;

        // Stable ordering: profile keys sorted alphabetically, with the
        // declared default (if any) bubbled to the top so the desktop
        // app's picker selects the right item by default without extra
        // logic.
        let mut profiles: Vec<_> = manifest
            .provision
            .as_ref()
            .map(|p| p.profiles.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let default = manifest
            .runtime
            .as_ref()
            .and_then(|r| r.provision_default.clone());
        profiles.sort_by(|a, b| a.0.cmp(b.0));
        if let Some(ref d) = default {
            if let Some(pos) = profiles.iter().position(|(name, _)| *name == d) {
                let entry = profiles.remove(pos);
                profiles.insert(0, entry);
            }
        }

        if self.output.is_json() {
            // The provision section is the source of truth for both
            // env blocks and field metadata. We resolve each profile's
            // enabled fields by walking the env blocks it references
            // and pulling the matching entries from `provision.fields`.
            let provision = manifest.provision.as_ref();
            emit_json_object(&json!({
                "available": true,
                "target": target,
                "default": default,
                "profiles": profiles.iter().map(|(name, p)| {
                    let fields = provision
                        .map(|prov| resolve_profile_fields(prov, p))
                        .unwrap_or_default();
                    json!({
                        "name": name,
                        "script": p.script,
                        "fields": fields,
                    })
                }).collect::<Vec<_>>(),
            }));
        } else {
            println!("Target: {target}");
            if let Some(ref d) = default {
                println!("Default profile: {d}");
            }
            if profiles.is_empty() {
                println!("No provisioning profiles declared in stone manifest.");
            } else {
                println!("Available profiles:");
                for (name, p) in &profiles {
                    let script = p.script.as_deref().unwrap_or("(no script)");
                    println!("  {name} → {script}");
                }
            }
        }

        Ok(())
    }

    /// Emit the "unavailable" envelope in JSON mode, or print a
    /// helpful prose message in human mode. Always exits Ok so the
    /// desktop app can distinguish "no profiles yet" from "command
    /// failed" via the JSON shape rather than process exit code.
    fn bail_unavailable(&self, target: Option<&str>, reason: &str) -> Result<()> {
        if self.output.is_json() {
            emit_json_object(&json!({
                "available": false,
                "target": target,
                "reason": reason,
            }));
        } else {
            print_info(reason, OutputLevel::Normal);
        }
        Ok(())
    }

    /// Spin up a one-shot container with the project's SDK volume
    /// mounted and cat the stone manifest. Returns the file contents
    /// or `None` if the file isn't present (which is the common
    /// "SDK install didn't write one for this target" case).
    ///
    /// The shell command writes to a temp variable so we can detect
    /// "file not found" without docker treating the cat failure as a
    /// container exit error (which would yield empty stdout AND a
    /// non-zero exit, indistinguishable from a real read failure).
    async fn read_stone_manifest(
        container_tool: &str,
        volume_name: &str,
        sdk_image: &str,
        target: &str,
    ) -> Result<Option<String>> {
        // `$AVOCADO_SDK_PREFIX` resolves to `<vol>/<target>/sdk/<arch>`
        // inside the container; `<arch>` is `uname -m`. Glob covers
        // both x86_64 and aarch64 layouts. The sentinel string lets us
        // distinguish "no file" from "empty file" without needing two
        // round trips.
        let script = format!(
            "f=$(ls /opt/_avocado/{target}/sdk/*/stone/stone-{target}.json 2>/dev/null | head -n 1); \
             if [ -z \"$f\" ]; then echo __AVOCADO_NO_STONE__; else cat \"$f\"; fi"
        );

        let out = AsyncCommand::new(container_tool)
            .args([
                "run",
                "--rm",
                "-v",
                &format!("{volume_name}:/opt/_avocado:ro"),
                "--entrypoint",
                "/bin/sh",
                sdk_image,
                "-c",
                &script,
            ])
            .output()
            .await
            .context("failed to spawn docker for stone manifest read")?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("docker exited {}: {stderr}", out.status);
        }
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stdout.contains("__AVOCADO_NO_STONE__") {
            return Ok(None);
        }
        Ok(Some(stdout))
    }
}

// MARK: stone manifest schema (subset we care about).
//
// Example shape (from meta-avocado-qcom/stone/stone-rubikpi3.json):
// {
//   "runtime": { "provision_default": "ufs", ... },
//   "provision": {
//     "profiles": {
//       "ufs":  { "script": "stone-provision-ufs.sh" },
//       "noop": { "script": "stone-provision-noop.sh" }
//     }
//   }
// }

#[derive(Debug, Deserialize)]
struct StoneManifest {
    runtime: Option<RuntimeSection>,
    provision: Option<ProvisionSection>,
}

#[derive(Debug, Deserialize)]
struct RuntimeSection {
    provision_default: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProvisionSection {
    profiles: BTreeMap<String, ProfileEntry>,
    /// Named env blocks. Values may contain `${VAR}` placeholders that
    /// reference entries in `fields`. Profiles select which blocks to
    /// activate via their own `envs` list.
    #[serde(default)]
    envs: BTreeMap<String, BTreeMap<String, String>>,
    /// Field metadata describing the variables that may appear in the
    /// env blocks above. Keyed by variable name (matches `${VAR}`).
    #[serde(default)]
    fields: BTreeMap<String, ProvisionField>,
}

#[derive(Debug, Deserialize)]
struct ProfileEntry {
    script: Option<String>,
    /// Names of env blocks (from `provision.envs`) this profile
    /// enables. Inline blocks aren't surfaced for form rendering —
    /// they have no field metadata to drive UI.
    #[serde(default)]
    envs: Vec<JsonValue>,
}

/// Mirrors the stone manifest's `provision.fields.<name>` entries.
/// `default` is intentionally `JsonValue` so the type-specific shape
/// (bool / string / number) round-trips to the desktop without us
/// re-encoding it.
#[derive(Debug, Deserialize)]
struct ProvisionField {
    #[serde(rename = "type")]
    field_type: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<JsonValue>,
}

/// Walk a profile's env-block references and return the field
/// metadata for every `${VAR}` placeholder it resolves to. Inline env
/// blocks (objects directly in the profile's `envs` list rather than
/// named string references) are intentionally ignored — they don't
/// participate in the form-rendering flow because they have no
/// declared field metadata.
///
/// The returned list is stable: fields appear in the order they're
/// first encountered while walking the named env blocks, with
/// duplicates suppressed.
fn resolve_profile_fields(prov: &ProvisionSection, profile: &ProfileEntry) -> Vec<JsonValue> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<JsonValue> = Vec::new();

    for env_ref in &profile.envs {
        let JsonValue::String(block_name) = env_ref else {
            continue;
        };
        let Some(block) = prov.envs.get(block_name) else {
            continue;
        };
        for value in block.values() {
            for var in extract_placeholders(value) {
                if !seen.insert(var.clone()) {
                    continue;
                }
                if let Some(field) = prov.fields.get(&var) {
                    out.push(json!({
                        "name": var,
                        "type": field.field_type,
                        "label": field.label,
                        "description": field.description,
                        "required": field.required,
                        "default": field.default,
                    }));
                }
            }
        }
    }
    out
}

/// Extract `${VAR}` placeholders from a string. Supports multiple
/// placeholders per value; ignores `$VAR` (no braces) and `$$` escapes
/// since the manifest convention is to always use `${...}`.
fn extract_placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(end) = s[i + 2..].find('}') {
                let var = &s[i + 2..i + 2 + end];
                if !var.is_empty() {
                    out.push(var.to_string());
                }
                i += 2 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_placeholders() {
        assert_eq!(extract_placeholders("${FOO}"), vec!["FOO"]);
        assert_eq!(extract_placeholders("${A}/${B}"), vec!["A", "B"]);
        assert_eq!(extract_placeholders("plain"), Vec::<String>::new());
        assert_eq!(extract_placeholders("$FOO"), Vec::<String>::new());
        assert_eq!(extract_placeholders("${}"), Vec::<String>::new());
    }

    #[test]
    fn resolves_profile_fields_via_named_env_block() {
        let raw = r#"{
            "profiles": {
                "tegraflash": { "script": "x.sh", "envs": ["flash_options"] }
            },
            "envs": {
                "flash_options": {
                    "ERASE_NVME": "${ERASE_NVME}",
                    "BOARDCTL_TARGET": "${BOARDCTL_TARGET}"
                }
            },
            "fields": {
                "ERASE_NVME": { "type": "boolean", "label": "Erase NVMe", "default": false },
                "BOARDCTL_TARGET": { "type": "string", "required": true }
            }
        }"#;
        let prov: ProvisionSection = serde_json::from_str(raw).unwrap();
        let profile = prov.profiles.get("tegraflash").unwrap();
        let fields = resolve_profile_fields(&prov, profile);
        assert_eq!(fields.len(), 2);
        let names: Vec<_> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ERASE_NVME"));
        assert!(names.contains(&"BOARDCTL_TARGET"));
    }

    #[test]
    fn resolves_jetson_flash_options_with_full_field_metadata() {
        // Matches the shape we'll see from a real Jetson stone manifest:
        // one named env block referencing five fields, mixed bool/string,
        // mixed required/default. The resolved list should round-trip
        // every field's metadata and serialize stable JSON the desktop
        // can decode without further parsing.
        let raw = r#"{
            "profiles": {
                "tegraflash": {"script":"x.sh","envs":["flash_options"]}
            },
            "envs": {
                "flash_options": {
                    "ERASE_NVME": "${ERASE_NVME}",
                    "ERASE_EMMC": "${ERASE_EMMC}",
                    "ERASE_ONLY": "${ERASE_ONLY}",
                    "BOARDCTL_TARGET": "${BOARDCTL_TARGET}",
                    "BOARDCTL_SERIAL": "${BOARDCTL_SERIAL}"
                }
            },
            "fields": {
                "ERASE_NVME": {"type":"boolean","label":"Erase NVMe","required":false,"default":false},
                "ERASE_EMMC": {"type":"boolean","label":"Erase eMMC","required":false,"default":false},
                "ERASE_ONLY": {"type":"boolean","label":"Erase only","required":false,"default":false},
                "BOARDCTL_TARGET": {"type":"string","label":"boardctl target","required":false},
                "BOARDCTL_SERIAL": {"type":"string","label":"boardctl serial","required":false}
            }
        }"#;
        let prov: ProvisionSection = serde_json::from_str(raw).unwrap();
        let profile = prov.profiles.get("tegraflash").unwrap();
        let fields = resolve_profile_fields(&prov, profile);
        assert_eq!(fields.len(), 5);
        let by_name: std::collections::BTreeMap<&str, &JsonValue> = fields
            .iter()
            .map(|f| (f["name"].as_str().unwrap(), f))
            .collect();
        assert_eq!(by_name["ERASE_NVME"]["type"], "boolean");
        assert_eq!(by_name["ERASE_NVME"]["default"], false);
        assert_eq!(by_name["BOARDCTL_TARGET"]["type"], "string");
        assert_eq!(by_name["BOARDCTL_TARGET"]["default"], JsonValue::Null);
        assert_eq!(by_name["BOARDCTL_TARGET"]["required"], false);
    }

    #[test]
    fn ignores_inline_env_blocks_and_unknown_fields() {
        let raw = r#"{
            "profiles": {
                "noop": { "script": "n.sh" },
                "weird": { "script": "w.sh", "envs": [{"K": "${UNKNOWN}"}] }
            }
        }"#;
        let prov: ProvisionSection = serde_json::from_str(raw).unwrap();
        assert!(resolve_profile_fields(&prov, prov.profiles.get("noop").unwrap()).is_empty());
        assert!(resolve_profile_fields(&prov, prov.profiles.get("weird").unwrap()).is_empty());
    }
}
