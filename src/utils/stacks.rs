//! Stacks client. When stacks are configured (`avocado stack config`), a project's
//! `/opt/_avocado/<target>` is a stack view (an EROFS layer + a private upper) mounted inside
//! its `avo-<uuid>` volume, and `sdk install` produces layers other projects with the same
//! inputs share.
//!
//! Every stacks operation runs in the privileged helper container (`crates/avocado-stacks`) on
//! the same docker daemon as the SDK containers, so native hosts and avocado-vm take the same
//! path. A host that cannot run it (kernel older than 6.12, no privileged containers) falls
//! back to plain volumes with a warning.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::utils::output::{print_info, print_warning, OutputLevel};

pub const DEFAULT_HELPER_IMAGE: &str = "avocado-stacks:poc";

/// Paths under `/opt/_avocado/<target>` that are project-private: never in a layer, and kept
/// across layer switches. Extension/runtime outputs (where confext claim tokens and signed
/// images live) are here, so secrets cannot reach a shared layer. The stamps here describe
/// this project's builds, not the layer: the SDK and sysroot install stamps stay in layers,
/// because they describe the tree they sit in. `dir/*` = every directory directly under
/// `dir` (the per-runtime image stamps).
pub const PRIVATE: &[&str] = &[
    "includes",
    "extensions",
    "runtimes",
    "output",
    "kernel",
    "var-staging",
    ".stamps/ext",
    ".stamps/runtime",
    ".stamps/rootfs/*",
    ".stamps/initramfs/*",
];

/// Per-machine stacks settings: `<avocado config dir>/stacks.yaml`, then the
/// environment (`AVOCADO_STACKS`, `AVOCADO_STACKS_HELPER`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Stacks directory on the docker host (absolute). Unset = stacks off.
    #[serde(default)]
    pub root: Option<String>,
    /// Helper image (pin by digest in production).
    #[serde(default)]
    pub helper_image: Option<String>,
    #[serde(default)]
    pub gc: GcSettings,
}

/// Automatic cleanup after each `sdk install`. Sizes take K/M/G/T suffixes (powers of 1024).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcSettings {
    /// Keep stacks at or under this size: evict unused layers (leaves first, least
    /// recently used first) down to 80 % of it.
    #[serde(default)]
    pub max_size: Option<String>,
    /// Keep at least this much free on the stacks filesystem.
    #[serde(default)]
    pub min_free: Option<String>,
}

impl Settings {
    pub fn helper_image(&self) -> &str {
        self.helper_image.as_deref().unwrap_or(DEFAULT_HELPER_IMAGE)
    }

    /// `(--high, --low, --min-free)` for the helper's `gc`, or `None` when no limit is set.
    pub fn gc_args(&self) -> Result<Option<Vec<String>>> {
        let max = self.gc.max_size.as_deref().map(parse_size).transpose()?;
        let min_free = self.gc.min_free.as_deref().map(parse_size).transpose()?;
        if max.is_none() && min_free.is_none() {
            return Ok(None);
        }
        let mut a = Vec::new();
        if let Some(m) = max {
            a.extend([
                "--high".to_string(),
                m.to_string(),
                "--low".into(),
                (m / 10 * 8).to_string(),
            ]);
        }
        if let Some(f) = min_free {
            a.extend(["--min-free".to_string(), f.to_string()]);
        }
        Ok(Some(a))
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(crate::utils::signing_keys::get_avocado_config_dir()?.join("stacks.yaml"))
}

/// Settings from a config file's text and the two environment variables, validated.
/// `AVOCADO_STACKS=off` (or `0`, or empty) turns configured stacks off.
pub fn resolve_settings(
    file: Option<&str>,
    env_root: Option<&str>,
    env_helper: Option<&str>,
) -> Result<Settings> {
    let mut s: Settings = match file {
        Some(t) if !t.trim().is_empty() => {
            serde_yaml::from_str(t).context("invalid stacks config")?
        }
        _ => Settings::default(),
    };
    if let Some(r) = env_root {
        s.root = match r.trim() {
            "" | "0" | "off" | "false" => None,
            r => Some(r.to_string()),
        };
    }
    if let Some(h) = env_helper.filter(|h| !h.is_empty()) {
        s.helper_image = Some(h.to_string());
    }
    if let Some(r) = &s.root {
        // A path on the docker host (Linux, or the avocado-vm), whatever OS runs the CLI.
        if !r.starts_with('/') {
            bail!("stacks root '{r}' must be an absolute path");
        }
        if r.contains(':') || r.contains(char::is_whitespace) {
            bail!("stacks root '{r}' must not contain ':' or whitespace (it is a bind mount)");
        }
    }
    s.gc_args()?;
    Ok(s)
}

/// The machine's settings, read once per process.
pub fn settings() -> Result<Settings> {
    static S: std::sync::OnceLock<std::result::Result<Settings, String>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| {
        let file = config_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok());
        resolve_settings(
            file.as_deref(),
            std::env::var("AVOCADO_STACKS").ok().as_deref(),
            std::env::var("AVOCADO_STACKS_HELPER").ok().as_deref(),
        )
        .map_err(|e| format!("{e:#}"))
    })
    .clone()
    .map_err(|e| anyhow::anyhow!(e))
}

