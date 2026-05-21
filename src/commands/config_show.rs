//! `avocado config show` — surface a parsed avocado.yaml in a stable,
//! UI-friendly shape.
//!
//! Exists primarily so the desktop app doesn't have to parse YAML
//! itself. Returns ONLY the fields the UI consumes today; growing this
//! schema is a deliberate, additive operation.
//!
//! `--detail` opts into a richer `detail` sub-object with nested
//! extensions / packages / SDK info and runtime↔extension cross-refs.
//! The default (non-`--detail`) output is byte-stable.

use anyhow::{Context, Result};
use serde_json::json;

use crate::utils::config::{load_config, Config, SupportedTargets};
use crate::utils::output_format::{emit_json_object, OutputFormat};
use crate::utils::runtime_extension::RuntimeExtensionSpec;

pub struct ConfigShowCommand {
    pub config_path: String,
    pub output: OutputFormat,
    pub detail: bool,
}

impl ConfigShowCommand {
    pub async fn execute(&self) -> Result<()> {
        let cfg = load_config(&self.config_path)
            .with_context(|| format!("Failed to load config at '{}'", self.config_path))?;

        let mut payload = build_base_payload(&self.config_path, &cfg);

        if self.detail {
            // `load_composed` resolves remote/external extension configs
            // into one merged YAML view, so extension definitions
            // declared outside the main file still show up here.
            let composed = Config::load_composed(&self.config_path, cfg.default_target.as_deref())
                .with_context(|| {
                    format!(
                        "Failed to load composed config at '{}' for --detail",
                        self.config_path
                    )
                })?;
            let detail = build_detail(&composed.merged_value);
            payload
                .as_object_mut()
                .expect("base payload is an object")
                .insert("detail".to_string(), detail);
        }

        if self.output.is_json() {
            emit_json_object(&payload);
        } else {
            // Human mode prints a YAML-ish summary that mirrors the
            // structure of the JSON without requiring jq to read.
            print_human_summary(&payload);
        }

        Ok(())
    }
}

/// The original, narrow projection. Kept byte-identical to the
/// pre-`--detail` output so existing consumers don't have to special-case.
fn build_base_payload(config_path: &str, cfg: &Config) -> serde_json::Value {
    let distro = cfg.distro.as_ref().map(|d| {
        json!({
            "release": d.release,
            "channel": d.channel,
        })
    });

    let supported_targets = match &cfg.supported_targets {
        Some(SupportedTargets::All(s)) if s == "*" => json!("*"),
        Some(SupportedTargets::List(list)) => json!(list),
        Some(SupportedTargets::All(other)) => json!(other),
        None => serde_json::Value::Null,
    };

    let runtimes: Vec<_> = cfg
        .runtimes
        .as_ref()
        .map(|m| {
            let mut list: Vec<_> = m
                .iter()
                .map(|(name, r)| {
                    json!({
                        "name": name,
                        "target": r.target,
                        "target_board": r.target_board,
                        "version": r.version,
                    })
                })
                .collect();
            // Stable ordering so the UI doesn't dance around.
            list.sort_by(|a, b| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            });
            list
        })
        .unwrap_or_default();

    let provision_profiles: Vec<String> = cfg
        .provision_profiles
        .as_ref()
        .map(|m| {
            let mut names: Vec<String> = m.keys().cloned().collect();
            names.sort();
            names
        })
        .unwrap_or_default();

    // Connect mirror info — the desktop "Delete project" flow uses
    // this to decide whether to offer "also delete from Avocado
    // Connect" and what IDs to pass to `connect projects delete`.
    let connect = cfg.connect.as_ref().map(|c| {
        json!({
            "org": c.org,
            "project": c.project,
        })
    });

    json!({
        "config_path": config_path,
        "distro": distro,
        "default_target": cfg.default_target,
        "default_target_board": cfg.default_target_board,
        "default_runtime": cfg.default_runtime,
        "supported_targets": supported_targets,
        "src_dir": cfg.src_dir,
        "runtimes": runtimes,
        "provision_profiles": provision_profiles,
        "connect": connect,
    })
}

