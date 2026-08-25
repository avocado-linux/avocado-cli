//! `avocado vm update` — fetch the latest VM release and atomic-swap
//! it into the managed install dir.
//!
//! Update policy is driven by the per-artifact `update_policy` field
//! in the remote manifest:
//!
//! - `replace` — always re-downloaded when the sha differs (kernel,
//!   initramfs, rootfs).
//! - `seed_only` — the `var` image. Fetched on first install and, on a
//!   version bump, re-fetched and re-seeded. See below.
//!
//! ## Why a version bump needs more than the boot artifacts
//!
//! `var` holds more than user state: the VM's own system extensions
//! live in `/var/lib/avocado` and are merged onto `/usr` and `/etc` at
//! boot. Replacing only kernel/initramfs/rootfs therefore boots a new
//! kernel against the *old* userspace, and nothing catches it — each
//! extension's `extension-release` carries `ID=_any`, systemd-sysext's
//! "matches any OS" wildcard, so mismatched extensions merge silently.
//!
//! It cannot be fixed by carrying the new seed's `var` wholesale
//! either: the same filesystem holds the user's Docker volumes and
//! installed SDKs (`$AVOCADO_PREFIX` is a Docker named volume inside
//! the VM), which are expensive to rebuild and entirely theirs.
//!
//! So the two lifetimes are separated at the granularity that already
//! distinguishes them. The avocado-owned half — `runtimes/`, `images/`
//! and the `active` pointer — is content-addressed and versioned, so
//! the guest can install the new runtime *alongside* the old one out of
//! the new seed and switch `active` only once it is fully staged.
//! `vm update` records the seed sha here; `vm start` attaches that seed
//! read-only; the guest does the work in early boot, before extensions
//! merge. Nothing the user owns is touched, and every failure leaves
//! the VM on its previous runtime rather than on none.
//!
//! The btrfs work has to happen guest-side regardless: this VM exists
//! because the host is macOS or Windows, neither of which can mount a
//! btrfs image.
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
use crate::utils::vm_update_check::{check_for_vm_update, VmUpdateStatus, DEFAULT_BASE};

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

        // Channel poll (24h cached).
        let avail = match check_for_vm_update(&channel_name, installed_version.as_deref()).await {
            VmUpdateStatus::Available(avail) => avail,
            // A newer release exists that this CLI must not install.
            // Fail rather than print-and-succeed: host applications map
            // a non-zero `--check` exit to an error status carrying our
            // stderr, so the actionable "run `avocado upgrade` first"
            // message reaches their UI without them needing to
            // understand a new field. In JSON mode a structured line
            // goes out first so stdout parsers can tell "blocked on CLI
            // version" from "check broke".
            VmUpdateStatus::CliTooOld { message, remote } => {
                if self.output.is_json() {
                    crate::utils::output_format::emit_json_object(&json!({
                        "channel": channel_name,
                        "installed": installed_version,
                        "remote": remote,
                        "update_available": true,
                        "cli_too_old": true,
                        "message": message,
                    }));
                }
                bail!(message)
            }
            // Print the "what's available" summary so --check is useful
            // even when nothing's new.
            VmUpdateStatus::NoUpdate => {
                return print_up_to_date(installed_version.as_deref(), &channel_name, self.output)
            }
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
        // Fetched once and kept: we parse this text to plan the download
        // and later write the same bytes out as the installed manifest.
        // Two separate GETs could disagree if the release were
        // re-published mid-run, leaving a manifest that doesn't describe
        // the artifacts we actually committed.
        let manifest_raw = http
            .get(&platform_entry.manifest_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let new_manifest: Manifest =
            serde_json::from_str(&manifest_raw).context("parsing remote manifest")?;

        // Decide what to download. `replace` artifacts are pulled
        // whenever the sha differs; the `seed_only` var image is pulled
        // on first install, on any sha change, and when the seed file
        // itself is missing (see the module docs).
        let install_dir = paths.install_dir();
        std::fs::create_dir_all(&install_dir)
            .with_context(|| format!("creating install dir {}", install_dir.display()))?;
        let downloads = plan_downloads(&new_manifest, installed.as_ref(), &install_dir);
        if downloads.is_empty() {
            println!("avocado vm update: nothing to download (all artifacts already current).");
            return Ok(());
        }

        let sync_var_seed =
            should_sync_var_seed(installed.as_ref(), &new_manifest, paths.var_disk().exists());

        // Confirm with the user (unless --yes). This no longer needs a
        // destructive-consent path: the update replaces boot artifacts
        // and schedules a state sync, and keeps the VM's Docker volumes,
        // installed SDKs and /data either way.
        if !self.assume_yes {
            // The prompt writes prose into the NDJSON stream and blocks
            // on stdin; machine mode never prompts.
            if self.output.is_json() {
                bail!("refusing to prompt in --output json mode; re-run with --yes");
            }
            confirm(&avail.pointer, installed_version.as_deref(), sync_var_seed)?;
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
        // The live var disk is never touched here — it holds the user's
        // Docker volumes and installed SDKs, which is the whole reason
        // this is a sync and not a reset.
        if sync_var_seed {
            // Record the seed the guest owes a state sync from, rather
            // than acting on the live disk here. `vm start` attaches
            // that seed read-only and the guest lifts the new runtime
            // out of it in early boot, before extensions merge — the
            // only place the work can happen, since the host is macOS
            // or Windows and has no btrfs.
            //
            // Nothing is destroyed: the new runtime is installed
            // alongside the old and `active` only moves once it is
            // fully staged, so every failure leaves the VM on its
            // previous runtime rather than on none.
            let new_sha = new_manifest
                .artifact("var")
                .map(|a| a.sha256.clone())
                .expect("should_sync_var_seed only fires on a var artifact");
            let mut cfg = crate::utils::vm::config::VmConfig::load(&paths)
                .context("loading config.yaml to record the pending var seed")?;
            cfg.runtime
                .get_or_insert_with(Default::default)
                .pending_var_seed_sha = Some(new_sha);
            cfg.save(&paths)
                .context("recording runtime.pending_var_seed_sha in config.yaml")?;

            if json_mode {
                // Distinct from the old `var_reset`: nothing is lost, so
                // host applications keep their cached install state.
                // Extensions change on the next boot, not on this call.
                crate::utils::output_format::emit_json_object(&json!({
                    "event": "var_seed_sync_pending",
                    "reason": "vm_image_updated",
                }));
            } else {
                println!(
                    "avocado vm update: the VM will pick up the new Avocado \
                     extensions on its next start; installed SDKs, Docker \
                     volumes and /data are kept."
                );
            }
        }

        // Write the new manifest last — it's the marker that says
        // "this install is complete at this version."
        let manifest_path = install_dir.join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(
            &serde_json::from_str::<serde_json::Value>(&manifest_raw)
                .context("re-parsing remote manifest for the install dir")?,
        )?;
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

/// Decide what to download from the new manifest.
fn plan_downloads(
    new: &Manifest,
    installed: Option<&Manifest>,
    install_dir: &Path,
) -> Vec<PlannedDownload> {
    let mut out = Vec::new();
    for (role, art) in &new.artifacts {
        // Sha comparison against the installed manifest, for both
        // policies. `None` (no installed manifest — first install)
        // never matches, so everything is fetched.
        let installed_sha = installed
            .and_then(|m| m.artifact(role))
            .map(|a| a.sha256.as_str());
        let sha_differs = installed_sha != Some(art.sha256.as_str());
        let wanted = match art.update_policy {
            // On a first install, the file's presence is the only signal
            // we have — there's no installed manifest to compare against,
            // and a var image already in place is one we just fetched.
            // On an update the sha decides, so an unchanged var image
            // costs neither a ~450 MB download nor the user's state.
            // A missing seed is re-fetched regardless of sha: without a
            // source `seed_var_disk` no-ops and the VM boots with no
            // /var, unrepaired by any later update.
            UpdatePolicy::SeedOnly => match installed {
                Some(_) => sha_differs || !install_dir.join(&art.file).exists(),
                None => !install_dir.join(&art.file).exists(),
            },
            UpdatePolicy::Replace => sha_differs,
        };
        if wanted {
            out.push(PlannedDownload {
                file: art.file.clone(),
                sha256: art.sha256.clone(),
                size: art.size,
            });
        }
    }
    out
}

/// Whether the guest owes a state sync from the new seed:
///
/// - an installed manifest exists (a first install has nothing to carry
///   forward),
/// - the live disk exists — a never-started VM gets the new seed copied
///   wholesale by `seed_var_disk`, so it is already current. This case
///   also *must* stay false: the copy is byte-identical to the seed, so
///   attaching that seed alongside it would put two devices with one
///   btrfs fsid in front of the kernel.
/// - a `seed_only` artifact's sha changed. Keyed on the manifests, not
///   the planned downloads: `plan_downloads` also re-fetches a seed
///   that merely went missing (same sha), which needs no sync.
fn should_sync_var_seed(
    installed: Option<&Manifest>,
    new: &Manifest,
    var_disk_exists: bool,
) -> bool {
    let Some(installed) = installed else {
        return false;
    };
    if !var_disk_exists {
        return false;
    }
    new.artifacts.iter().any(|(role, art)| {
        art.update_policy == UpdatePolicy::SeedOnly
            && installed.artifact(role).map(|a| a.sha256.as_str()) != Some(art.sha256.as_str())
    })
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

fn confirm(p: &ChannelPointer, installed: Option<&str>, sync_var_seed: bool) -> Result<()> {
    let from = installed.unwrap_or("(not installed)");
    println!("avocado vm update: {} -> {}", from, p.version);
    if sync_var_seed {
        // Informational, not a consent gate: the sync installs the new
        // runtime alongside the old and only then switches, so nothing
        // the user owns is at risk. Worth saying anyway, because the
        // extensions change on the next boot rather than on this call.
        println!();
        println!("This release ships new system extensions. They are applied on the");
        println!("VM's next start; installed SDKs, Docker volumes and /data are kept.");
        println!();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with one `replace` artifact (rootfs) and the
    /// `seed_only` var image, at caller-chosen shas.
    fn manifest_json(rootfs_sha: &str, var_sha: &str) -> String {
        format!(
            r#"{{
              "format": "avocado-direct",
              "format_version": 1,
              "version": "0.4.0",
              "platform": "avocado-qemuarm64",
              "architecture": "arm64",
              "artifacts": {{
                "rootfs": {{ "file": "rootfs.erofs-lz4", "sha256": "{rootfs_sha}",
                             "type": "erofs-lz4", "update_policy": "replace" }},
                "var":    {{ "file": "var.btrfs", "sha256": "{var_sha}",
                             "type": "btrfs", "update_policy": "seed_only" }}
              }},
              "cmdline_default": ""
            }}"#
        )
    }

    fn manifest(rootfs_sha: &str, var_sha: &str) -> Manifest {
        serde_json::from_str(&manifest_json(rootfs_sha, var_sha)).expect("fixture parses")
    }

    fn planned(downloads: &[PlannedDownload], file: &str) -> bool {
        downloads.iter().any(|d| d.file == file)
    }

    #[test]
    fn first_install_fetches_every_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_downloads(&manifest("aa", "bb"), None, dir.path());
        assert!(planned(&plan, "rootfs.erofs-lz4"));
        assert!(planned(&plan, "var.btrfs"));
    }

    #[test]
    fn first_install_skips_a_var_image_already_on_disk() {
        // Resuming a first install that already pulled the ~450 MB var
        // image: no installed manifest to compare shas against, so
        // presence is the only signal.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("var.btrfs"), b"seed").unwrap();
        let plan = plan_downloads(&manifest("aa", "bb"), None, dir.path());
        assert!(planned(&plan, "rootfs.erofs-lz4"));
        assert!(!planned(&plan, "var.btrfs"));
    }

    #[test]
    fn update_with_a_new_var_image_plans_it() {
        let dir = tempfile::tempdir().unwrap();
        // The var image is on disk from the previous install — under the
        // old rule that alone was enough to skip it, which is exactly how
        // a stale /var survived a version bump.
        std::fs::write(dir.path().join("var.btrfs"), b"old seed").unwrap();
        let installed = manifest("aa", "bb");
        let plan = plan_downloads(&manifest("aa2", "bb2"), Some(&installed), dir.path());
        assert!(planned(&plan, "var.btrfs"));
    }

    #[test]
    fn update_leaves_an_unchanged_var_image_alone() {
        // A release that only bumps the boot artifacts must not cost the
        // user their /var — nothing in it is stale.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("var.btrfs"), b"seed").unwrap();
        let installed = manifest("aa", "bb");
        let plan = plan_downloads(&manifest("aa2", "bb"), Some(&installed), dir.path());
        assert!(planned(&plan, "rootfs.erofs-lz4"));
        assert!(!planned(&plan, "var.btrfs"));
    }

    #[test]
    fn update_refetches_a_missing_seed_even_with_a_matching_sha() {
        // The seed vanished from the install dir (a crash, a manual
        // delete) but the sha is unchanged. Without a presence check the
        // plan is empty, `seed_var_disk` no-ops without a source and the
        // VM boots with no /var — and since the sha never changes on its
        // own, no later update would repair it either.
        let dir = tempfile::tempdir().unwrap();
        let installed = manifest("aa", "bb");
        let plan = plan_downloads(&manifest("aa", "bb"), Some(&installed), dir.path());
        assert!(planned(&plan, "var.btrfs"));
        assert!(!planned(&plan, "rootfs.erofs-lz4"));
    }

    #[test]
    fn update_with_nothing_changed_plans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("var.btrfs"), b"seed").unwrap();
        let installed = manifest("aa", "bb");
        let plan = plan_downloads(&manifest("aa", "bb"), Some(&installed), dir.path());
        assert!(plan.is_empty());
    }

    // ── should_sync_var_seed — when the guest owes a state sync ──

    #[test]
    fn sync_requires_a_var_sha_change() {
        let old = manifest("aa", "bb");
        assert!(should_sync_var_seed(
            Some(&old),
            &manifest("aa2", "bb2"),
            true
        ));
        // Boot-artifact-only release: var untouched.
        assert!(!should_sync_var_seed(
            Some(&old),
            &manifest("aa2", "bb"),
            true
        ));
    }

    #[test]
    fn sync_never_fires_on_first_install_or_without_a_live_disk() {
        let old = manifest("aa", "bb");
        // No installed manifest → nothing to carry forward.
        assert!(!should_sync_var_seed(None, &manifest("aa", "bb"), true));
        // VM never started → `seed_var_disk` copies the new seed
        // wholesale, so it is already current. Syncing anyway would
        // attach a seed byte-identical to the live disk, colliding on
        // btrfs fsid.
        assert!(!should_sync_var_seed(
            Some(&old),
            &manifest("aa2", "bb2"),
            false
        ));
    }

    #[test]
    fn repairing_a_missing_seed_does_not_schedule_a_sync() {
        // `plan_downloads` re-fetches a merely-missing seed at the same
        // sha. That repair must not be read as "the release changed
        // var" — the seed it would attach is the one the live disk was
        // copied from, i.e. the fsid-collision case.
        let old = manifest("aa", "bb");
        assert!(!should_sync_var_seed(
            Some(&old),
            &manifest("aa", "bb"),
            true
        ));
    }
}
