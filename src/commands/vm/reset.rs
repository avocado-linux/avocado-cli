//! `avocado vm reset` — wipe the persistent `var.btrfs` and re-seed it
//! from the installed var artifact.
//!
//! This is the explicit "I want a clean slate" lever. Use cases:
//!
//! - The `var` partition is corrupted and the VM won't boot past
//!   systemd's `local-fs.target`.
//! - You want to test a provision flow from scratch (no leftover
//!   Docker volumes, no cached extension data, no resident project
//!   work in `/data`).
//! - `var` has accumulated cruft you'd rather drop than triage.
//!
//! The fresh `var` comes from the **installed** seed artifact (the
//! `var` entry in the manifest at the recorded artifact dir). That
//! means after `avocado vm update`, this command resets to the
//! freshly-updated seed; on a dev host using `--vm-source`, it
//! resets to the dev `var.btrfs` in that source dir. The VM image
//! version isn't changed — only state. Use `avocado vm update` to
//! pick up a new image, then `avocado vm reset` if you also want to
//! drop accumulated state.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::utils::vm::lifecycle;
use crate::utils::vm::manifest::Manifest;
use crate::utils::vm::state::VmPaths;

pub struct ResetCommand {
    pub assume_yes: bool,
}

impl ResetCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        paths.ensure()?;

        // Find the artifact dir the last `vm start` / `vm rebuild` /
        // `vm update` recorded. The seed lives there per
        // role_link=var in the manifest. Without a recorded dir we
        // can't know which seed to use — refuse cleanly.
        let artifact_dir = read_artifact_dir(&paths).ok_or_else(|| {
            anyhow::anyhow!(
                "no recorded artifact dir at {}. Run `avocado vm start --vm-source <dir>` or `avocado vm update` first.",
                paths.artifact_dir_file().display(),
            )
        })?;

        let manifest_path = artifact_dir.join("manifest.json");
        let manifest = Manifest::load(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let var_src = manifest
            .artifact_path("var", &artifact_dir)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "manifest at {} has no `var` artifact — nothing to reset to.",
                    manifest_path.display(),
                )
            })?;
        if !var_src.exists() {
            bail!(
                "seed var artifact {} is missing — re-run `avocado vm update` to fetch it.",
                var_src.display(),
            );
        }

        // Was the VM running before? Used to decide auto-restart.
        let was_running = lifecycle::status()
            .await
            .map(|s| s.running)
            .unwrap_or(false);

        if !self.assume_yes {
            confirm(was_running, &paths.var_disk(), &var_src)?;
        }

        if was_running {
            println!("avocado vm reset: stopping VM…");
            // Graceful stop. Errors here are best-effort — if the VM
            // had already died, the var-file copy can still proceed.
            let _ = lifecycle::stop(false).await;
        }

        // Remove the existing var first so a mid-copy interruption
        // doesn't leave a half-written file mistaken for valid state.
        let dest = paths.var_disk();
        if dest.exists() {
            std::fs::remove_file(&dest).with_context(|| format!("removing {}", dest.display()))?;
        }
        std::fs::copy(&var_src, &dest).with_context(|| {
            format!("copying seed {} -> {}", var_src.display(), dest.display(),)
        })?;

        println!(
            "avocado vm reset: var.btrfs replaced with seed ({} bytes).",
            std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
        );

        if was_running {
            println!("avocado vm reset: restarting VM…");
            let opts = lifecycle::StartOptions {
                vm_source: artifact_dir,
                memory_mib: 4096,
                cpus: 4,
                ssh_port: None,
                cmdline_extra: None,
                workspace: None,
                var_size: None,
            };
            lifecycle::start(opts).await?;
        }

        Ok(())
    }
}

fn read_artifact_dir(paths: &VmPaths) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(paths.artifact_dir_file()).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn confirm(was_running: bool, dest: &std::path::Path, src: &std::path::Path) -> Result<()> {
    println!("avocado vm reset:");
    println!("  will delete  {}", dest.display());
    println!("  will seed from  {}", src.display());
    if was_running {
        println!("  VM is currently running and will be stopped + restarted.");
    }
    println!();
    println!("This wipes /var inside the VM (Docker volumes, container caches,");
    println!("project work in /data, /etc/machine-id, etc.). Cannot be undone.");
    println!();
    print!("Type 'reset' to confirm: ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    if line.trim() != "reset" {
        bail!("aborted by user");
    }
    Ok(())
}