/// `200G`, `512M`, `1.5T`, `4096`: bytes, suffixes in powers of 1024.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let m: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => bail!("size '{s}': unknown unit '{c}' (use K, M, G or T)"),
            };
            (&t[..i], m)
        }
        _ => (t, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("size '{s}' is not a number"))?;
    if !n.is_finite() || n < 0.0 {
        bail!("size '{s}' must be a positive number");
    }
    Ok((n * mult as f64) as u64)
}

pub fn human_bytes(b: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// The stacks root to use for builds in this process: configured, and the helper's preflight
/// passed on this docker host. A failed preflight warns once and falls back to plain volumes.
pub async fn active_root(tool: &str) -> Option<String> {
    static ACTIVE: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    ACTIVE
        .get_or_init(|| async {
            let s = match settings() {
                Ok(s) => s,
                Err(e) => {
                    print_warning(
                        &format!("Stacks disabled: {e:#}. Using plain volumes."),
                        OutputLevel::Normal,
                    );
                    return None;
                }
            };
            let root = s.root.clone()?;
            match Helper::new(tool, &root, &s).json(&["preflight"]).await {
                Ok(_) => Some(root),
                Err(e) => {
                    print_warning(
                        &format!(
                            "Stacks at {root} cannot run on this host ({e:#}). Using plain volumes."
                        ),
                        OutputLevel::Normal,
                    );
                    None
                }
            }
        })
        .await
        .clone()
}

/// One helper invocation context: the container tool, the stacks root and the helper image.
pub struct Helper {
    tool: String,
    root: String,
    image: String,
}

impl Helper {
    pub fn new(tool: &str, root: &str, s: &Settings) -> Helper {
        Helper {
            tool: tool.to_string(),
            root: root.to_string(),
            image: s.helper_image().to_string(),
        }
    }

    /// The helper for configured stacks, or `None` when none is configured.
    pub fn configured(tool: &str) -> Result<Option<Helper>> {
        let s = settings()?;
        Ok(s.root.as_deref().map(|r| Helper::new(tool, r, &s)))
    }

    fn args(&self) -> Result<Vec<String>> {
        let mut a = vec![
            self.tool.clone(),
            "run".into(),
            "--rm".into(),
            "--privileged".into(),
            "--network=none".into(),
            "-v".into(),
            format!("{}:{}:rshared", self.root, self.root),
        ];
        // Views are mounted inside the project's docker volume, so the helper needs the volumes
        // directory with shared propagation for its mounts to reach the daemon's namespace.
        let v = volumes_dir(&self.tool)?;
        a.extend(["-v".into(), format!("{v}:{v}:rshared")]);
        a.extend([self.image.clone(), "--root".into(), self.root.clone()]);
        Ok(a)
    }

    pub async fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut a = self.args()?;
        a.extend(args.iter().map(|s| s.to_string()));
        Command::new(&a[0])
            .args(&a[1..])
            .stdin(Stdio::null())
            .output()
            .await
            .context("run the stacks helper")
    }

    pub async fn json(&self, args: &[&str]) -> Result<Value> {
        let o = self.output(args).await?;
        if !o.status.success() {
            bail!(
                "stacks {}: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Ok(serde_json::from_slice(&o.stdout).unwrap_or(Value::Null))
    }

    /// Take the build lock `name`; waits while another builder holds it. The holder keeps it
    /// until its stdin closes, so the lock dies with this process.
    pub async fn lock(&self, name: &str) -> Result<BuildLock> {
        let mut a = self.args()?;
        let stdin_only = crate::utils::interactivity::ContainerStdio::StdinOnly.flags();
        for (i, f) in stdin_only.iter().enumerate() {
            a.insert(2 + i, f.to_string());
        }
        a.extend(["hold".into(), name.into()]);
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
                        &format!("Stacks: waited {waited:.1}s for {name}."),
                        OutputLevel::Normal,
                    );
                }
                Ok(BuildLock { _child: child })
            }
            _ => bail!("stacks: could not take the lock {name}"),
        }
    }
}

