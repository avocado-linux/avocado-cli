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
    /// One round trip for a CLI step: the view's current layer, whether KEY exists, and the
    /// layer KEY resolves to (aliases followed).
    Check { view: String, key: String },
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
    /// Desktop-facing summary: store bytes, what the projects would take without sharing, and per
    /// project the shared (layer) vs private (upper) bytes.
    Usage,
    /// What this store offers a route planner.
    Describe,
    /// Evict LRU unpinned, unleased layers when over HIGH bytes, down to LOW.
    Gc {
        #[arg(long, default_value_t = u64::MAX)]
        high: u64,
        #[arg(long, default_value_t = 0)]
        low: u64,
        /// Hub/service mode (the agent's AVOCADO_VM_WORK_MIN_FREE_GB): evict when the store's
        /// filesystem has less than this many bytes free...
        #[arg(long)]
        min_free: Option<u64>,
        /// ...until it has at least this many (default 1.5 × --min-free).
        #[arg(long)]
        target_free: Option<u64>,
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
    /// Publish this store's descriptor (+ EXTRA json: rubicons, can_build, store_shared_with…)
    /// to REGISTRY as `backend-NAME`.
    Advertise {
        registry: String,
        name: String,
        #[arg(long, default_value = "{}")]
        extra: String,
    },
    /// Plan where to build and how bytes move. TOPOLOGY json; backends missing from it are
    /// discovered from --registry (`backend-*` tags).
    Plan {
        topology: PathBuf,
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Unmount every view and delete the whole store (bench cleanup).
    Destroy,
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
        Cmd::Check { view, key } => {
            let canon = std::fs::read_link(cli.root.join("layers").join(format!("{key}.json")))
                .ok()
                .and_then(|p| p.to_str().and_then(|s| s.strip_suffix(".json")).map(String::from))
                .unwrap_or_else(|| key.clone());
            write_out(&json!({"current": store.view(&view)?.map(|v| v.layer), "has": store.has_layer(&key), "canonical": canon}));
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
        Cmd::Usage => {
            let s = store.stats()?;
            let store_bytes = s["object_bytes"].as_u64().unwrap_or(0) + s["layer_meta_bytes"].as_u64().unwrap_or(0) + s["upper_bytes"].as_u64().unwrap_or(0);
            let mut projects = Vec::new();
            let mut without = 0u64;
            for v in store.views()? {
                let logical = store.record(&v.layer).map(|r| r.logical_bytes).unwrap_or(0);
                let private = avocado_store::du(&store.view_dir(&v.name).join("upper"));
                without += logical + private;
                projects.push(json!({"view": v.name, "layer": v.layer, "shared_bytes": logical, "private_bytes": private, "mounted_at": v.at}));
            }
            write_out(&json!({
                "store_bytes": store_bytes, "without_sharing_bytes": without,
                "saved_bytes": without.saturating_sub(store_bytes),
                "layers": s["layers"].as_array().map(|l| l.len()).unwrap_or(0), "objects": s["objects"],
                "free_bytes": nix_statvfs(&cli.root), "projects": projects,
            }));
        }
        Cmd::Describe => write_out(&describe(&store, &cli.root)?),
        Cmd::Gc { high, low, min_free, target_free, dry_run } => {
            let (mut high, mut low) = (high, low);
            if let Some(mf) = min_free {
                // Free space is the budget: evicting X bytes frees X, so translate to watermarks.
                let used = store.gc(u64::MAX, 0, true)?["before_bytes"].as_u64().unwrap_or(0);
                let free = nix_statvfs(&cli.root);
                let tf = target_free.unwrap_or(mf + mf / 2);
                high = (used + free).saturating_sub(mf);
                low = (used + free).saturating_sub(tf);
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
        Cmd::Push { registry, key, known } => write_out(&oci::push(&store, &oci::Remote::parse(&registry)?, &key, &known)?),
        Cmd::Pull { registry, key } => write_out(&oci::pull(&store, &oci::Remote::parse(&registry)?, &key)?),
        Cmd::Checkout { key, dst } => store.checkout(&key, &dst)?,
        Cmd::Advertise { registry, name, extra } => {
            let mut d = describe(&store, &cli.root)?;
            d["name"] = json!(name);
            if let (Some(o), Some(e)) = (d.as_object_mut(), serde_json::from_str::<serde_json::Value>(&extra)?.as_object()) {
                o.extend(e.clone());
            }
            oci::put_json(&oci::Remote::parse(&registry)?, &format!("backend-{name}"), "application/vnd.avocado.backend.v1+json", &d)?;
            write_out(&d);
        }
        Cmd::Plan { topology, registry, json: as_json } => {
            let mut topo: serde_json::Value = serde_json::from_slice(&std::fs::read(&topology)?)?;
            if let Some(r) = registry {
                let remote = oci::Remote::parse(&r)?;
                let mut have: Vec<String> = topo["backends"].as_array().map(|b| b.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect()).unwrap_or_default();
                for t in oci::tags(&remote)?.into_iter().filter(|t| t.starts_with("backend-")) {
                    let name = t.trim_start_matches("backend-").to_string();
                    if have.contains(&name) {
                        // Declared in the topology: merge the advertised store contents in.
                        if let Some(d) = oci::get_json(&remote, &t)? {
                            for b in topo["backends"].as_array_mut().into_iter().flatten() {
                                if b["name"] == name.as_str() && b.get("store").is_none() {
                                    b["store"] = d["store"].clone();
                                }
                            }
                        }
                        continue;
                    }
                    if let Some(d) = oci::get_json(&remote, &t)? {
                        topo["backends"].as_array_mut().map(|b| b.push(d));
                        have.push(name);
                    }
                }
            }
            let p = avocado_store::plan::plan(&topo)?;
            if as_json {
                write_out(&p);
            } else {
                print!("{}", avocado_store::plan::render(&p));
            }
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

fn describe(store: &Store, root: &std::path::Path) -> Result<serde_json::Value> {
    let s = store.stats()?;
    Ok(json!({
        "host": std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),
        "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "mem_bytes": meminfo_total(),
        "cli": env!("CARGO_PKG_VERSION"),
        "store": {"root": root, "free_bytes": nix_statvfs(root), "object_bytes": s["object_bytes"],
                  "layers": s["layers"].as_array().map(|l| l.iter().map(|l| json!({"key": l["key"], "inputs": l["inputs"], "logical_bytes": l["logical_bytes"]})).collect::<Vec<_>>())},
    }))
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
