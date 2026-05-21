//! `avocado vm update` — fetch the latest VM release and atomic-swap
//! it into the managed install dir.
//!
//! Update policy is driven by the per-artifact `update_policy` field
//! in the remote manifest:
//!
//! - `replace` — always re-downloaded when the sha differs (kernel,
//!   initramfs, rootfs).
//! - `seed_only` — downloaded only on first install. On subsequent
//!   updates we skip it entirely so the user's `var` (Docker volumes,
//!   project caches in `/data`, the persistent `var.btrfs`) survives
//!   across image bumps. Refresh via `avocado vm reset-var` if you
//!   actually want a clean slate.
//!
//! Behaviour with a running VM: query lifecycle, stop it cleanly,
//! perform the swap, restart with the same `start` options. The
//! "was-running" intent is persisted in the staging dir so a
//! crash-during-update preserves the restart on retry.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::ClientBuilder;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::utils::output_format::OutputFormat;
use crate::utils::user_config::UserConfig;
use crate::utils::vm::channel::ChannelPointer;
use crate::utils::vm::manifest::{Manifest, UpdatePolicy};
use crate::utils::vm::staging::StagingDir;
use crate::utils::vm::state::VmPaths;
use crate::utils::vm_update_check::{check_for_vm_update, DEFAULT_BASE};