fn volumes_dir(tool: &str) -> Result<String> {
    static DIR: std::sync::OnceLock<std::result::Result<String, String>> =
        std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let o = std::process::Command::new(tool)
            .args(["info", "-f", "{{.DockerRootDir}}"])
            .output()
            .map_err(|e| format!("{tool} info: {e}"))?;
        let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !o.status.success() || dir.is_empty() {
            return Err(format!(
                "{tool} did not report its data root (stacks need docker): {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Ok(format!("{dir}/volumes"))
    })
    .clone()
    .map_err(|e| anyhow::anyhow!(e))
}

/// Volumes whose views this process has already remounted.
static REMOUNTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Called whenever a project's volume is resolved: remount any of its views that are recorded
/// but not mounted (a reboot drops mounts). Without this, a command would silently see the
/// volume's own (empty) directory where the view belongs.
pub async fn remount_volume_views(tool: &str, volume: &str) -> Result<()> {
    let Some(root) = active_root(tool).await else {
        return Ok(());
    };
    if REMOUNTED.lock().unwrap().iter().any(|v| v == volume) {
        return Ok(());
    }
    let h = Helper::new(tool, &root, &settings()?);
    h.json(&["remount", &format!("{volume}-")])
        .await
        .with_context(|| format!("remount the stack views of {volume}"))?;
    REMOUNTED.lock().unwrap().push(volume.to_string());
    Ok(())
}

/// Before a project volume is removed: drop its stack views (their uppers and leases), so
/// `volume rm` never walks into a mounted view. With stacks unconfigured, refuse when views
/// are still mounted inside the volume (seen on a native Linux host): removing the volume
/// would empty them first and then fail.
pub async fn drop_volume_views(tool: &str, volume: &str) -> Result<Vec<String>> {
    if !volume.starts_with("avo-") {
        return Ok(Vec::new());
    }
    // Only stacks that can run here can hold views here: configured stacks whose helper
    // cannot run (or an invalid config) means plain volumes, and `clean` must keep working.
    let active = match active_root(tool).await {
        Some(root) => Some(Helper::new(tool, &root, &settings()?)),
        None => None,
    };
    match active {
        Some(h) => {
            let r = h
                .json(&["drop", "--prefix", &format!("{volume}-")])
                .await
                .with_context(|| format!("drop the stack views of {volume}"))?;
            REMOUNTED.lock().unwrap().retain(|v| v != volume);
            Ok(r["dropped"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default())
        }
        None => {
            let mounted = mounts_inside_volume(volume);
            if !mounted.is_empty() {
                bail!(
                    "volume {volume} has stack views mounted ({}) but stacks are not \
                     configured; configure them (`avocado stack config`) so they can be dropped first",
                    mounted.join(", ")
                );
            }
            Ok(Vec::new())
        }
    }
}

/// Mount points inside `volume`'s data directory, as this host sees them (empty when docker
/// runs elsewhere, e.g. in avocado-vm).
fn mounts_inside_volume(volume: &str) -> Vec<String> {
    let needle = format!("/volumes/{volume}/_data/");
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .filter(|m| m.contains(&needle))
        .map(String::from)
        .collect()
}

pub fn view_name(volume: &str, target: &str) -> String {
    format!("{volume}-{target}")
}

pub fn key_of(inputs: &Value) -> String {
    // serde_json maps are sorted, so this is canonical.
    crate::utils::signing_keys::hex::encode(&Sha256::digest(inputs.to_string().as_bytes()))
}

/// Whether a key may be shared with other projects: only when every package source is
/// immutable, i.e. the lock pins the distro feed to a snapshot, the releasever in force is
/// exactly that snapshot (not a user override naming a moving channel), and the project
/// declares no `repos:` of its own (URLs and paths that can change under the same name).
pub fn shareable(
    snapshot: Option<&crate::utils::lockfile::RepoSnapshot>,
    releasever: Option<&str>,
    has_custom_repos: bool,
) -> bool {
    let Some(s) = snapshot else { return false };
    !has_custom_repos
        && releasever
            == Some(
                crate::utils::snapshot::effective_releasever(&s.release, &s.channel, &s.snapshot)
                    .as_str(),
            )
}

/// Whether the view already holds `key`'s tree: it is on that layer, or on one built on top
/// of it (a re-run whose earlier steps are unchanged must not switch the view down and back).
pub fn already_in_view(check: &Value, key: &str) -> bool {
    let canon = check["canonical"].as_str().unwrap_or(key);
    let current = check["current"].as_str();
    current == Some(canon)
        || current == Some(key)
        || check["ancestors"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(canon)))
}

/// The keys the next run will compute for this run's steps, once the lock pins what this run
/// resolved: `(next, key)` for each step whose key changes. Each step keys on its own sections
/// of the lock only, so exactly those are refreshed; a step's parent is the previous step's
/// next key; a fresh-build nonce is not carried over.
pub fn next_keys(steps: &[(String, Value, String)], lock: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut prev: Option<(String, String)> = None; // (key, next key) of the previous step
    for (parent, inputs, key) in steps {
        let mut v = inputs.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("fresh");
        }
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
            Some((old, new)) if old == parent => new.clone(),
            _ => parent.clone(),
        };
        let next = key_of(&json!({"parent": parent, "inputs": v}));
        if next != *key {
            out.push((next.clone(), key.clone()));
        }
        prev = Some((key.clone(), next));
    }
    out
}

