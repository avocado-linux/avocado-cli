//! `avocado stack`: see and reclaim the space stacks use.

use anyhow::{bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::stacks::{config_path, human_bytes, orphan_reason, parse_size, settings, Helper};
use crate::utils::volume::VolumeState;

pub struct StackCommand {
    container_tool: String,
}

fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(secs);
    match d {
        0..=119 => "just now".to_string(),
        120..=7199 => format!("{}m ago", d / 60),
        7200..=172_799 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn short(k: &str) -> &str {
    &k[..k.len().min(12)]
}

fn bytes(v: &Value) -> String {
    human_bytes(v.as_u64().unwrap_or(0))
}

/// The status report as text: totals, then projects, then layers (largest unique first).
pub fn render_status(r: &Value) -> String {
    let mut o = String::new();
    o += &format!(
        "Stacks {} ({} free)\n",
        r["root"].as_str().unwrap_or(""),
        bytes(&r["free_bytes"])
    );
    o += &format!(
        "  total {}: objects {} ({} files), layer metadata {}, private uppers {}\n",
        bytes(&r["total_bytes"]),
        bytes(&r["object_bytes"]),
        r["objects"],
        bytes(&r["layer_meta_bytes"]),
        bytes(&r["upper_bytes"])
    );
    o += &format!(
        "  reclaimable now (`avocado stack gc`): {}\n",
        bytes(&r["reclaimable_bytes"])
    );
    let empty = vec![];
    let views = r["views"].as_array().unwrap_or(&empty);
    let layers = r["layers"].as_array().unwrap_or(&empty);
    let label_of = |key: &str| {
        layers
            .iter()
            .find(|l| l["key"] == key)
            .and_then(|l| l["label"].as_str())
            .unwrap_or("")
            .to_string()
    };
    o += &format!("\nProjects ({})\n", views.len());
    let project_of = |v: &Value| {
        v["meta"]["project"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("(view {})", v["name"].as_str().unwrap_or("")))
    };
    let w = views
        .iter()
        .map(|v| project_of(v).len())
        .max()
        .unwrap_or(0)
        .max(7);
    if !views.is_empty() {
        o += &format!(
            "  {:<w$} {:<16} {:<26} {:>10}  {}\n",
            "PROJECT", "TARGET", "LAYER", "PRIVATE", "STATE"
        );
    }
    for v in views {
        let project = project_of(v);
        let layer = v["layer"].as_str().unwrap_or("");
        o += &format!(
            "  {:<w$} {:<16} {:<26} {:>10}  {}\n",
            project,
            v["meta"]["target"].as_str().unwrap_or("-"),
            format!("{} {}", short(layer), label_of(layer)),
            bytes(&v["private_bytes"]),
            if v["mounted"] == true {
                "mounted"
            } else {
                "not mounted"
            }
        );
    }
    let mut sorted: Vec<&Value> = layers.iter().filter(|l| l["key"] != "empty").collect();
    sorted.sort_by_key(|l| std::cmp::Reverse(l["unique_bytes"].as_u64().unwrap_or(0)));
    o += &format!("\nLayers ({})\n", sorted.len());
    if !sorted.is_empty() {
        o += &format!(
            "  {:<13} {:<18} {:<22} {:>10} {:>10}  {}\n",
            "KEY", "STEP", "HELD BY", "UNIQUE", "LOGICAL", "LAST USED"
        );
    }
    for l in sorted {
        let reasons: Vec<&str> = l["protected_by"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let views = reasons.iter().filter(|r| r.starts_with("view ")).count();
        let bases = reasons
            .iter()
            .filter(|r| r.starts_with("base of view "))
            .count();
        let mut held: Vec<String> = reasons
            .iter()
            .filter(|r| !r.starts_with("view ") && !r.starts_with("base of view "))
            .map(|r| r.to_string())
            .collect();
        if views > 0 {
            held.insert(0, format!("{views} view(s)"));
        }
        if bases > 0 {
            held.push(format!("base of {bases} view(s)"));
        }
        let held = if held.is_empty() {
            match l["children"].as_u64().unwrap_or(0) {
                0 => "unused".to_string(),
                n => format!("unused ({n} built on it)"),
            }
        } else {
            held.join(", ")
        };
        o += &format!(
            "  {:<13} {:<18} {:<22} {:>10} {:>10}  {}\n",
            short(l["key"].as_str().unwrap_or("")),
            l["label"].as_str().unwrap_or("-"),
            held,
            bytes(&l["unique_bytes"]),
            bytes(&l["logical_bytes"]),
            ago(l["last_used"].as_u64().unwrap_or(0))
        );
    }
    o += "\nUNIQUE = what evicting that layer alone frees. LOGICAL = the tree's size as a project sees it.\n";
    o
}

impl StackCommand {
    pub fn new(container_tool: String) -> Self {
        Self { container_tool }
    }

    fn helper(&self) -> Result<Helper> {
        match Helper::configured(&self.container_tool)? {
            Some(h) => Ok(h),
            None => bail!(
                "stacks are not configured. Set `root:` in {} (see `avocado stack config`)",
                config_path()?.display()
            ),
        }
    }

    pub async fn status(&self, json: bool) -> Result<()> {
        let r = self.helper()?.json(&["report"]).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            print!("{}", render_status(&r));
        }
        Ok(())
    }

    /// Evict layers no project uses: to the given (or configured) limits, else all of them.
    pub async fn gc(
        &self,
        max_size: Option<String>,
        min_free: Option<String>,
        dry_run: bool,
    ) -> Result<()> {
        let h = self.helper()?;
        let mut args: Vec<String> = vec!["gc".into()];
        let limits = max_size.is_some() || min_free.is_some();
        if let Some(m) = max_size.as_deref().map(parse_size).transpose()? {
            args.extend([
                "--high".into(),
                m.to_string(),
                "--low".into(),
                m.to_string(),
            ]);
        }
        if let Some(f) = min_free.as_deref().map(parse_size).transpose()? {
            args.extend(["--min-free".into(), f.to_string()]);
        }
        if !limits {
            // No limit: everything unused goes.
            args.extend(["--high".into(), "0".into(), "--low".into(), "0".into()]);
        }
        if dry_run {
            args.push("--dry-run".into());
        }
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = h.json(&a).await?;
        let evicted = r["evicted_layers"].as_array().cloned().unwrap_or_default();
        for l in &evicted {
            print_info(
                &format!(
                    "{} {} {} ({})",
                    if dry_run { "would evict" } else { "evicted" },
                    short(l["key"].as_str().unwrap_or("")),
                    l["label"].as_str().unwrap_or(""),
                    bytes(&l["freed_bytes"])
                ),
                OutputLevel::Normal,
            );
        }
        print_success(
            &format!(
                "{} {} ({} layers, {} unreferenced files); stacks {} -> {}.",
                if dry_run { "Would free" } else { "Freed" },
                bytes(&r["freed_bytes"]),
                evicted.len(),
                r["removed_objects"],
                bytes(&r["before_bytes"]),
                bytes(&r["after_bytes"])
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// `Some(true/false)` when docker answered whether `volume` exists, `None` when it could
    /// not (daemon down, timeout): a view is never dropped on a non-answer.
    async fn volume_present(&self, volume: &str) -> Option<bool> {
        let o = tokio::process::Command::new(&self.container_tool)
            .args(["volume", "inspect", volume])
            .output()
            .await
            .ok()?;
        if o.status.success() {
            return Some(true);
        }
        let err = String::from_utf8_lossy(&o.stderr).to_lowercase();
        err.contains("no such volume").then_some(false)
    }

    /// Drop views whose project or volume is gone (a project deleted without `avocado clean`,
    /// a volume removed by hand). Their layers then become reclaimable by `gc`.
    pub async fn prune(&self, dry_run: bool) -> Result<()> {
        let h = self.helper()?;
        let r = h.json(&["report"]).await?;
        let mut dropped = 0;
        let mut freed = 0;
        for v in r["views"].as_array().cloned().unwrap_or_default() {
            let name = v["name"].as_str().unwrap_or("").to_string();
            let meta = &v["meta"];
            let volume = meta["volume"]
                .as_str()
                .map(String::from)
                .or_else(|| (name.len() > 40).then(|| name[..40].to_string()))
                .unwrap_or_default();
            let project = meta["project"].as_str().map(PathBuf::from);
            // Any doubt keeps the view: docker that did not answer, a project directory this
            // user cannot look into (another user's home), an unreadable state file.
            let Some(volume_exists) = self.volume_present(&volume).await else {
                continue;
            };
            let project_exists = match project.as_deref().map(Path::try_exists) {
                Some(Ok(e)) => e,
                Some(Err(_)) => continue,
                None => true,
            };
            let state_volume = match project.as_deref().filter(|_| project_exists) {
                Some(p) => match VolumeState::load_from_dir(p) {
                    Ok(s) => s.map(|s| s.volume_name),
                    Err(_) => continue,
                },
                None => None,
            };
            let reason = orphan_reason(
                &name,
                meta,
                volume_exists,
                project_exists,
                state_volume.as_deref(),
            );
            let Some(reason) = reason else { continue };
            print_info(
                &format!(
                    "{} view {name} ({}): {reason}",
                    if dry_run { "would drop" } else { "dropping" },
                    bytes(&v["upper_bytes"])
                ),
                OutputLevel::Normal,
            );
            if !dry_run {
                h.json(&["drop", &name]).await?;
            }
            dropped += 1;
            freed += v["upper_bytes"].as_u64().unwrap_or(0);
        }
        print_success(
            &format!(
                "{} {dropped} orphaned view(s), {} of private files. Run `avocado stack gc` to reclaim the layers only they used.",
                if dry_run { "Would drop" } else { "Dropped" },
                human_bytes(freed)
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Drop one project's views (all targets, or one): its private files and its hold on its
    /// layers. The project's next `avocado install` starts from the shared layers again.
    pub async fn drop_project(
        &self,
        dir: &Path,
        target: Option<&str>,
        dry_run: bool,
    ) -> Result<()> {
        let h = self.helper()?;
        let Some(state) = VolumeState::load_from_dir(dir)? else {
            bail!(
                "{} has no .avocado-state: not an avocado project, or never installed",
                dir.display()
            );
        };
        let prefix = match target {
            Some(t) => crate::utils::stacks::view_name(&state.volume_name, t),
            None => format!("{}-", state.volume_name),
        };
        let r = h.json(&["report"]).await?;
        let mine: Vec<Value> = r["views"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v["name"].as_str().is_some_and(|n| n.starts_with(&prefix)))
            .collect();
        if mine.is_empty() {
            print_info("No stack views for this project.", OutputLevel::Normal);
            return Ok(());
        }
        for v in &mine {
            let name = v["name"].as_str().unwrap_or("");
            print_info(
                &format!(
                    "{} view {name} ({} private)",
                    if dry_run { "would drop" } else { "dropping" },
                    bytes(&v["upper_bytes"])
                ),
                OutputLevel::Normal,
            );
            if !dry_run {
                h.json(&["drop", name]).await?;
            }
        }
        Ok(())
    }

    /// Delete all stacks: every view (projects reinstall from scratch next time) and
    /// every layer. The files are root-owned, so this is the way to remove them.
    pub async fn destroy(&self, yes: bool) -> Result<()> {
        let h = self.helper()?;
        let r = h.json(&["report"]).await?;
        let views = r["views"].as_array().map(|v| v.len()).unwrap_or(0);
        if !yes {
            bail!(
                "this deletes all stacks at {} ({}, {views} project view(s)); every project \
                 reinstalls its SDK next time. Re-run with --yes to proceed",
                r["root"].as_str().unwrap_or(""),
                bytes(&r["total_bytes"])
            );
        }
        h.json(&["destroy"]).await?;
        print_success(
            &format!(
                "Deleted all stacks ({}). Remove `root:` from {} to stop using stacks.",
                bytes(&r["total_bytes"]),
                config_path()?.display()
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    pub async fn config(&self) -> Result<()> {
        let path = config_path()?;
        let s = settings()?;
        println!(
            "Config file: {}{}",
            path.display(),
            if path.exists() { "" } else { " (not present)" }
        );
        for (var, val) in [
            ("AVOCADO_STACKS", std::env::var("AVOCADO_STACKS").ok()),
            (
                "AVOCADO_STACKS_HELPER",
                std::env::var("AVOCADO_STACKS_HELPER").ok(),
            ),
        ] {
            if let Some(v) = val {
                println!("Environment: {var}={v} (overrides the file)");
            }
        }
        println!("\n{}", serde_yaml::to_string(&s)?);
        match &s.root {
            None => println!(
                "Stacks are not configured. To enable them, write {}:\n\n\
                 root: /var/lib/avocado/stacks   # absolute path on the docker host, on a\n\
                 \x20                              # filesystem with room (not tmpfs)\n\
                 # helper_image: avocado-stacks:poc   # not published yet: build it from\n\
                 \x20                                     # crates/avocado-stacks (see its README)\n\
                 # gc:\n\
                 #   max_size: 200G   # evict unused layers to stay under this\n\
                 #   min_free: 20G    # ...or to keep this much free on that filesystem\n",
                path.display()
            ),
            Some(root) => {
                match crate::utils::stacks::active_root(&self.container_tool).await {
                    Some(_) => print_success(
                        &format!("Stacks at {root} work on this host."),
                        OutputLevel::Normal,
                    ),
                    None => {
                        bail!("stacks at {root} cannot run on this host (see the warning above)")
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_shows_projects_layers_and_what_is_reclaimable() {
        let r = json!({
            "root": "/srv/stacks", "free_bytes": 10u64 << 30, "total_bytes": 5u64 << 30,
            "object_bytes": 4u64 << 30, "objects": 100, "layer_meta_bytes": 1u64 << 20,
            "upper_bytes": 1u64 << 30, "reclaimable_bytes": 512u64 << 20,
            "views": [{"name": "avo-x-qemux86-64", "layer": "aaaaaaaaaaaaaaaa", "mounted": true,
                       "upper_bytes": 1u64 << 30,
                       "meta": {"project": "/home/u/proj", "target": "qemux86-64"}}],
            "layers": [
                {"key": "aaaaaaaaaaaaaaaa", "label": "L2 project", "protected_by": ["view avo-x-qemux86-64"],
                 "unique_bytes": 100u64 << 20, "logical_bytes": 3u64 << 30, "children": 0, "last_used": 0},
                {"key": "bbbbbbbbbbbbbbbb", "label": "L1 target base", "protected_by": [],
                 "unique_bytes": 512u64 << 20, "logical_bytes": 2u64 << 30, "children": 1, "last_used": 0},
                {"key": "empty", "label": null, "protected_by": ["base"], "unique_bytes": 0}
            ]
        });
        let t = render_status(&r);
        assert!(t.contains("/srv/stacks (10.0 GiB free)"), "{t}");
        assert!(
            t.contains("reclaimable now (`avocado stack gc`): 512.0 MiB"),
            "{t}"
        );
        assert!(
            t.contains("/home/u/proj") && t.contains("qemux86-64") && t.contains("mounted"),
            "{t}"
        );
        assert!(t.contains("aaaaaaaaaaaa L2 project"), "{t}");
        assert!(t.contains("unused (1 built on it)"), "{t}");
        assert!(t.contains("1 view(s)"), "{t}");
        let base = json!({"root": "/s", "views": [], "layers": [{"key": "cccccccccccccccc", "label": "L0 sdk bootstrap",
            "protected_by": ["base of view a", "base of view b", "pin keep"], "unique_bytes": 1, "children": 2}]});
        assert!(
            render_status(&base).contains("pin keep, base of 2 view(s)"),
            "{}",
            render_status(&base)
        );
        assert!(
            !t.contains("empty"),
            "the empty base layer is not listed: {t}"
        );
        // Layers: largest unique first.
        let layers = t.split("\nLayers (2)\n").nth(1).expect("layers section");
        assert!(
            layers.find("bbbbbbbbbbbb").unwrap() < layers.find("aaaaaaaaaaaa").unwrap(),
            "{t}"
        );
    }
}
