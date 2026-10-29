use anyhow::{bail, Result};
use avocado_store::{free_bytes, write_out, Store, EMPTY_KEY};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

/// Content-addressed layer store. Runs as root in the privileged helper container.
#[derive(Parser)]
struct Cli {
    /// Store root (on the docker host's filesystem).
    #[arg(long, env = "AVOCADO_STORE_ROOT")]
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
    },
    /// Mount every recorded, unmounted view whose name starts with PREFIX.
    Remount {
        prefix: String,
    },
    /// Make ALIAS another name for layer KEY.
    Alias {
        alias: String,
        key: String,
    },
    Umount {
        view: String,
    },
    /// Unmount and delete a view and its upper (releases its lease).
    Drop {
        view: String,
        /// Drop every view whose name starts with VIEW.
        #[arg(long)]
        prefix: bool,
    },
    /// Switch a view to layer KEY, keeping only its private paths.
    Use {
        view: String,
        key: String,
        #[arg(long)]
        private: Vec<String>,
    },
    /// Harvest a mounted view into layer KEY and remount it there.
    Commit {
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
        /// Evict while the store's filesystem has less than this many bytes free...
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
    /// Unmount every view and delete the whole store.
    Destroy,
}

/// File-backed EROFS (mount(2) on the image file, no loop device) needs 6.12; overlay
/// data-only lowers need 6.5. Below 6.12 a loop-device fallback would be needed.
fn kernel_ok(release: &str) -> bool {
    let mut it = release
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<u32>().ok());
    let (maj, min) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (maj, min) >= (6, 12)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::open(&cli.root)?;
    match cli.cmd {
        Cmd::Preflight => {
            let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease")?
                .trim()
                .to_string();
            if !kernel_ok(&rel) {
                bail!("kernel {rel} is older than 6.12 (file-backed EROFS)");
            }
            let name = format!("preflight-{}", avocado_store::uniq());
            let r = store.mount_view(&name, Some(EMPTY_KEY), &[]);
            let _ = store.drop_view(&name);
            if let Err(e) = r {
                bail!("cannot mount an EROFS + overlay view here: {e:#}");
            }
            write_out(&json!({"ok": true, "kernel": rel}));
        }
        Cmd::Import {
            src,
            key,
            inputs,
            exclude,
        } => {
            let r = store.import_dir(&src, &key, serde_json::from_str(&inputs)?, &exclude)?;
            write_out(&serde_json::to_value(r)?);
        }
        Cmd::Check { view, key } => {
            let current = store.view(&view)?.map(|v| store.canonical(&v.layer));
            let ancestors = current
                .as_deref()
                .map(|c| store.ancestors(c))
                .unwrap_or_default();
            write_out(&json!({
                "current": current, "ancestors": ancestors,
                "has": store.has_layer(&key), "canonical": store.canonical(&key),
            }));
        }
        Cmd::Has { key } => {
            if !store.has_layer(&key) {
                std::process::exit(1);
            }
        }
        Cmd::Record { key } => write_out(&serde_json::to_value(store.record(&key)?)?),
        Cmd::Mount {
            view,
            layer,
            private,
            at,
            meta,
        } => {
            let _l = store.view_lock(&view)?;
            let meta = meta.map(|m| serde_json::from_str(&m)).transpose()?;
            write_out(&serde_json::to_value(store.mount_view_at(
                &view,
                layer.as_deref(),
                &private,
                at.as_deref(),
                meta,
            )?)?)
        }
        Cmd::Remount { prefix } => write_out(&json!({"mounted": store.remount(&prefix)?})),
        Cmd::Alias { alias, key } => store.alias(&alias, &key)?,
        Cmd::Umount { view } => {
            let _l = store.view_lock(&view)?;
            store.umount_view(&view)?
        }
        Cmd::Drop { view, prefix } => {
            if prefix {
                write_out(&json!({"dropped": store.drop_prefix(&view)?}));
            } else {
                let _l = store.view_lock(&view)?;
                store.drop_view(&view)?;
                write_out(&json!({"dropped": [view]}));
            }
        }
        Cmd::Use { view, key, private } => {
            let _l = store.view_lock(&view)?;
            write_out(&serde_json::to_value(
                store.use_layer(&view, &key, &private)?,
            )?)
        }
        Cmd::Commit { view, key, inputs } => {
            let _l = store.view_lock(&view)?;
            write_out(&serde_json::to_value(store.commit_view(
                &view,
                &key,
                serde_json::from_str(&inputs)?,
            )?)?)
        }
        Cmd::Verify { key } => {
            let (ok, got, secs) = store.verify(&key)?;
            write_out(&json!({"key": key, "ok": ok, "tree_digest": got, "secs": secs}));
            if !ok {
                std::process::exit(2);
            }
        }
        Cmd::Report => write_out(&store.report()?),
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
                let used = store.gc(u64::MAX, 0, true)?["before_bytes"]
                    .as_u64()
                    .unwrap_or(0);
                let free = free_bytes(&cli.root);
                let tf = target_free.unwrap_or(mf + mf / 2);
                high = high.min((used + free).saturating_sub(mf));
                low = low.max((used + free).saturating_sub(tf)).min(high);
            }
            write_out(&store.gc(high, low, dry_run)?)
        }
        Cmd::Pin { name, keys } => store.pin(&name, &keys)?,
        Cmd::Unpin { name } => store.unpin(&name)?,
        Cmd::Hold { key } => {
            let t = std::time::Instant::now();
            let _l = store.key_lock(&key)?;
            println!("held {:.3}", t.elapsed().as_secs_f64());
            avocado_store::wait_stdin_eof();
        }
        Cmd::Destroy => {
            for v in store.views()? {
                store.drop_view(&v.name)?;
            }
            for d in ["objects", "layers", "views", "locks", "pins", "tmp"] {
                let p = cli.root.join(d);
                if p.exists() {
                    std::fs::remove_dir_all(&p)?;
                }
            }
            let _ = std::fs::remove_file(cli.root.join("gc.lock"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn kernel_floor_is_file_backed_erofs() {
        assert!(!super::kernel_ok("6.8.0-45-generic"));
        assert!(!super::kernel_ok("6.11.9"));
        assert!(super::kernel_ok("6.12.1"));
        assert!(super::kernel_ok("7.1.7-200.fc44.x86_64"));
    }
}