/// Whether anything a step installs is unpinned: a lock section it writes (`writes`) is
/// empty, or one of its own package pins (`pins`) is missing. Then the same inputs do not
/// name the same packages (an `unlock`, or `update` on a moving channel).
pub fn pins_missing(inputs: &Value) -> bool {
    let empty = |v: &Value| {
        v.is_null()
            || v.as_object().is_some_and(|m| m.is_empty())
            || v.as_array().is_some_and(|a| a.is_empty())
    };
    let section_missing = inputs["writes"].as_array().is_some_and(|w| {
        w.iter()
            .any(|s| s.as_str().is_some_and(|s| empty(&inputs["lock"][s])))
    });
    let pin_missing = inputs["pins"]
        .as_array()
        .is_some_and(|a| a.iter().any(|p| p.is_null()));
    section_missing || pin_missing
}

/// Fill in what `lock` (a target's lock section) lacks from `from` (the section a layer was
/// built with), recursively, never overwriting: on a layer hit, the project gets the pins of
/// what it now has, and anything it had already pinned (which the key matched) stays.
pub fn fill_missing(lock: &mut Value, from: &Value) {
    match (lock, from) {
        (Value::Object(l), Value::Object(f)) => {
            for (k, fv) in f {
                // A section the layer was built without says nothing: never write a null
                // into the lock (its sections are typed maps).
                if fv.is_null() {
                    continue;
                }
                match l.get_mut(k) {
                    Some(lv) if !lv.is_null() => fill_missing(lv, fv),
                    _ => {
                        l.insert(k.clone(), fv.clone());
                    }
                }
            }
        }
        (l, f) if l.is_null() => *l = f.clone(),
        _ => {}
    }
}

/// Why a view no longer belongs to a project, or `None` when it still does. `meta` is what the
/// CLI recorded on the view; `volume_exists` and `state_volume` (the project's
/// `.avocado-state` volume, when the project dir still has one) are looked up by the caller.
pub fn orphan_reason(
    name: &str,
    meta: &Value,
    volume_exists: bool,
    project_exists: bool,
    state_volume: Option<&str>,
) -> Option<String> {
    let volume = meta["volume"]
        .as_str()
        .map(String::from)
        .or_else(|| (name.starts_with("avo-") && name.len() > 40).then(|| name[..40].to_string()));
    let Some(volume) = volume else {
        return Some("not a project view".to_string());
    };
    if !volume_exists {
        return Some(format!("volume {volume} is gone"));
    }
    // Only an absolute project path says anything about which project this is.
    if meta["project"]
        .as_str()
        .is_some_and(|p| Path::new(p).is_absolute())
    {
        if !project_exists {
            return Some(format!(
                "project {} is gone",
                meta["project"].as_str().unwrap_or("")
            ));
        }
        if state_volume.is_some_and(|s| s != volume) {
            return Some(format!(
                "project {} now uses another volume",
                meta["project"].as_str().unwrap_or("")
            ));
        }
    }
    None
}

/// The SDK container image's id: part of every key (same tag, different image = different
/// key). The id is the image config's digest, so it is the same on every host that has the
/// same image.
pub async fn image_id(tool: &str, image: &str) -> Result<String> {
    let inspect = || async {
        Command::new(tool)
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .output()
            .await
    };
    let mut o = inspect().await?;
    if !o.status.success() {
        let p = Command::new(tool)
            .args(["pull", "-q", image])
            .output()
            .await?;
        if !p.status.success() {
            bail!(
                "cannot pull {image}: {}",
                String::from_utf8_lossy(&p.stderr).trim()
            );
        }
        o = inspect().await?;
    }
    let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if !o.status.success() || id.is_empty() {
        bail!("cannot read the image id of {image}");
    }
    Ok(id)
}