/// CLI surface — keep this in sync with the clap variant in main.rs.
pub struct UpdateCommand {
    pub channel: Option<String>,
    pub check_only: bool,
    pub assume_yes: bool,
    pub output: OutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
#[allow(dead_code)] // ValueEnum is used by clap
pub enum UpdateMode {
    /// Default — replace `update_policy=replace` artifacts only;
    /// preserve `update_policy=seed_only` (var) entirely.
    Replace,
}

impl UpdateCommand {
    pub async fn execute(self) -> Result<()> {
        let user_cfg = UserConfig::load().context("loading ~/.avocado/config.yaml")?;
        let channel_name = user_cfg.vm_channel(self.channel.as_deref());

        let paths = VmPaths::resolve()?;
        paths.ensure()?;

        // Resolve the installed version, if any. Both the v1 contract
        // (manifest at install_dir/manifest.json with .version) and a
        // freshly-bootstrapped host (no manifest yet) are valid states.
        let installed_manifest_path = paths.install_manifest();
        let installed = if installed_manifest_path.exists() {
            Some(Manifest::load(&installed_manifest_path).context("reading installed manifest")?)
        } else {
            None
        };
        let installed_version = installed
            .as_ref()
            .and_then(|m| m.version.as_deref())
            .map(|s| s.to_string());

        // Channel poll (24h cached). Returns Some only when newer
        // *and* CLI is compatible with min_cli_version.
        let avail = check_for_vm_update(&channel_name, installed_version.as_deref()).await;

        // Print the "what's available" summary so --check is useful
        // even when nothing's new.
        let Some(avail) = avail else {
            return print_up_to_date(installed_version.as_deref(), &channel_name, self.output);
        };

        if self.check_only {
            return print_update_available(
                &avail.pointer,
                installed_version.as_deref(),
                self.output,
            );
        }

        // Decide a target platform — the manifest's `.platform` is the
        // key into the channel pointer. On first install we have no
        // manifest; use a host-arch default.
        let platform = installed
            .as_ref()
            .map(|m| m.platform.clone())
            .unwrap_or_else(default_platform_for_host);
        let platform_entry = avail.pointer.platform(&platform).ok_or_else(|| {
            anyhow::anyhow!(
                "channel '{}' does not advertise platform '{}' (available: {})",
                channel_name,
                platform,
                avail
                    .pointer
                    .platforms
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })?;

        // Confirm with the user (unless --yes).
        if !self.assume_yes {
            confirm(&avail.pointer, installed_version.as_deref())?;
        }

        // HTTP client — `connect_timeout` is bounded so a stalled DNS /
        // TCP handshake fails fast, but the overall request timeout is
        // unset because artifact downloads can run several minutes on
        // slow links (the var.btrfs alone is ~450 MB). A global
        // `.timeout(Duration::from_secs(30))` is what previously caused
        // `Error: operation timed out` mid-download on real-world
        // connections.
        let http = ClientBuilder::new()
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()?;
        let new_manifest: Manifest = serde_json::from_str(
            &http
                .get(&platform_entry.manifest_url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?,
        )
        .context("parsing remote manifest")?;

        // Decide what to download. `seed_only` artifacts are pulled
        // only when the installed dir has no copy of them (first run);
        // on a real update they're skipped entirely. `replace`
        // artifacts are pulled whenever the sha differs.
        let install_dir = paths.install_dir();
        std::fs::create_dir_all(&install_dir)
            .with_context(|| format!("creating install dir {}", install_dir.display()))?;
        let downloads = plan_downloads(&new_manifest, installed.as_ref(), &install_dir);
        if downloads.is_empty() {
            println!("avocado vm update: nothing to download (all artifacts already current).");
            return Ok(());
        }

        // Was the VM running before we tear it down?
        let was_running = is_vm_running().await;

        // Stage.
        let version = new_manifest
            .version
            .clone()
            .unwrap_or_else(|| avail.pointer.version.clone());
        let stage = StagingDir::create(&install_dir, &version)?;
        stage.record_was_running(was_running)?;

        let json_mode = self.output.is_json();

        // Pre-create the MultiProgress + one ProgressBar per artifact
        // so the user sees the whole queue from the start (bars at 0%
        // for not-yet-started files, filling sequentially as each
        // download runs). Matches `avocado connect upload`'s rendering
        // for a consistent look across the CLI.
        let multi = if !json_mode {
            Some(MultiProgress::new())
        } else {
            None
        };
        let bars: Vec<Option<ProgressBar>> = downloads
            .iter()
            .map(|item| {
                multi.as_ref().map(|m| {
                    let pb = m.add(ProgressBar::new(item.size.unwrap_or(0)));
                    pb.set_style(
                        ProgressStyle::with_template(
                            "  {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
                        )
                        .expect("static template parses")
                        .progress_chars("#>-"),
                    );
                    pb.set_message(item.file.clone());
                    pb
                })
            })
            .collect();

        for (idx, item) in downloads.iter().enumerate() {
            let url = format!(
                "{}/{}",
                platform_entry.base_url.trim_end_matches('/'),
                item.file,
            );
            download_artifact(
                &http,
                &url,
                &stage.slot(&item.file),
                item,
                idx + 1,
                downloads.len(),
                json_mode,
                bars[idx].as_ref(),
            )
            .await
            .with_context(|| format!("downloading {}", item.file))?;
            stage
                .verify_sha256(&item.file, &item.sha256)
                .context("staged artifact sha256 mismatch")?;
        }

        // Stop the VM if running. We hold the staging dir open across
        // this so a crash here leaves staging in place for retry.
        if was_running {
            println!("avocado vm update: stopping VM…");
            crate::utils::vm::lifecycle::stop(false).await.ok();
        }

        // Commit all staged files into install_dir.
        for item in &downloads {
            stage.commit(&item.file).with_context(|| {
                format!("committing {} into {}", item.file, install_dir.display())
            })?;
        }
        // Write the new manifest last — it's the marker that says
        // "this install is complete at this version."
        let manifest_path = install_dir.join("manifest.json");
        let manifest_bytes =
            serde_json::to_vec_pretty(&serde_json::from_str::<serde_json::Value>(
                &http
                    .get(&platform_entry.manifest_url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?,
            )?)?;
        std::fs::write(&manifest_path, &manifest_bytes)
            .with_context(|| format!("writing {}", manifest_path.display()))?;

        // Also keep the legacy ~/.avocado/vm/manifest.json (used by
        // existing status / start paths) in sync with the install.
        std::fs::copy(&manifest_path, paths.manifest()).ok();

        // Drop the artifact-dir pointer so `vm start` (no --vm-source)
        // boots from the managed install.
        let _ = std::fs::write(paths.artifact_dir_file(), install_dir.display().to_string());

        stage.cleanup();

        if was_running {
            println!("avocado vm update: restarting VM…");
            // Minimal start opts — None for cpus/memory means lifecycle::start
            // reads `runtime.*` from ~/.avocado/vm/config.yaml (or falls back
            // to DEFAULT_CPUS / DEFAULT_MEMORY_MIB). Other knobs we deliberately
            // don't try to reconstruct from the user's original flags; this is
            // "restart with persisted/default settings", and `vm start --foo=…`
            // is the path when the user wants to re-customise.
            let opts = crate::utils::vm::lifecycle::StartOptions {
                vm_source: install_dir.clone(),
                memory_mib: None,
                cpus: None,
                ssh_port: None,
                cmdline_extra: None,
                workspace: None,
                var_size: None,
                dns_override: None,
            };
            crate::utils::vm::lifecycle::start(opts).await?;
        }

        println!("avocado vm update: updated to {}.", version);
        Ok(())
    }
}

struct PlannedDownload {
    file: String,
    sha256: String,
    size: Option<u64>,
}

/// Decide what to download from the new manifest. Skips `seed_only`
/// artifacts when an installed copy exists.
fn plan_downloads(
    new: &Manifest,
    installed: Option<&Manifest>,
    install_dir: &Path,
) -> Vec<PlannedDownload> {
    let mut out = Vec::new();
    for (role, art) in &new.artifacts {
        match art.update_policy {
            UpdatePolicy::SeedOnly => {
                // Pull only on first install (no existing file in
                // install_dir for this role's filename).
                if !install_dir.join(&art.file).exists() {
                    out.push(PlannedDownload {
                        file: art.file.clone(),
                        sha256: art.sha256.clone(),
                        size: art.size,
                    });
                }
            }
            UpdatePolicy::Replace => {
                // Pull if installed sha differs (or no installed manifest yet).
                let installed_sha = installed
                    .and_then(|m| m.artifact(role))
                    .map(|a| a.sha256.as_str());
                if installed_sha != Some(art.sha256.as_str()) {
                    out.push(PlannedDownload {
                        file: art.file.clone(),
                        sha256: art.sha256.clone(),
                        size: art.size,
                    });
                }
            }
        }
    }
    out
}

/// Best guess at the host's platform string. Matches what the avocado-vm
/// stone generator emits.
fn default_platform_for_host() -> String {
    // Manual mapping until we have a host-introspection helper. arm64
    // covers Apple Silicon + Linux ARM64; x86_64 covers Intel/AMD.
    match std::env::consts::ARCH {
        "aarch64" => "avocado-qemuarm64".to_string(),
        _ => "avocado-qemux86-64".to_string(),
    }
}

async fn is_vm_running() -> bool {
    crate::utils::vm::lifecycle::status()
        .await
        .map(|s| s.running)
        .unwrap_or(false)
}

fn confirm(p: &ChannelPointer, installed: Option<&str>) -> Result<()> {
    let from = installed.unwrap_or("(not installed)");
    println!("avocado vm update: {} -> {}", from, p.version);
    print!("Proceed? [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("aborted by user");
    }
    Ok(())
}

fn print_up_to_date(installed: Option<&str>, channel: &str, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        crate::utils::output_format::emit_json_object(&json!({
            "channel": channel,
            "installed": installed,
            "remote": null,
            "update_available": false,
        }));
    } else {
        match installed {
            Some(v) => println!("avocado vm: {} is current (channel {}).", v, channel),
            None => println!("avocado vm: no installed manifest; nothing to compare against."),
        }
    }
    Ok(())
}

fn print_update_available(
    p: &ChannelPointer,
    installed: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    if output.is_json() {
        crate::utils::output_format::emit_json_object(&json!({
            "channel": p.channel,
            "installed": installed,
            "remote": p.version,
            "released_at": p.released_at,
            "update_available": true,
        }));
    } else {
        println!(
            "avocado vm: {} available (you have {}).",
            p.version,
            installed.unwrap_or("(not installed)"),
        );
        println!("  channel:      {}", p.channel);
        println!("  released_at:  {}", p.released_at);
        println!("  source:       {}", DEFAULT_BASE);
        println!();
        println!("Run `avocado vm update` to apply.");
    }
    Ok(())
}

/// Stream-download one artifact to `dest`.
///
/// - Writes chunks straight to disk via std::fs::File. Doesn't buffer
///   the full body in memory — important for the var.btrfs which is
///   ~450 MB.
/// - In human mode shows an indicatif progress bar with bytes / total /
///   rate / ETA.
/// - In `--output json` mode emits NDJSON progress events throttled to
///   ~10 Hz so the desktop app can drive a progress bar without being
///   flooded.
#[allow(clippy::too_many_arguments)]
async fn download_artifact(
    http: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    item: &PlannedDownload,
    idx: usize,
    total_items: usize,
    json_mode: bool,
    pb: Option<&ProgressBar>,
) -> Result<()> {
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;
    let total_bytes = resp.content_length().or(item.size).unwrap_or(0);

    // Bar was pre-created in the caller with the manifest's `size`. The
    // HTTP response's content_length is the authoritative figure once
    // the request lands — adjust the length if it differs.
    if let Some(pb) = pb {
        if total_bytes > 0 {
            pb.set_length(total_bytes);
        }
    } else if json_mode {
        crate::utils::output_format::emit_json_object(&json!({
            "event": "download_started",
            "file": item.file,
            "size": total_bytes,
            "index": idx,
            "total": total_items,
        }));
    }

    let mut file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    let mut last_emit = Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading body of {url}"))?;
        file.write_all(&chunk)
            .with_context(|| format!("writing to {}", dest.display()))?;
        written += chunk.len() as u64;
        if let Some(pb) = pb {
            pb.set_position(written);
        } else if json_mode && last_emit.elapsed() >= Duration::from_millis(100) {
            crate::utils::output_format::emit_json_object(&json!({
                "event": "download_progress",
                "file": item.file,
                "bytes": written,
                "total": total_bytes,
            }));
            last_emit = Instant::now();
        }
    }
    file.sync_all().ok();
    drop(file);
    if let Some(pb) = pb {
        // Leave the bar visible at 100% with a "(done)" tail — matches
        // `avocado connect upload`'s finish-with-message style.
        pb.finish_with_message(format!("{} (done)", item.file));
    }
    if json_mode {
        crate::utils::output_format::emit_json_object(&json!({
            "event": "download_completed",
            "file": item.file,
            "bytes": written,
            "index": idx,
            "total": total_items,
        }));
    }
    Ok(())
}
