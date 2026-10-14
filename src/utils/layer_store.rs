//! Layer store client (PoC). With `AVOCADO_LAYER_STORE=<store root on the docker host>`, a
//! project's `/opt/_avocado/<target>` is a store view (an EROFS layer + a private upper) instead
//! of a directory in its `avo-<uuid>` volume, and `sdk install` produces shared layers.
//!
//! Every store operation runs in the privileged `avocado-store:poc` helper container on the same
//! docker daemon as the SDK containers, so native hosts and avocado-vm take the same path.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::utils::output::{print_info, OutputLevel};

pub const HELPER_IMAGE: &str = "avocado-store:poc";

/// Top-level names under `/opt/_avocado/<target>` that are project-private: never shared, never
/// in a layer. Extension/runtime outputs (where confext claim tokens and signed images live) are
/// here, so secrets cannot reach a shared layer.
pub const PRIVATE: &[&str] = &[
    "includes",
    "extensions",
    "runtimes",
    "output",
    "kernel",
    "var-staging",
];

pub fn store_root() -> Option<String> {
    std::env::var("AVOCADO_LAYER_STORE")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn view_name(volume: &str, target: &str) -> String {
    format!("{volume}-{target}")
}

/// Volumes whose views this process has already remounted.
static REMOUNTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn helper_args(tool: &str, root: &str) -> Vec<String> {
    let mut a = vec![
        tool.to_string(),
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--network=host".into(),
        "-v".into(),
        format!("{root}:{root}:rshared"),
    ];
    // Views are mounted inside the project's docker volume, so the helper needs the volumes
    // directory with shared propagation for its mounts to reach the daemon's namespace.
    if let Some(v) = volumes_dir(tool) {
        a.extend(["-v".into(), format!("{v}:{v}:rshared")]);
    }
    a.extend([HELPER_IMAGE.into(), "--root".into(), root.to_string()]);
    a
}

fn volumes_dir(tool: &str) -> Option<String> {
    static DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let o = std::process::Command::new(tool)
            .args(["info", "-f", "{{.DockerRootDir}}"])
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| format!("{}/volumes", String::from_utf8_lossy(&o.stdout).trim()))
    })
    .clone()
}