/// Inputs every key carries: the layout and the CLI build. The CLI build is the full
/// `--version` identity (sha and date), not the crate version: the step scripts live in the
/// CLI, so two builds of the same version can install different trees.
pub fn base_inputs(releasever: Option<&str>) -> Value {
    json!({
        "layout": "stacks/1",
        "cli": env!("AVOCADO_CLI_VERSION"),
        "releasever": releasever,
    })
}

/// How a step's layer came to be in the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The view already held it (or a layer built on it): nothing ran, nothing switched.
    InView,
    /// Another project (or an earlier run) built it: the view switched to it.
    Hit,
    /// This run built and committed it.
    Built,
}

pub struct StepResult {
    pub key: String,
    pub outcome: Outcome,
    /// On a hit, the lock section the layer was built with (fill the project's lock from it).
    pub lock: Value,
}

pub struct Stacks {
    pub view: String,
    helper: Helper,
    volume: String,
    target: String,
    project: PathBuf,
    shareable: bool,
    /// (parent, inputs, key) of each step this session ran or hit, in order.
    steps: Mutex<Vec<(String, Value, String)>>,
}

/// Holds a build lock (a helper container blocked on our stdin); dropping it releases.
pub struct BuildLock {
    _child: Child,
}

impl Stacks {
    pub fn new(
        root: &str,
        tool: &str,
        volume: &str,
        target: &str,
        project: &Path,
        shareable: bool,
    ) -> Result<Self> {
        Ok(Self {
            view: view_name(volume, target),
            helper: Helper::new(tool, root, &settings()?),
            volume: volume.to_string(),
            target: target.to_string(),
            project: project.to_path_buf(),
            shareable,
            steps: Mutex::new(Vec::new()),
        })
    }

    /// Mount (creating if new) this project's view at `<volume>/_data/<target>`.
    pub async fn ensure_view(&self) -> Result<()> {
        let o = Command::new(&self.helper.tool)
            .args(["volume", "inspect", "-f", "{{.Mountpoint}}", &self.volume])
            .output()
            .await?;
        if !o.status.success() {
            bail!("stacks: no volume {}", self.volume);
        }
        let at = format!(
            "{}/{}",
            String::from_utf8_lossy(&o.stdout).trim(),
            self.target
        );
        let meta = json!({
            "project": self.project, "target": self.target, "volume": self.volume,
        })
        .to_string();
        let mut a = vec![
            "mount",
            self.view.as_str(),
            "--at",
            at.as_str(),
            "--meta",
            meta.as_str(),
        ];
        for p in PRIVATE {
            a.extend(["--private", p]);
        }
        self.helper.json(&a).await?;
        Ok(())
    }

    /// Serialize installs into this view (two `sdk install`s in one project).
    pub async fn lock_install(&self) -> Result<BuildLock> {
        self.helper.lock(&format!("install.{}", self.view)).await
    }

