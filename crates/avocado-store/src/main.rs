use anyhow::{bail, Result};
use avocado_store::{oci, write_out, Store, EMPTY_KEY};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

/// Content-addressed layer store (PoC). Runs as root in the privileged helper container.
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
    /// Kernel + overlay data-only lowers + EROFS: mount a real view and tear it down.
    Preflight,
    /// Import a directory tree as layer KEY.
    Import {
        src: PathBuf,
        key: String,
        #[arg(long, default_value = "{}")]
        inputs: String,
        /// Top-level names to leave out.
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Exit 0 when layer KEY exists.
    Has { key: String },
    /// Print a layer record.
    Record { key: String },
    /// Mount a view (new views start on LAYER, default the empty layer).
    Mount {
        view: String,
        #[arg(long)]
        layer: Option<String>,
        #[arg(long)]
        private: Vec<String>,
        /// Mount here instead of views/<name>/merged (recorded; later mounts reuse it).
        #[arg(long)]
        at: Option<PathBuf>,
    },
    /// Mount every recorded, unmounted view whose name starts with PREFIX.
    Remount { prefix: String },
    /// Make ALIAS another name for layer KEY.
    Alias { alias: String, key: String },
    Umount { view: String },
    /// Unmount and delete a view and its upper (releases its lease).
    Drop { view: String },
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
    Verify { key: String },
    /// Usage, layers, views, pins.
    Stats,
    /// What this store offers a route planner.
    Describe,
    /// Evict LRU unpinned, unleased layers when over HIGH bytes, down to LOW.
    Gc {
        #[arg(long)]
        high: u64,
        #[arg(long)]
        low: u64,
        #[arg(long)]
        dry_run: bool,
    },
    Pin { name: String, keys: Vec<String> },
    Unpin { name: String },
    /// Hold the build lock for KEY until stdin closes. Prints "held" once taken.
    Hold { key: String },
    /// Push layer KEY to http://host:port/repo; --known: layers the remote already has.
    Push {
        registry: String,
        key: String,
        #[arg(long)]
        known: Vec<String>,
    },
    /// Pull layer KEY, fetching only missing objects.
    Pull { registry: String, key: String },
    /// Materialize layer KEY as a plain directory.
    Checkout { key: String, dst: PathBuf },
}

fn kernel_ok(release: &str) -> bool {
    let mut it = release.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse::<u32>().ok());
    let (maj, min) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (maj, min) >= (6, 6)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::open(&cli.root)?;
    match cli.cmd {
        Cmd::Preflight => {
            let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim().to_string();
            if !kernel_ok(&rel) {
                bail!("kernel {rel} < 6.6: overlay data-only lowers are missing. Use avocado-vm.");
            }
            let name = format!("preflight-{}", avocado_store::uniq());
            let r = store.mount_view(&name, Some(EMPTY_KEY), &[]);
            let _ = store.drop_view(&name);
            if let Err(e) = r {
                bail!("cannot mount an EROFS + overlay view here ({e:#}). Use avocado-vm.");
            }
            write_out(&json!({"ok": true, "kernel": rel}));
        }
        Cmd::Import { src, key, inputs, exclude } => {
            let r = store.import_dir(&src, &key, serde_json::from_str(&inputs)?, &exclude)?;
            write_out(&serde_json::to_value(r)?);
        }
        Cmd::Has { key } => {
            if !store.has_layer(&key) {
                std::process::exit(1);
            }
        }
        Cmd::Record { key } => write_out(&serde_json::to_value(store.record(&key)?)?),
        Cmd::Mount { view, layer, private, at } => write_out(&serde_json::to_value(store.mount_view_at(
            &view,
            layer.as_deref(),
            &private,
            at.as_deref(),
        )?)?),
        Cmd::Remount { prefix } => write_out(&json!({"mounted": store.remount(&prefix)?})),
        Cmd::Alias { alias, key } => store.alias(&alias, &key)?,
        Cmd::Umount { view } => store.umount_view(&view)?,
        Cmd::Drop { view } => store.drop_view(&view)?,
        Cmd::Use { view, key, private } => write_out(&serde_json::to_value(store.use_layer(&view, &key, &private)?)?),
        Cmd::Commit { view, key, inputs } => {
            write_out(&serde_json::to_value(store.commit_view(&view, &key, serde_json::from_str(&inputs)?)?)?)
        }
        Cmd::Verify { key } => {
            let (ok, got, secs) = store.verify(&key)?;
            write_out(&json!({"key": key, "ok": ok, "tree_digest": got, "secs": secs}));
            if !ok {
                std::process::exit(2);
            }
        }
        Cmd::Stats => write_out(&store.stats()?),
        Cmd::Describe => {
            let s = store.stats()?;
            let st = nix_statvfs(&cli.root);
            write_out(&json!({
                "host": std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),
                "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
                "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
                "mem_bytes": meminfo_total(),
                "store": {"root": cli.root, "free_bytes": st, "object_bytes": s["object_bytes"],
                          "layers": s["layers"].as_array().map(|l| l.iter().map(|l| json!({"key": l["key"], "inputs": l["inputs"]})).collect::<Vec<_>>())},
            }));
        }
        Cmd::Gc { high, low, dry_run } => write_out(&store.gc(high, low, dry_run)?),
        Cmd::Pin { name, keys } => store.pin(&name, &keys)?,
        Cmd::Unpin { name } => store.unpin(&name)?,
        Cmd::Hold { key } => {
            let t = std::time::Instant::now();
            let _l = store.key_lock(&key)?;
            println!("held {:.3}", t.elapsed().as_secs_f64());
            avocado_store::wait_stdin_eof();
        }
        Cmd::Push { registry, key, known } => write_out(&oci::push(&store, &oci::Remote::parse(&registry)?, &key, &known)?),
        Cmd::Pull { registry, key } => write_out(&oci::pull(&store, &oci::Remote::parse(&registry)?, &key)?),
        Cmd::Checkout { key, dst } => store.checkout(&key, &dst)?,
    }
    Ok(())
}

fn nix_statvfs(p: &std::path::Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    s.f_bavail as u64 * s.f_frsize as u64
}

fn meminfo_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok()))
        .map(|k| k * 1024)
        .unwrap_or(0)
}
