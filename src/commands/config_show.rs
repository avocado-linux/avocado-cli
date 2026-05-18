//! `avocado config show` — surface a parsed avocado.yaml in a stable,
//! UI-friendly shape.
//!
//! Exists primarily so the desktop app doesn't have to parse YAML
//! itself. Returns ONLY the fields the UI consumes today; growing this
//! schema is a deliberate, additive operation.

use anyhow::{Context, Result};
use serde_json::json;

use crate::utils::config::{load_config, SupportedTargets};
use crate::utils::output_format::{emit_json_object, OutputFormat};

pub struct ConfigShowCommand {
    pub config_path: String,
    pub output: OutputFormat,
}

impl ConfigShowCommand {
    pub async fn execute(&self) -> Result<()> {
        let cfg = load_config(&self.config_path)
            .with_context(|| format!("Failed to load config at '{}'", self.config_path))?;

        // Stable, narrow projection. The full Config struct contains
        // many internal fields the UI doesn't need; baking them all
        // in would couple the wire format to the CLI's internals.
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

        let payload = json!({
            "config_path": self.config_path,
            "distro": distro,
            "default_target": cfg.default_target,
            "default_target_board": cfg.default_target_board,
            "default_runtime": cfg.default_runtime,
            "supported_targets": supported_targets,
            "src_dir": cfg.src_dir,
            "runtimes": runtimes,
            "provision_profiles": provision_profiles,
            "connect": connect,
        });

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
}