    /// Refuse to switch the view while another container has the project's volume: a running
    /// `build` or `sdk run` would see its tree replaced under it.
    async fn assert_idle(&self) -> Result<()> {
        // This process's own parked session containers would keep the old tree: drop them
        // (they restart on demand). Anyone else's means another command is working here.
        crate::utils::container::SessionContainers::release_sessions(&self.helper.tool);
        let o = Command::new(&self.helper.tool)
            .args([
                "ps",
                "--filter",
                &format!("volume={}", self.volume),
                "--format",
                "{{.ID}} {{.Names}}",
            ])
            .output()
            .await?;
        let own = format!("avocado-sess-{}-", std::process::id());
        let ids: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| {
                !l.split_whitespace()
                    .nth(1)
                    .is_some_and(|n| n.starts_with(&own))
            })
            .filter_map(|l| l.split_whitespace().next().map(String::from))
            .collect();
        if !ids.is_empty() {
            bail!(
                "stacks: {} container(s) are using this project's volume ({}): another \
                 avocado command is running in this project. Wait for it to finish (or stop \
                 those containers) and retry.",
                ids.len(),
                ids.join(", ")
            );
        }
        Ok(())
    }

    /// Scope a key: unshareable inputs get the view's name (only this project can hit them),
    /// and while anything the step installs is unpinned a fresh nonce, because then the same
    /// inputs do not name the same packages (after `avocado update` or `unlock` cleared pins,
    /// the old layer is stale).
    pub fn scope(&self, inputs: &mut Value) {
        if !self.shareable {
            inputs["unshared"] = self.view.clone().into();
            if pins_missing(inputs) {
                inputs["fresh"] = uuid::Uuid::new_v4().to_string().into();
            }
        }
    }

    /// After the install saved the lock file, alias the keys the next run will compute (with
    /// this run's pins) to this run's layers, so the re-run hits instead of rebuilding.
    pub async fn alias_for_lock(&self, lock: &Value) -> Result<()> {
        let steps = self.steps.lock().unwrap().clone();
        for (next, key) in next_keys(&steps, lock) {
            self.helper.json(&["alias", &next, &key]).await?;
        }
        Ok(())
    }

    /// Run one step against the view. The step's layer is a function of `parent` (the layer it
    /// runs on) and `inputs`. Already in the view: nothing happens. A hit switches the view to
    /// it. A miss puts the view on `parent`, runs `step` under the key's build lock, and commits
    /// the result with the lock section `step` returns.
    pub async fn step<F, Fut>(
        &self,
        label: &str,
        parent: &str,
        inputs: &Value,
        step: F,
    ) -> Result<StepResult>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        let t = std::time::Instant::now();
        let key = key_of(&json!({"parent": parent, "inputs": inputs}));
        self.steps
            .lock()
            .unwrap()
            .push((parent.to_string(), inputs.clone(), key.clone()));
        let c = self.helper.json(&["check", &self.view, &key]).await?;
        if already_in_view(&c, &key) {
            print_info(
                &format!("Stacks: {label} {} already in the view.", &key[..12]),
                OutputLevel::Normal,
            );
            // The project's lock may have lost pins the view's tree still has (an `unlock`
            // whose re-run lands on the same layer): give them back.
            let lock = match self.helper.json(&["record", &key]).await {
                Ok(rec) => rec["inputs"]["lock"].clone(),
                Err(_) => Value::Null,
            };
            return Ok(StepResult {
                key,
                outcome: Outcome::InView,
                lock,
            });
        }
        self.assert_idle().await?;
        let _lock = self.helper.lock(&key).await?;
        // `use` re-checks under the stacks GC lock: if GC evicted the layer since `check`, it
        // fails and this is a miss like any other.
        if self.helper.output(&["has", &key]).await?.status.success()
            && self.helper.json(&["use", &self.view, &key]).await.is_ok()
        {
            let rec = self.helper.json(&["record", &key]).await?;
            print_info(
                &format!(
                    "Stacks: {label} hit {} ({:.1}s).",
                    &key[..12],
                    t.elapsed().as_secs_f64()
                ),
                OutputLevel::Normal,
            );
            return Ok(StepResult {
                key,
                outcome: Outcome::Hit,
                lock: rec["inputs"]["lock"].clone(),
            });
        }
        self.helper.json(&["use", &self.view, parent]).await?;
        let lock = step().await?;
        let rec = self
            .helper
            .json(&[
                "commit",
                &self.view,
                &key,
                "--inputs",
                &json!({"label": label, "parent": parent, "inputs": inputs, "lock": lock})
                    .to_string(),
            ])
            .await?;
        print_info(
            &format!(
                "Stacks: {label} committed {} ({} files, {} new objects, {} new; commit {:.1}s, step total {:.1}s).",
                &key[..12],
                rec["files"],
                rec["new_objects"],
                human_bytes(rec["new_bytes"].as_u64().unwrap_or(0)),
                rec["secs"].as_f64().unwrap_or(0.0),
                t.elapsed().as_secs_f64()
            ),
            OutputLevel::Normal,
        );
        Ok(StepResult {
            key,
            outcome: Outcome::Built,
            lock: Value::Null,
        })
    }

    /// Put the view on exactly `key` (not a layer built on it): for a project whose last
    /// part fell away.
    pub async fn settle_on(&self, key: &str) -> Result<()> {
        let c = self.helper.json(&["check", &self.view, key]).await?;
        if c["current"] == c["canonical"] {
            return Ok(());
        }
        self.assert_idle().await?;
        self.helper.json(&["use", &self.view, key]).await?;
        print_info(
            &format!("Stacks: view back on {}.", &key[..key.len().min(12)]),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Apply the configured automatic cleanup (no-op without `gc:` limits).
    pub async fn auto_gc(&self) -> Result<()> {
        let Some(args) = settings()?.gc_args()? else {
            return Ok(());
        };
        let mut a = vec!["gc"];
        a.extend(args.iter().map(String::as_str));
        let r = self.helper.json(&a).await?;
        let freed = r["freed_bytes"].as_u64().unwrap_or(0);
        if freed > 0 {
            print_info(
                &format!(
                    "Stacks: freed {} ({} layers evicted).",
                    human_bytes(freed),
                    r["evicted_layers"].as_array().map(|a| a.len()).unwrap_or(0)
                ),
                OutputLevel::Normal,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::lockfile::RepoSnapshot;

    fn snap() -> RepoSnapshot {
        RepoSnapshot {
            release: "2026".into(),
            channel: "edge".into(),
            snapshot: "20261027-1".into(),
            repo_url: None,
            created: None,
        }
    }

    #[test]
    fn only_snapshot_pinned_feeds_are_shareable() {
        let s = snap();
        let pinned = crate::utils::snapshot::effective_releasever("2026", "edge", "20261027-1");
        assert!(shareable(Some(&s), Some(&pinned), false));
        // A moving channel exported by hand, or no pin at all.
        assert!(!shareable(Some(&s), Some("2026/edge"), false));
        assert!(!shareable(None, Some(&pinned), false));
        assert!(!shareable(Some(&s), None, false));
        // Project feeds (URLs, paths) can change under the same name.
        assert!(!shareable(Some(&s), Some(&pinned), true));
    }

    #[test]
    fn a_view_holds_its_layer_and_every_layer_it_was_built_on() {
        let c = json!({"current": "l2", "ancestors": ["l1", "l0", "empty"], "has": true, "canonical": "l1"});
        assert!(already_in_view(&c, "l1-alias"));
        assert!(already_in_view(
            &json!({"current": "l2", "ancestors": [], "canonical": "l2"}),
            "l2"
        ));
        // Not on the chain: switch.
        let other = json!({"current": "l2", "ancestors": ["l1", "l0", "empty"], "has": true, "canonical": "other"});
        assert!(!already_in_view(&other, "other"));
        // A new view (no current layer).
        assert!(!already_in_view(
            &json!({"current": null, "ancestors": [], "has": true, "canonical": "k"}),
            "k"
        ));
    }

    /// The next run's keys are computed from this run's inputs with the saved lock's pins, so
    /// the re-run hits the layers this run built; parents chain through the new keys.
    #[test]
    fn next_keys_chain_parents_and_drop_the_fresh_nonce() {
        let l0 = json!({"step": "L0", "host_arch": "x86_64", "pins_of": ["avocado-sdk-bootstrap"], "pins": [null]});
        let l1 = json!({"step": "L1", "lock": {"rootfs": null, "sdk": null}, "fresh": "abc"});
        let k0 = key_of(&json!({"parent": "empty", "inputs": l0}));
        let k1 = key_of(&json!({"parent": k0, "inputs": l1}));
        let steps = vec![
            ("empty".to_string(), l0.clone(), k0.clone()),
            (k0.clone(), l1.clone(), k1.clone()),
        ];
        let lock =
            json!({"sdk": {"x86_64": {"avocado-sdk-bootstrap": "1.0-r0"}}, "rootfs": {"a": "1"}});
        let next = next_keys(&steps, &lock);
        let mut l0n = l0.clone();
        l0n["pins"] = json!(["1.0-r0"]);
        let n0 = key_of(&json!({"parent": "empty", "inputs": l0n}));
        let l1n = json!({"step": "L1", "lock": {"rootfs": {"a": "1"}, "sdk": lock["sdk"]}});
        let n1 = key_of(&json!({"parent": n0, "inputs": l1n}));
        assert_eq!(next, vec![(n0, k0), (n1, k1)]);
        // Nothing changed: no aliases.
        let same = vec![(
            "empty".to_string(),
            l0n.clone(),
            key_of(&json!({"parent": "empty", "inputs": l0n})),
        )];
        assert!(next_keys(&same, &lock).is_empty());
    }

    #[test]
    fn pins_are_missing_when_any_written_section_or_pin_is_empty() {
        let l1 = |rootfs: Value, initramfs: Value| {
            json!({"writes": ["rootfs", "initramfs"],
                   "lock": {"sdk": {"x86_64": {"a": "1"}}, "rootfs": rootfs, "initramfs": initramfs}})
        };
        assert!(!pins_missing(&l1(json!({"r": "1"}), json!({"i": "1"}))));
        // `avocado unlock --rootfs`: one written section empty is enough.
        assert!(pins_missing(&l1(json!({}), json!({"i": "1"}))));
        assert!(pins_missing(&l1(Value::Null, json!({"i": "1"}))));
        // A section the step only reads (sdk) does not count.
        let mut v = l1(json!({"r": "1"}), json!({"i": "1"}));
        v["lock"]["sdk"] = Value::Null;
        assert!(!pins_missing(&v));
        // L0: its own package pins.
        assert!(pins_missing(&json!({"pins": ["1.0", null]})));
        assert!(!pins_missing(&json!({"pins": ["1.0", "2.0"]})));
        // A Project layer with no target-dev writes nothing of its own.
        assert!(!pins_missing(&json!({"writes": [], "lock": {}})));
    }

    #[test]
    fn fill_missing_adds_pins_and_never_overwrites() {
        let mut lock = json!({"sdk": {"x86_64": {"a": "1"}}, "rootfs": {}});
        let from = json!({"sdk": {"x86_64": {"a": "9", "b": "2"}}, "rootfs": {"r": "3"}, "initramfs": {"i": "4"},
                          "kernels": null, "target-sysroot": null});
        fill_missing(&mut lock, &from);
        assert_eq!(
            lock,
            json!({"sdk": {"x86_64": {"a": "1", "b": "2"}}, "rootfs": {"r": "3"}, "initramfs": {"i": "4"}})
        );
    }

    #[test]
    fn settings_come_from_the_file_then_the_environment() {
        let file = "root: /var/lib/avocado/stacks\nhelper_image: reg/avocado-stacks@sha256:abc\ngc:\n  max_size: 200G\n  min_free: 20G\n";
        let s = resolve_settings(Some(file), None, None).unwrap();
        assert_eq!(s.root.as_deref(), Some("/var/lib/avocado/stacks"));
        assert_eq!(s.helper_image(), "reg/avocado-stacks@sha256:abc");
        let gc = s.gc_args().unwrap().unwrap();
        assert_eq!(gc[1], (200u64 << 30).to_string());
        assert_eq!(gc[3], ((200u64 << 30) / 10 * 8).to_string());
        assert_eq!(gc[5], (20u64 << 30).to_string());
        // The environment overrides, and can turn configured stacks off.
        let e = resolve_settings(Some(file), Some("/srv/stacks"), Some("img:2")).unwrap();
        assert_eq!(
            (e.root.as_deref(), e.helper_image()),
            (Some("/srv/stacks"), "img:2")
        );
        assert_eq!(
            resolve_settings(Some(file), Some("off"), None)
                .unwrap()
                .root,
            None
        );
        // No config at all: stacks off.
        let none = resolve_settings(None, None, None).unwrap();
        assert_eq!(
            (none.root.as_deref(), none.helper_image()),
            (None, DEFAULT_HELPER_IMAGE)
        );
        assert!(none.gc_args().unwrap().is_none());
    }

    #[test]
    fn settings_are_validated() {
        for bad in [
            "root: relative/dir\n",
            "root: /a:b\n",
            "gc:\n  max_size: 10X\n",
            "rot: /x\n",
        ] {
            assert!(resolve_settings(Some(bad), None, None).is_err(), "{bad}");
        }
        assert!(resolve_settings(None, Some("relative"), None).is_err());
    }

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(
            parse_size("1.5T").unwrap(),
            (1.5 * (1u64 << 40) as f64) as u64
        );
        assert_eq!(parse_size(" 2g ").unwrap(), 2 << 30);
        assert!(parse_size("ten").is_err() && parse_size("-1G").is_err());
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn orphan_views_are_the_ones_whose_project_or_volume_is_gone() {
        let vol = "avo-11111111-2222-3333-4444-555555555555";
        let meta = json!({"project": "/p", "target": "qemux86-64", "volume": vol});
        let name = format!("{vol}-qemux86-64");
        assert_eq!(orphan_reason(&name, &meta, true, true, Some(vol)), None);
        assert!(orphan_reason(&name, &meta, false, true, Some(vol))
            .unwrap()
            .contains("volume"));
        assert!(orphan_reason(&name, &meta, true, false, None)
            .unwrap()
            .contains("gone"));
        assert!(orphan_reason(&name, &meta, true, true, Some("avo-other"))
            .unwrap()
            .contains("another volume"));
        // A view recorded before views carried metadata: judged by its volume.
        assert_eq!(orphan_reason(&name, &Value::Null, true, false, None), None);
        // A relative project path names whatever the current directory is: never judge by it.
        let rel = json!({"project": ".", "target": "qemux86-64", "volume": vol});
        assert_eq!(
            orphan_reason(&name, &rel, true, true, Some("avo-other")),
            None
        );
        assert!(orphan_reason(&name, &Value::Null, false, false, None).is_some());
        assert!(orphan_reason("verify-x", &Value::Null, true, true, None).is_some());
    }
}