/// Called whenever a project's volume is resolved: remount any of its views that are recorded
/// but not mounted (a reboot drops mounts). Without this, a command would silently see the
/// volume's own (empty) directory where the view belongs.
pub async fn remount_volume_views(tool: &str, volume: &str) -> Result<()> {
    let Some(root) = store_root() else {
        return Ok(());
    };
    if REMOUNTED.lock().unwrap().iter().any(|v| v == volume) {
        return Ok(());
    }
    let mut a = helper_args(tool, &root);
    a.extend(["remount".into(), format!("{volume}-")]);
    let o = Command::new(&a[0])
        .args(&a[1..])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !o.status.success() {
        bail!(
            "layer store: cannot remount views of {volume}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    REMOUNTED.lock().unwrap().push(volume.to_string());
    Ok(())
}

pub fn key_of(inputs: &Value) -> String {
    // serde_json maps are sorted, so this is canonical.
    Sha256::digest(inputs.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub struct LayerStore {
    pub root: String,
    pub view: String,
    volume: String,
    target: String,
    tool: String,
    /// (parent, inputs, key) of each step this session ran or hit, in order.
    steps: Mutex<Vec<(String, Value, String)>>,
}

/// Holds a per-key build lock (a helper container blocked on our stdin); dropping it releases.
pub struct BuildLock {
    _child: Child,
}

impl LayerStore {
    pub fn new(root: String, tool: &str, volume: &str, target: &str) -> Self {
        Self {
            root,
            view: view_name(volume, target),
            volume: volume.to_string(),
            target: target.to_string(),
            tool: tool.to_string(),
            steps: Mutex::new(Vec::new()),
        }
    }

    async fn helper(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut a = helper_args(&self.tool, &self.root);
        a.extend(args.iter().map(|s| s.to_string()));
        Command::new(&a[0])
            .args(&a[1..])
            .stdin(Stdio::null())
            .output()
            .await
            .context("run the layer store helper")
    }

    async fn helper_json(&self, args: &[&str]) -> Result<Value> {
        let o = self.helper(args).await?;
        if !o.status.success() {
            bail!(
                "layer store {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Ok(serde_json::from_slice(&o.stdout).unwrap_or(Value::Null))
    }

    /// Mount (creating if new) this project's view at `<volume>/_data/<target>`.
    pub async fn ensure_view(&self) -> Result<()> {
        let o = Command::new(&self.tool)
            .args(["volume", "inspect", "-f", "{{.Mountpoint}}", &self.volume])
            .output()
            .await?;
        if !o.status.success() {
            bail!("layer store: no volume {}", self.volume);
        }
        let at = format!(
            "{}/{}",
            String::from_utf8_lossy(&o.stdout).trim(),
            self.target
        );
        let mut a = vec!["mount", self.view.as_str(), "--at", at.as_str()];
        for p in PRIVATE {
            a.extend(["--private", p]);
        }
        self.helper_json(&a).await?;
        Ok(())
    }

    /// A key without a snapshot-pinned feed is not shareable: scope it to this project.
    pub fn scope(&self, inputs: &mut Value) {
        if inputs["releasever"].is_null() {
            inputs["unshared"] = self.view.clone().into();
        }
    }

    pub async fn has(&self, key: &str) -> Result<bool> {
        Ok(self.helper(&["has", key]).await?.status.success())
    }

    pub async fn use_layer(&self, key: &str) -> Result<()> {
        self.helper_json(&["use", &self.view, key])
            .await
            .map(|_| ())
    }

    pub async fn commit(&self, key: &str, inputs: &Value) -> Result<Value> {
        self.helper_json(&["commit", &self.view, key, "--inputs", &inputs.to_string()])
            .await
    }

    /// Alias `alias` → `key` so a lookup by either hits (e.g. the key a re-run computes once
    /// the lock file pins what the first run resolved).
    pub async fn alias(&self, alias: &str, key: &str) -> Result<()> {
        self.helper_json(&["alias", alias, key]).await.map(|_| ())
    }

    /// After the install saved the lock file, the next run keys its steps on the pins this run
    /// resolved. Alias those keys to this run's layers so the re-run hits instead of rebuilding
    /// identical layers. `lock` is the target's section of the saved lock file.
    pub async fn alias_for_lock(&self, lock: &Value) -> Result<()> {
        let steps = self.steps.lock().unwrap().clone();
        let mut prev: Option<(String, String)> = None; // (old key, new key) of the previous step
        for (parent, inputs, key) in steps {
            let mut v = inputs.clone();
            // Each step keys on its own sections of the lock only: refresh exactly those.
            if let Some(sections) = v["lock"].as_object().cloned() {
                for k in sections.keys() {
                    v["lock"][k] = lock[k].clone();
                }
            }
            if let (Some(names), Some(arch)) = (
                v["pins_of"].as_array().cloned(),
                v["host_arch"].as_str().map(String::from),
            ) {
                v["pins"] = names
                    .iter()
                    .map(|n| lock["sdk"][&arch][n.as_str().unwrap_or("")].clone())
                    .collect();
            }
            let parent = match &prev {
                Some((old, new)) if *old == parent => new.clone(),
                _ => parent,
            };
            let next = key_of(&json!({"parent": parent, "inputs": v}));
            if next != key {
                self.alias(&next, &key).await?;
            }
            prev = Some((key, next));
        }
        Ok(())
    }

    /// Take the build lock for `key`; waits while another builder holds it.
    pub async fn lock(&self, key: &str) -> Result<BuildLock> {
        let mut a = helper_args(&self.tool, &self.root);
        // The holder keeps the lock until its stdin closes: attach stdin (our pipe, never a TTY).
        let stdin_only = crate::utils::interactivity::ContainerStdio::StdinOnly.flags();
        for (i, f) in stdin_only.iter().enumerate() {
            a.insert(2 + i, f.to_string());
        }
        a.extend(["hold".into(), key.into()]);
        let mut child = Command::new(&a[0])
            .args(&a[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start the build-lock holder")?;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        match lines.next_line().await? {
            Some(l) if l.starts_with("held") => {
                let waited: f64 = l
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                if waited > 1.0 {
                    print_info(
                        &format!(
                            "Layer store: waited {waited:.1}s for another builder of {}.",
                            &key[..12]
                        ),
                        OutputLevel::Normal,
                    );
                }
                Ok(BuildLock { _child: child })
            }
            _ => bail!("layer store: could not take the build lock for {key}"),
        }
    }

    /// Run one step against the view. The step's layer is a function of `parent` (the layer it
    /// runs on) and `inputs`: a hit switches the view to it and skips `step`; a miss puts the view
    /// on `parent`, runs `step` under the key's build lock and commits the result.
    /// Returns the step's layer key.
    pub async fn step<F, Fut>(
        &self,
        label: &str,
        parent: &str,
        inputs: &Value,
        step: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let t = std::time::Instant::now();
        let key = key_of(&json!({"parent": parent, "inputs": inputs}));
        self.steps
            .lock()
            .unwrap()
            .push((parent.to_string(), inputs.clone(), key.clone()));
        let c = self.helper_json(&["check", &self.view, &key]).await?;
        let current = c["current"].as_str();
        if current == Some(key.as_str()) || (c["has"] == true && current == c["canonical"].as_str())
        {
            print_info(
                &format!("Layer store: {label} {} already in the view.", &key[..12]),
                OutputLevel::Normal,
            );
            return Ok(key);
        }
        let _lock = self.lock(&key).await?;
        // `use` re-checks under the store's GC lock: if GC evicted the layer since `has`, it
        // fails and this is a miss like any other.
        if self.has(&key).await? && self.use_layer(&key).await.is_ok() {
            print_info(
                &format!(
                    "Layer store: {label} hit {} ({:.1}s).",
                    &key[..12],
                    t.elapsed().as_secs_f64()
                ),
                OutputLevel::Normal,
            );
            return Ok(key);
        }
        self.use_layer(parent).await?;
        step().await?;
        let rec = self
            .commit(
                &key,
                &json!({"label": label, "parent": parent, "inputs": inputs}),
            )
            .await?;
        print_info(
            &format!(
                "Layer store: {label} committed {} ({} files, {} new objects, {:.1} MB new; commit {:.1}s, step total {:.1}s).",
                &key[..12],
                rec["files"],
                rec["new_objects"],
                rec["new_bytes"].as_f64().unwrap_or(0.0) / 1e6,
                rec["secs"].as_f64().unwrap_or(0.0),
                t.elapsed().as_secs_f64()
            ),
            OutputLevel::Normal,
        );
        Ok(key)
    }
}

/// Run the helper against `root` with extra docker args (e.g. a bind of a bundle to import).
pub async fn run_helper(
    tool: &str,
    root: &str,
    docker_args: &[String],
    args: &[&str],
) -> Result<Value> {
    let mut a = helper_args(tool, root);
    let at = a.iter().position(|x| x == HELPER_IMAGE).unwrap();
    for (i, d) in docker_args.iter().enumerate() {
        a.insert(at + i, d.clone());
    }
    a.extend(args.iter().map(|s| s.to_string()));
    let o = Command::new(&a[0])
        .args(&a[1..])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !o.status.success() {
        bail!(
            "layer store {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&o.stdout).unwrap_or(Value::Null))
}

/// Provision by pull (PoC, `AVOCADO_HUB_STORE=1`): a bundle directory is `bundle.json` +
/// `blobs/<sha256>`, which is already the store's object naming. Import it as layer
/// `bundle-<sha256(bundle.json)>` into the local store and return the key.
pub async fn import_bundle(
    tool: &str,
    root: &str,
    bundle: &std::path::Path,
) -> Result<(String, Value)> {
    let meta = std::fs::read(bundle.join("bundle.json"))?;
    let key = format!(
        "bundle-{}",
        Sha256::digest(&meta)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let bind = format!("{}:/bundle:ro", bundle.display());
    let rec = run_helper(
        tool,
        root,
        &["-v".into(), bind],
        &["import", "/bundle", &key],
    )
    .await?;
    Ok((key, rec))
}

/// Where the hub reads blob `sha256` when the store root sits inside the VM workspace share.
pub fn blob_url_in_share(store_root_rel: &str, sha256: &str) -> String {
    format!(
        "{}/{}/objects/{}/{}",
        crate::utils::vm::share::VM_MOUNT_POINT,
        store_root_rel.trim_matches('/'),
        &sha256[..2],
        &sha256[2..]
    )
}

/// The SDK container image's id: part of every key (same tag, different image = different key).
pub async fn image_id(tool: &str, image: &str) -> Result<String> {
    let o = Command::new(tool)
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
        .await?;
    if !o.status.success() {
        let p = Command::new(tool)
            .args(["pull", "-q", image])
            .output()
            .await?;
        if !p.status.success() {
            bail!("cannot pull {image}");
        }
        return Box::pin(image_id(tool, image)).await;
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Inputs every key carries: what the brief calls the layout (store PoC version + CLI build).
pub fn base_inputs() -> Value {
    json!({
        "layout": "layer-store-poc/1",
        "cli": env!("CARGO_PKG_VERSION"),
        // Unset = the feed was not pinned to a snapshot; the caller must then not share.
        "releasever": std::env::var("AVOCADO_RELEASEVER").ok(),
    })
}
