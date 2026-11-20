use anyhow::{bail, Result};
use avocado_stacks::{free_bytes, write_out, Stacks, EMPTY_KEY};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

/// Content-addressed layer cache. Runs as root in the privileged helper container.
#[derive(Parser)]
struct Cli {
    /// Stacks root (on the docker host's filesystem).
    #[arg(long, env = "AVOCADO_STACKS_ROOT")]
    root: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Kernel + overlay data-only lowers + file-backed EROFS: mount a real view and tear it down.
    Preflight,
    /// Import a directory tree as layer KEY.
    Import {
        src: PathBuf,
        key: String,
        #[arg(long, default_value = "{}")]
        inputs: String,
        /// Paths to leave out (see `--private` on mount).
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// One round trip for a CLI step: the view's current layer and its ancestors, whether KEY
    /// exists, and the layer KEY resolves to (aliases followed).
    Check {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
        key: String,
    },
    /// Exit 0 when layer KEY exists.
    Has {
        key: String,
    },
    /// Print a layer record.
    Record {
        key: String,
    },
    /// Mount a view (new views start on LAYER, default the empty layer).
    Mount {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
        #[arg(long)]
        layer: Option<String>,
        /// Paths that never enter a layer, relative to the view root; `dir/*` = every
        /// directory directly under `dir`.
        #[arg(long)]
        private: Vec<String>,
        /// Mount here instead of views/<name>/merged (recorded; later mounts reuse it).
        #[arg(long)]
        at: Option<PathBuf>,
        /// Owner metadata to record on the view (JSON).
        #[arg(long)]
        meta: Option<String>,
        /// Take over a plain install at the mount point (see `Stacks::adopt`). Only when no
        /// container uses the volume.
        #[arg(long)]
        adopt: bool,
    },
    /// Mark VIEW's upper as holding work no layer has (a step ran without committing).
    Taint {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
    },
    /// Mount every recorded, unmounted view whose name starts with PREFIX.
    Remount {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        prefix: String,
    },
    /// Make ALIAS another name for layer KEY.
    Alias {
        alias: String,
        key: String,
    },
    Umount {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
    },
    /// Unmount and delete a view and its upper (releases its lease).
    Drop {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
        /// Drop every view whose name starts with VIEW.
        #[arg(long)]
        prefix: bool,
    },
    /// Switch a view to layer KEY, keeping only its private paths.
    Use {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
        key: String,
        #[arg(long)]
        private: Vec<String>,
    },
    /// Harvest a mounted view into layer KEY and remount it there.
    Commit {
        #[arg(value_parser = avocado_stacks::check_view_name)]
        view: String,
        key: String,
        #[arg(long, default_value = "{}")]
        inputs: String,
    },
    /// Re-hash a layer through a mounted view and compare its tree digest.
    Verify {
        key: String,
    },
    /// Where the space goes: totals, per layer (unique bytes, what holds it), per view.
    Report,
    /// Evict unprotected layers (leaves first, least recently used first) when over HIGH
    /// bytes, down to LOW, then drop unreferenced objects.
    Gc {
        #[arg(long, default_value_t = u64::MAX)]
        high: u64,
        #[arg(long, default_value_t = 0)]
        low: u64,
        /// Evict while the stacks filesystem has less than this many bytes free...
        #[arg(long)]
        min_free: Option<u64>,
        /// ...until it has at least this many (default 1.5 × --min-free).
        #[arg(long)]
        target_free: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    Pin {
        name: String,
        keys: Vec<String>,
    },
    Unpin {
        name: String,
    },
    /// Hold the build lock for KEY until stdin closes. Prints "held" once taken.
    Hold {
        key: String,
    },
    /// Unmount every view and delete all stacks.
    Destroy,
}

/// File-backed EROFS (mount(2) on the image file, no loop device) needs 6.12; overlay
/// data-only lowers need 6.5. Below 6.12 a loop-device fallback would be needed.
fn kernel_ok(release: &str) -> bool {
    let mut it = release
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<u32>().ok());
    let (maj, min) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    // Overlay data-only lowers (the objects/ layer under every view) arrived in 6.5. EROFS
    // images mount file-backed from 6.12 and through a loop device below that.
    (maj, min) >= (6, 5)
}

/// `preflight`'s exit code when this host cannot run stacks (its kernel, or a view that will not
/// mount). Any other failure exits 1 and may pass (a full root, a daemon hiccup), so the CLI
/// remembers only this one.
const REFUSED: i32 = 3;

/// Whether a failed view mount is this host's answer rather than a condition that passes: only
/// the kernel rejecting the filesystem or its options (EINVAL, ENODEV, ENOTBLK, or mount(8)
/// saying so). Loop devices running out, EBUSY, ENOMEM, a full or read-only root may all pass
/// on a retry. Only the innermost error counts: the loop fallback's context quotes the
/// file-backed attempt's ENOTBLK, which is expected on kernels below 6.12.
fn mount_refusal(e: &anyhow::Error) -> bool {
    let root = e.root_cause();
    if let Some(io) = root.downcast_ref::<std::io::Error>() {
        return matches!(
            io.raw_os_error(),
            Some(libc::EINVAL | libc::ENODEV | libc::ENOTBLK)
        );
    }
    let msg = root.to_string();
    [
        "unknown filesystem type",
        "wrong fs type",
        "Invalid argument",
        "Block device required",
    ]
    .iter()
    .any(|m| msg.contains(m))
}

fn refuse(msg: String) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(REFUSED)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let stacks = Stacks::open(&cli.root)?;
    match cli.cmd {
        Cmd::Preflight => {
            let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease")?
                .trim()
                .to_string();
            if !kernel_ok(&rel) {
                refuse(format!(
                    "kernel {rel} is older than 6.5 (overlay data-only lowers)"
                ));
            }
            let name = format!("preflight-{}", avocado_stacks::uniq());
            let r = stacks.mount_view(&name, Some(EMPTY_KEY), &[]);
            let _ = stacks.drop_view(&name);
            match r {
                Err(e) if mount_refusal(&e) => {
                    refuse(format!("cannot mount an EROFS + overlay view here: {e:#}"))
                }
                Err(e) => bail!("cannot mount an EROFS + overlay view here: {e:#}"),
                Ok(_) => {}
            }
            write_out(&json!({"ok": true, "kernel": rel, "erofs": stacks.last_mode()}));
        }
        Cmd::Import {
            src,
            key,
            inputs,
            exclude,
        } => {
            let r = stacks.import_dir(&src, &key, serde_json::from_str(&inputs)?, &exclude)?;
            write_out(&serde_json::to_value(r)?);
        }
        Cmd::Check { view, key } => {
            let current = stacks.view(&view)?.map(|v| stacks.canonical(&v.layer));
            let ancestors = current
                .as_deref()
                .map(|c| stacks.ancestors(c))
                .unwrap_or_default();
            let dirty = stacks.view(&view)?.is_some_and(|v| v.dirty);
            write_out(&json!({
                "current": current, "ancestors": ancestors, "dirty": dirty,
                "has": stacks.has_layer(&key), "canonical": stacks.canonical(&key),
            }));
        }
        Cmd::Has { key } => {
            if !stacks.has_layer(&key) {
                std::process::exit(1);
            }
        }
        Cmd::Record { key } => write_out(&serde_json::to_value(stacks.record(&key)?)?),
        Cmd::Mount {
            view,
            layer,
            private,
            at,
            meta,
            adopt,
        } => {
            let _l = stacks.view_lock(&view)?;
            let meta = meta.map(|m| serde_json::from_str(&m)).transpose()?;
            let existed = stacks.view(&view)?.is_some();
            let r = stacks.mount_view_at(
                &view,
                layer.as_deref(),
                &private,
                at.as_deref(),
                meta,
                adopt,
            );
            // A new view refused before anything moved (NEEDS_ADOPT) leaves no record behind,
            // or `remount` would later report a project as stuck on stacks that never was. Any
            // other failure may have moved project outputs into the upper: keep it.
            if let Err(e) = &r {
                if !existed && format!("{e:#}").contains("NEEDS_ADOPT") {
                    let _ = stacks.drop_view(&view);
                }
            }
            write_out(&serde_json::to_value(r?)?)
        }
        Cmd::Taint { view } => {
            let _l = stacks.view_lock(&view)?;
            stacks.taint(&view)?
        }
        Cmd::Remount { prefix } => {
            let (mounted, blocked) = stacks.remount(&prefix)?;
            write_out(&json!({"mounted": mounted, "needs_adopt": blocked}))
        }
        Cmd::Alias { alias, key } => {
            // As a commit writes a layer: gc must not evict `key` between the check and the
            // links, and two aliases of one name must not interleave.
            let _gc = stacks.gc_lock(false)?;
            let _l = stacks.key_lock(&alias)?;
            stacks.alias(&alias, &key)?
        }
        Cmd::Umount { view } => {
            let _l = stacks.view_lock(&view)?;
            stacks.umount_view(&view)?
        }
        Cmd::Drop { view, prefix } => {
            if prefix {
                write_out(&json!({"dropped": stacks.drop_prefix(&view)?}));
            } else {
                let _l = stacks.view_lock(&view)?;
                stacks.drop_view(&view)?;
                write_out(&json!({"dropped": [view]}));
            }
        }
        Cmd::Use { view, key, private } => {
            let _l = stacks.view_lock(&view)?;
            write_out(&serde_json::to_value(
                stacks.use_layer(&view, &key, &private)?,
            )?)
        }
        Cmd::Commit { view, key, inputs } => {
            let _l = stacks.view_lock(&view)?;
            write_out(&serde_json::to_value(stacks.commit_view(
                &view,
                &key,
                serde_json::from_str(&inputs)?,
            )?)?)
        }
        Cmd::Verify { key } => {
            let (ok, got, secs) = stacks.verify(&key)?;
            write_out(&json!({"key": key, "ok": ok, "tree_digest": got, "secs": secs}));
            if !ok {
                std::process::exit(2);
            }
        }
        Cmd::Report => write_out(&stacks.report()?),
        Cmd::Gc {
            high,
            low,
            min_free,
            target_free,
            dry_run,
        } => {
            let (mut high, mut low) = (high, low);
            if let Some(mf) = min_free {
                // Free space is the budget: evicting X bytes frees X, so translate to watermarks.
                let used = stacks.gc(u64::MAX, 0, true)?["before_bytes"]
                    .as_u64()
                    .unwrap_or(0);
                let free = free_bytes(&cli.root);
                let tf = target_free.unwrap_or(mf + mf / 2);
                high = high.min((used + free).saturating_sub(mf));
                low = low.max((used + free).saturating_sub(tf)).min(high);
            }
            write_out(&stacks.gc(high, low, dry_run)?)
        }
        Cmd::Pin { name, keys } => stacks.pin(&name, &keys)?,
        Cmd::Unpin { name } => stacks.unpin(&name)?,
        Cmd::Hold { key } => {
            let t = std::time::Instant::now();
            let _l = stacks.key_lock(&key)?;
            println!("held {:.3}", t.elapsed().as_secs_f64());
            avocado_stacks::wait_stdin_eof();
        }
        Cmd::Destroy => {
            // Exclusive, as gc takes it: every writer (import, mount, use, commit) holds it
            // shared while its staging mount lives in tmp/ and its objects are half written,
            // and no new one starts until the teardown is done.
            let _gc = stacks.gc_lock(true)?;
            stacks.clear_tmp()?;
            for v in stacks.views()? {
                stacks.drop_view(&v.name)?;
            }
            // locks/ and gc.lock stay: a helper waiting on a lock file that is unlinked would
            // take a lock on the old inode while the next one locks a new file.
            for d in ["objects", "layers", "views", "pins", "tmp"] {
                let p = cli.root.join(d);
                if p.exists() {
                    std::fs::remove_dir_all(&p)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn kernel_floor_is_overlay_data_only_lowers() {
        assert!(!super::kernel_ok("5.15.0-125-generic"));
        assert!(!super::kernel_ok("6.1.0-26-amd64"));
        assert!(!super::kernel_ok("6.4.16"));
        assert!(super::kernel_ok("6.5.0"));
        assert!(super::kernel_ok("6.8.0-45-generic"));
        assert!(super::kernel_ok("6.11.9"));
        assert!(super::kernel_ok("6.12.1"));
        assert!(super::kernel_ok("7.1.7-200.fc44.x86_64"));
    }

    /// Only the kernel rejecting the mount is a refusal; a full root, loop devices running out
    /// or a busy mount may pass on a retry.
    #[test]
    fn only_a_kernel_rejection_is_a_refusal() {
        let os = |n| anyhow::Error::from(std::io::Error::from_raw_os_error(n)).context("mount");
        assert!(super::mount_refusal(&os(libc::EINVAL)));
        assert!(super::mount_refusal(&os(libc::ENODEV)));
        for n in [
            libc::ENOSPC,
            libc::EBUSY,
            libc::ENOMEM,
            libc::EMFILE,
            libc::EROFS,
        ] {
            assert!(!super::mount_refusal(&os(n)), "errno {n}");
        }
        let text = anyhow::anyhow!("mkdir: No space left on device (os error 28)");
        assert!(!super::mount_refusal(&text));
        assert!(super::mount_refusal(&anyhow::anyhow!(
            "mount overlay: Invalid argument"
        )));
        assert!(super::mount_refusal(&anyhow::anyhow!(
            "mount: /v: unknown filesystem type 'erofs'."
        )));
        // The loop fallback quotes the file-backed ENOTBLK; its own failure decides.
        let lost = anyhow::anyhow!("mount: /v: failed to setup loop device for /i.")
            .context("mount erofs /i: file-backed: Block device required; through a loop device");
        assert!(!super::mount_refusal(&lost));
    }
}