/// Build the `detail` sub-object from the composed (post-include,
/// post-interpolation) YAML view. The UI consumes this to render the
/// per-project config tree: runtimes → extensions → packages.
///
/// Shape:
/// ```jsonc
/// {
///   "runtimes":   [{ name, node_path, extensions:[{name,enabled,defined,node_path}], packages:[…] }],
///   "extensions": [{ name, node_path, types:[…], packages:[…], enable_services:[…], used_by_runtimes:[…] }],
///   "sdk":        { "image": "…", "packages": […] }    // null when no sdk block
/// }
/// ```
fn build_detail(merged: &serde_yaml::Value) -> serde_json::Value {
    let extension_names: std::collections::HashSet<String> = merged
        .get("extensions")
        .and_then(|e| e.as_mapping())
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Walk runtimes first so we can compute `used_by_runtimes` for each
    // extension in a single pass.
    let mut runtimes_out: Vec<serde_json::Value> = Vec::new();
    // extension name → sorted set of runtime names that reference it.
    let mut used_by: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();

    if let Some(rt_map) = merged.get("runtimes").and_then(|r| r.as_mapping()) {
        let mut entries: Vec<(&str, &serde_yaml::Value)> = rt_map
            .iter()
            .filter_map(|(k, v)| k.as_str().map(|n| (n, v)))
            .collect();
        entries.sort_by_key(|(n, _)| *n);

        for (rt_name, rt_value) in entries {
            let exts = extract_runtime_extensions(rt_value, &extension_names);
            for ext in &exts {
                if let Some(name) = ext["name"].as_str() {
                    if ext["defined"].as_bool().unwrap_or(false) {
                        used_by
                            .entry(name.to_string())
                            .or_default()
                            .insert(rt_name.to_string());
                    }
                }
            }

            let packages = extract_package_names(rt_value.get("packages"));

            runtimes_out.push(json!({
                "name": rt_name,
                "node_path": format!("/runtimes/{rt_name}"),
                "extensions": exts,
                "packages": packages,
            }));
        }
    }

    // Now walk extensions, decorating each with the runtimes that pull it in.
    let mut extensions_out: Vec<serde_json::Value> = Vec::new();
    if let Some(ext_map) = merged.get("extensions").and_then(|e| e.as_mapping()) {
        let mut entries: Vec<(&str, &serde_yaml::Value)> = ext_map
            .iter()
            .filter_map(|(k, v)| k.as_str().map(|n| (n, v)))
            .collect();
        entries.sort_by_key(|(n, _)| *n);

        for (ext_name, ext_value) in entries {
            let types = extract_string_list(ext_value.get("types"));
            let packages = extract_package_names(ext_value.get("packages"));
            let enable_services = extract_string_list(ext_value.get("enable_services"));
            let used: Vec<String> = used_by
                .get(ext_name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();

            extensions_out.push(json!({
                "name": ext_name,
                "node_path": format!("/extensions/{ext_name}"),
                "types": types,
                "packages": packages,
                "enable_services": enable_services,
                "used_by_runtimes": used,
            }));
        }
    }

    let sdk = merged.get("sdk").and_then(|s| s.as_mapping()).map(|m| {
        let image = m
            .get(serde_yaml::Value::String("image".to_string()))
            .and_then(|v| v.as_str())
            .map(String::from);
        let packages = extract_package_names(
            m.get(serde_yaml::Value::String("packages".to_string()))
                // `dependencies` is the deprecated alias the typed
                // SdkConfig also accepts.
                .or_else(|| m.get(serde_yaml::Value::String("dependencies".to_string()))),
        );
        json!({
            "image": image,
            "packages": packages,
        })
    });

    json!({
        "runtimes": runtimes_out,
        "extensions": extensions_out,
        "sdk": sdk,
    })
}

/// Extract a runtime's `extensions:` list as structured entries with
/// resolution flags. Each entry:
/// - `name` — the extension reference name (post-interpolation).
/// - `enabled` — auto-activate when the runtime is provisioned (the
///   `- foo: { enabled: false }` map form sets this to false).
/// - `defined` — true iff the name resolves to a top-level
///   `extensions.<name>` entry. False flags broken references that
///   the UI should mark with a warning.
/// - `node_path` — pointer to the extension definition, or null when
///   undefined.
fn extract_runtime_extensions(
    rt_value: &serde_yaml::Value,
    extension_names: &std::collections::HashSet<String>,
) -> Vec<serde_json::Value> {
    let Some(list) = rt_value.get("extensions").and_then(|e| e.as_sequence()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(RuntimeExtensionSpec::parse_entry)
        .map(|spec| {
            let defined = extension_names.contains(&spec.name);
            let node_path = if defined {
                Some(format!("/extensions/{}", spec.name))
            } else {
                None
            };
            json!({
                "name": spec.name,
                "enabled": spec.enabled,
                "defined": defined,
                "node_path": node_path,
            })
        })
        .collect()
}

/// Extract package names from either grammar:
/// - Map form: `packages: { nginx: '*', curl: '1.0' }` → keys.
/// - List form: `packages: [nginx, curl]` → items.
/// - Anything else (missing, scalar, …) → empty.
///
/// Version specs are intentionally dropped — the project-detail tree
/// shows what's in the runtime, not at which version. Add a separate
/// detailed projection later if a use-case appears.
fn extract_package_names(value: Option<&serde_yaml::Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut names: Vec<String> = if let Some(map) = value.as_mapping() {
        map.keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect()
    } else if let Some(list) = value.as_sequence() {
        list.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        Vec::new()
    };
    names.sort();
    names.dedup();
    names
}

/// Extract a sequence of strings from a YAML field, tolerating
/// missing/non-sequence values (return empty).
fn extract_string_list(value: Option<&serde_yaml::Value>) -> Vec<String> {
    let Some(seq) = value.and_then(|v| v.as_sequence()) else {
        return Vec::new();
    };
    seq.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

fn print_human_summary(payload: &serde_json::Value) {
    println!("Config: {}", payload["config_path"].as_str().unwrap_or("?"));
    if let Some(distro) = payload["distro"].as_object() {
        let release = distro
            .get("release")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let channel = distro
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("  distro: {release}/{channel}");
    }
    if let Some(t) = payload["default_target"].as_str() {
        println!("  default_target: {t}");
    }
    match &payload["supported_targets"] {
        serde_json::Value::String(s) => println!("  supported_targets: {s}"),
        serde_json::Value::Array(list) => {
            let names: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            println!("  supported_targets: [{}]", names.join(", "));
        }
        _ => {}
    }
    if let Some(rs) = payload["runtimes"].as_array() {
        if !rs.is_empty() {
            println!("  runtimes:");
            for r in rs {
                let name = r["name"].as_str().unwrap_or("?");
                let target = r["target"].as_str().unwrap_or("(inherits default)");
                println!("    - {name} → {target}");
            }
        }
    }
    if let Some(ps) = payload["provision_profiles"].as_array() {
        if !ps.is_empty() {
            let names: Vec<String> = ps
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            println!("  provision_profiles: [{}]", names.join(", "));
        }
    }
    if let Some(c) = payload["connect"].as_object() {
        let org = c.get("org").and_then(|v| v.as_str());
        let project = c.get("project").and_then(|v| v.as_str());
        if org.is_some() || project.is_some() {
            println!(
                "  connect: org={} project={}",
                org.unwrap_or("-"),
                project.unwrap_or("-"),
            );
        }
    }
    if let Some(detail) = payload.get("detail") {
        print_human_detail(detail);
    }
}

fn print_human_detail(detail: &serde_json::Value) {
    if let Some(rs) = detail.get("runtimes").and_then(|v| v.as_array()) {
        if !rs.is_empty() {
            println!("  detail.runtimes:");
            for r in rs {
                let name = r["name"].as_str().unwrap_or("?");
                println!("    - {name}:");
                if let Some(exts) = r.get("extensions").and_then(|v| v.as_array()) {
                    if !exts.is_empty() {
                        let parts: Vec<String> = exts
                            .iter()
                            .map(|e| {
                                let n = e["name"].as_str().unwrap_or("?");
                                let mark = if e["defined"].as_bool().unwrap_or(false) {
                                    ""
                                } else {
                                    "!"
                                };
                                format!("{n}{mark}")
                            })
                            .collect();
                        println!("        extensions: [{}]", parts.join(", "));
                    }
                }
                if let Some(pkgs) = r.get("packages").and_then(|v| v.as_array()) {
                    if !pkgs.is_empty() {
                        let names: Vec<String> = pkgs
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        println!("        packages:   [{}]", names.join(", "));
                    }
                }
            }
        }
    }
    if let Some(es) = detail.get("extensions").and_then(|v| v.as_array()) {
        if !es.is_empty() {
            println!("  detail.extensions:");
            for e in es {
                let name = e["name"].as_str().unwrap_or("?");
                let used: Vec<String> = e
                    .get("used_by_runtimes")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let pkg_count = e
                    .get("packages")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let used_s = if used.is_empty() {
                    "(unused)".to_string()
                } else {
                    format!("used by [{}]", used.join(", "))
                };
                println!("    - {name}: {pkg_count} packages, {used_s}");
            }
        }
    }
    if let Some(sdk) = detail.get("sdk").and_then(|v| v.as_object()) {
        let image = sdk.get("image").and_then(|v| v.as_str()).unwrap_or("?");
        let pkg_count = sdk
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        println!("  detail.sdk: image={image}, {pkg_count} packages");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn extracts_package_names_from_map_form() {
        let v = yaml("nginx: '*'\ncurl: '1.0'\n");
        let names = extract_package_names(Some(&v));
        assert_eq!(names, vec!["curl".to_string(), "nginx".to_string()]);
    }

    #[test]
    fn extracts_package_names_from_list_form() {
        let v = yaml("- nginx\n- curl\n");
        let names = extract_package_names(Some(&v));
        assert_eq!(names, vec!["curl".to_string(), "nginx".to_string()]);
    }

    #[test]
    fn package_names_missing_returns_empty() {
        assert!(extract_package_names(None).is_empty());
    }

    #[test]
    fn detail_links_runtimes_to_extensions_and_flags_broken_refs() {
        let merged = yaml(
            r#"
runtimes:
  dev:
    extensions:
      - real-ext
      - missing-ext
      - disabled-ext: { enabled: false }
    packages:
      avocado-runtime: '*'
extensions:
  real-ext:
    types: [sysext]
    packages:
      nginx: '*'
      curl: '*'
  disabled-ext:
    types: [confext]
    enable_services:
      - foo.service
sdk:
  image: docker.io/x/y:1
  packages:
    nativesdk-foo: '*'
"#,
        );

        let detail = build_detail(&merged);

        let dev = &detail["runtimes"][0];
        assert_eq!(dev["name"], "dev");
        assert_eq!(dev["node_path"], "/runtimes/dev");

        let exts = dev["extensions"].as_array().unwrap();
        assert_eq!(exts.len(), 3);

        let real = exts.iter().find(|e| e["name"] == "real-ext").unwrap();
        assert_eq!(real["defined"], true);
        assert_eq!(real["enabled"], true);
        assert_eq!(real["node_path"], "/extensions/real-ext");

        let missing = exts.iter().find(|e| e["name"] == "missing-ext").unwrap();
        assert_eq!(missing["defined"], false);
        assert!(missing["node_path"].is_null());

        let disabled = exts.iter().find(|e| e["name"] == "disabled-ext").unwrap();
        assert_eq!(disabled["enabled"], false);
        assert_eq!(disabled["defined"], true);

        // Runtime packages list (just names, no versions).
        let pkgs: Vec<&str> = dev["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(pkgs, vec!["avocado-runtime"]);

        // Extensions sorted alphabetically; only defined ones appear here
        // (missing-ext was a runtime ref, not a definition).
        let ext_list = detail["extensions"].as_array().unwrap();
        assert_eq!(ext_list.len(), 2);
        assert_eq!(ext_list[0]["name"], "disabled-ext");
        assert_eq!(ext_list[1]["name"], "real-ext");

        // used_by_runtimes is computed by inversion from runtime extension lists.
        assert_eq!(ext_list[1]["used_by_runtimes"], json!(["dev"]));
        let real_pkgs: Vec<&str> = ext_list[1]["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(real_pkgs, vec!["curl", "nginx"]);

        // enable_services round-trips.
        let services: Vec<&str> = ext_list[0]["enable_services"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(services, vec!["foo.service"]);

        // SDK summary present and stripped to image + package names.
        assert_eq!(detail["sdk"]["image"], "docker.io/x/y:1");
        let sdk_pkgs: Vec<&str> = detail["sdk"]["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(sdk_pkgs, vec!["nativesdk-foo"]);
    }

    #[test]
    fn detail_handles_empty_config() {
        let merged = yaml("default_target: qemux86-64\n");
        let detail = build_detail(&merged);
        assert!(detail["runtimes"].as_array().unwrap().is_empty());
        assert!(detail["extensions"].as_array().unwrap().is_empty());
        assert!(detail["sdk"].is_null());
    }

    #[test]
    fn extension_unused_by_any_runtime() {
        let merged = yaml(
            r#"
runtimes:
  dev:
    extensions: []
extensions:
  orphan:
    types: [sysext]
"#,
        );
        let detail = build_detail(&merged);
        let exts = detail["extensions"].as_array().unwrap();
        assert_eq!(exts.len(), 1);
        assert_eq!(exts[0]["name"], "orphan");
        assert!(exts[0]["used_by_runtimes"].as_array().unwrap().is_empty());
    }
}
