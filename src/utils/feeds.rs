//! Named package feeds: resolution, `.repo` generation, and the canonical
//! resolved-feed-set document.
//!
//! Config declares feeds in `repos:` (definitions — a map, so composed configs
//! merge by name) and orders/enables them in `distro.feeds:` (a list; position
//! is dnf priority, first wins). Exactly one feed is the *distro* feed: it
//! drives `$releasever` and the `.repo` files baked into the SDK image. Every
//! other enabled feed gets a `.repo` generated here with a fully expanded
//! baseurl, so it can point anywhere — a third-party RPM repo, a local mirror,
//! or a directory on disk.
//!
//! Secrets (`username`/`password`) reach dnf only through the generated
//! `.repo` files, which live in a per-run tempdir bind-mounted read-only into
//! the container and dropped when it exits. The canonical document written to
//! `.avocado/feeds/<target>.json` never contains them — it is what the build
//! cache hashes (fast-rebuilds/plan.md §4.0), so it records credential
//! *identity*, never the credential.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::utils::config::{Config, DistroRepoRef};

/// Recipe version of the canonical document. Bump whenever its fields or
/// their derivation change; the build cache folds it into its key so a recipe
/// change invalidates cached sysroots exactly once.
pub const CANONICAL_VERSION: u32 = 1;

/// Name the distro feed answers to when `distro.repo` is inline or absent.
pub const DEFAULT_DISTRO_FEED_NAME: &str = "avocado";

/// Built-in feed that can be re-scoped by name without a locator. It is the
/// `<machine>-target-ext` repo the SDK metadata RPM emits last.
pub const BUILTIN_EXT_FEED: &str = "avocado-ext";
const BUILTIN_EXT_REPO_GLOB: &str = "*-target-ext";

/// Prefix of every generated `.repo`/CA file. The entrypoint purges files with
/// this prefix on every run, so a feed removed from config disappears from the
/// sysroot instead of lingering in the docker volume.
pub const GENERATED_PREFIX: &str = "avocado-feed-";

/// In-container mount point for the generated repo files and `path:` feeds.
pub const CONTAINER_FEEDS_DIR: &str = "/run/avocado-feeds";

/// Host alias a loopback feed URL is rewritten to so dnf inside the container
/// reaches the developer's machine rather than the container itself.
pub const HOST_GATEWAY_ALIAS: &str = "host.docker.internal";

/// The User-Agent every feed request carries. Version always; when a Connect
/// profile is logged in, a non-secret per-token key id so usage attributes to
/// an account (fast, per-machine counters at the edge; roll-up to org via
/// `user_api_tokens.token_hash`). Space-free on purpose: it rides in
/// `$DNF_SDK_HOST`, which the entrypoint word-splits.
///
/// The key id is the first 12 hex chars of SHA-256(token) — the same digest
/// Connect already stores as `token_hash`, so it joins server-side with no
/// new endpoint, and reveals nothing about the token.
pub fn user_agent() -> String {
    let base = concat!("avocado-cli/", env!("CARGO_PKG_VERSION"));
    // tier/1 = authenticated, tier not yet assigned by Connect. The edge
    // routes on `tier/<n>`, so without it a logged-in client would share the
    // anonymous bucket; Connect raises it once it hands the CLI a real tier.
    match feed_key_id() {
        Some(id) => format!("{base};key/{id};tier/1"),
        None => base.to_string(),
    }
}

fn feed_key_id() -> Option<String> {
    use sha2::{Digest, Sha256};
    let cfg = crate::commands::connect::client::load_config().ok()??;
    let (_, profile) = cfg.resolve_profile(None, None).ok()?;
    let digest = Sha256::digest(profile.token.as_bytes());
    Some(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
}

/// Priority step between consecutive `distro.feeds` entries. The distro feed's
/// built-in repos keep their relative order inside one step (sdk, target, tune,
/// noarch, ext = base+0..4), so a step of 10 leaves headroom.
const PRIORITY_STEP: u32 = 10;

/// Build stage a feed applies to. Matches the command surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedStage {
    Sdk,
    Rootfs,
    Runtime,
    Ext,
    Initramfs,
    Kernel,
}

impl FeedStage {
    /// `sdk`-stage feeds land in the host reposdir (seen by the bootstrap and
    /// combined dnf confs); every other stage's dnf reads the target reposdir.
    // ponytail: sdk-stage feeds are host-only; a feed for the target sysroot
    // during `sdk install` would need both dirs and a distinct repoid.
    fn is_host(self) -> bool {
        matches!(self, FeedStage::Sdk)
    }
}

impl std::fmt::Display for FeedStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FeedStage::Sdk => "sdk",
            FeedStage::Rootfs => "rootfs",
            FeedStage::Runtime => "runtime",
            FeedStage::Ext => "ext",
            FeedStage::Initramfs => "initramfs",
            FeedStage::Kernel => "kernel",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeedKind {
    /// The distro feed — served by the SDK image's baked `.repo` files.
    Distro,
    Url,
    Path,
    /// A built-in repo re-scoped by name (`avocado-ext`).
    Builtin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Locality {
    Shared,
    /// Content comes from inside the project (`path:` feeds). Never cache-publishable.
    ProjectLocal,
}

/// A feed after resolution: everything dnf will see, plus what the cache needs.
/// `#[serde(skip)]` marks the fields that must never reach the canonical document.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedFeed {
    pub name: String,
    pub kind: FeedKind,
    /// Fully expanded baseurl as dnf sees it (in-container path for `path:` feeds).
    /// For the distro feed: the `{repo_url}/{releasever}` prefix its repos hang off.
    pub baseurl: String,
    pub priority: u32,
    pub gpgcheck: bool,
    pub gpgkey: Option<String>,
    /// Empty = every stage.
    pub stages: BTreeSet<FeedStage>,
    pub locality: Locality,
    /// Who resolves this feed — a username, org, or profile name — or `none`.
    /// Identity, never the secret.
    pub credential_identity: String,
    pub tls_verify: bool,
    #[serde(skip)]
    credential: Option<(String, String)>,
    #[serde(skip)]
    ca: Option<PathBuf>,
    /// Host directory to bind-mount for `path:` feeds.
    #[serde(skip)]
    pub mount: Option<PathBuf>,
    #[serde(skip)]
    loopback_rewritten: bool,
}

impl ResolvedFeed {
    fn applies_to(&self, stage: FeedStage) -> bool {
        self.stages.is_empty() || self.stages.contains(&stage)
    }

    fn repo_file(&self, ca_in_container: Option<&str>) -> String {
        let mut s = format!(
            "[{name}]\nname={name}\nbaseurl={base}\nenabled=1\ngpgcheck={gpg}\n",
            name = self.name,
            base = self.baseurl,
            gpg = u8::from(self.gpgcheck),
        );
        if let Some(k) = &self.gpgkey {
            s.push_str(&format!("gpgkey={k}\n"));
        }
        s.push_str(&format!("priority={}\n", self.priority));
        if let Some((u, p)) = &self.credential {
            s.push_str(&format!("username={u}\npassword={p}\n"));
        }
        if let Some(ca) = ca_in_container {
            s.push_str(&format!("sslcacert={ca}\n"));
        }
        if !self.tls_verify {
            s.push_str("sslverify=0\n");
        }
        s
    }
}

/// Every enabled feed for one target, in priority order, across all stages.
/// One document per target; consumers project it per stage by filtering on
/// `stages` (see fast-rebuilds/plan.md §4.0).
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedFeedSet {
    pub version: u32,
    pub target: String,
    pub feeds: Vec<ResolvedFeed>,
    /// True when any feed's content comes from inside the project.
    pub any_project_local: bool,
    /// True when any feed is fetched with a credential.
    pub any_credentialed: bool,
    /// Set when something precedes the distro feed, so its built-in repos must
    /// be renumbered above it at runtime.
    #[serde(skip)]
    distro_priority_base: Option<u32>,
}

/// What a container run needs to see the resolved feeds. The tempdir holds
/// the generated `.repo` files (secrets included) and is dropped with the
/// last clone.
#[derive(Debug, Clone)]
pub struct FeedMaterialization {
    _tempdir: Arc<tempfile::TempDir>,
    /// `(host_path, container_path)` read-only bind mounts, outermost first.
    pub mounts: Vec<(PathBuf, String)>,
    pub env: Vec<(String, String)>,
    pub dnf_args: Vec<String>,
    /// `--add-host` entries the container needs.
    pub add_hosts: Vec<String>,
}

impl ResolvedFeedSet {
    /// Resolve the feed set, or `None` when the project declares no feeds
    /// (the zero-cost path: behaviour is exactly today's single implicit feed).
    pub fn resolve(config: &Config, target: &str, config_dir: &Path) -> Result<Option<Self>> {
        let repos = config.repos.as_ref();
        let distro = config.distro.as_ref();
        let feeds_list = distro.and_then(|d| d.feeds.as_ref());
        let distro_name = match distro.and_then(|d| d.repo.as_ref()) {
            Some(DistroRepoRef::Named(n)) => n.as_str(),
            _ => DEFAULT_DISTRO_FEED_NAME,
        };
        if repos.is_none() && feeds_list.is_none() && distro_name == DEFAULT_DISTRO_FEED_NAME {
            return Ok(None);
        }
        let repos = repos.cloned().unwrap_or_default();

        // Validate definitions once, independent of enablement.
        for (name, def) in &repos {
            let locators = [def.url.is_some(), def.org.is_some(), def.path.is_some()]
                .iter()
                .filter(|b| **b)
                .count();
            if name == BUILTIN_EXT_FEED {
                if locators != 0 {
                    bail!("repos.{name}: '{BUILTIN_EXT_FEED}' is a built-in feed; only `stages` may be set on it");
                }
                continue;
            }
            if locators != 1 {
                bail!("repos.{name}: exactly one of `url`, `org`, or `path` is required");
            }
            if def.org.is_some() {
                // ponytail: org feeds resolve through Connect (Phase 3); parse, don't serve.
                bail!("repos.{name}: `org:` feeds are resolved through Connect and are not yet supported");
            }
            if def.password.is_some() && def.username.is_none() {
                bail!("repos.{name}: `password` requires `username`");
            }
        }
        if distro_name != DEFAULT_DISTRO_FEED_NAME && !repos.contains_key(distro_name) {
            bail!("distro.repo names '{distro_name}' but repos: does not define it");
        }

        // Enablement order: distro.feeds, with the distro feed implicitly first
        // unless the user placed it explicitly.
        let mut order: Vec<String> = feeds_list.cloned().unwrap_or_default();
        for n in &order {
            if n == BUILTIN_EXT_FEED {
                bail!("distro.feeds: '{BUILTIN_EXT_FEED}' is a built-in; re-scope it under repos:, don't list it");
            }
            if n != distro_name && !repos.contains_key(n) {
                bail!("distro.feeds names '{n}' but repos: does not define it");
            }
        }
        if !order.iter().any(|n| n == distro_name) {
            order.insert(0, distro_name.to_string());
        }
        {
            let mut seen = BTreeSet::new();
            for n in &order {
                if !seen.insert(n) {
                    bail!("distro.feeds lists '{n}' more than once");
                }
            }
        }

        let repo_url = config.effective_repo_url();
        let distro_releasever = config.get_releasever();
        let mut feeds = Vec::new();
        let mut distro_priority_base = None;

        for (k, name) in order.iter().enumerate() {
            let priority = PRIORITY_STEP * (k as u32 + 1);
            if name == distro_name {
                if k > 0 {
                    distro_priority_base = Some(priority);
                }
                feeds.push(ResolvedFeed {
                    name: name.clone(),
                    kind: FeedKind::Distro,
                    baseurl: match &distro_releasever {
                        Some(rv) => format!("{}/{rv}", repo_url.trim_end_matches('/')),
                        None => repo_url.clone(),
                    },
                    priority,
                    gpgcheck: false,
                    gpgkey: None,
                    stages: BTreeSet::new(),
                    locality: Locality::Shared,
                    credential_identity: "none".into(),
                    tls_verify: !config.get_repo_insecure(),
                    credential: None,
                    ca: None,
                    mount: None,
                    loopback_rewritten: false,
                });
                continue;
            }
            let def = &repos[name];
            if let Some(targets) = &def.targets {
                if !targets.iter().any(|t| t == target) {
                    continue;
                }
            }
            let stages: BTreeSet<FeedStage> = def.stages.iter().flatten().copied().collect();
            let feed_releasever = def
                .releasever
                .clone()
                .or_else(|| match (&def.release, &def.channel) {
                    (Some(r), Some(c)) => Some(format!("{r}/{c}")),
                    _ => None,
                })
                .or_else(|| distro_releasever.clone());
            let credential = def
                .username
                .as_ref()
                .map(|u| (u.clone(), def.password.clone().unwrap_or_default()));
            let credential_identity = def.username.clone().unwrap_or_else(|| "none".into());
            let common = |kind, baseurl, locality, mount, loopback_rewritten| ResolvedFeed {
                name: name.clone(),
                kind,
                baseurl,
                priority,
                gpgcheck: def.gpgcheck.unwrap_or(def.gpgkey.is_some()),
                gpgkey: def.gpgkey.clone(),
                stages: stages.clone(),
                locality,
                credential_identity: credential_identity.clone(),
                tls_verify: def.tls_verify.unwrap_or(true),
                credential: credential.clone(),
                ca: def.ca.as_ref().map(|c| resolve_relative(config_dir, c)),
                mount,
                loopback_rewritten,
            };

            if let Some(url) = &def.url {
                let mut expanded = url.replace("$target", target);
                if let Some(rv) = &feed_releasever {
                    expanded = expanded.replace("$releasever", rv);
                }
                let (expanded, rewritten) = rewrite_loopback(&expanded);
                feeds.push(common(
                    FeedKind::Url,
                    expanded,
                    Locality::Shared,
                    None,
                    rewritten,
                ));
            } else if let Some(p) = &def.path {
                let host = resolve_relative(config_dir, p);
                if !host.join("repodata").join("repomd.xml").is_file() {
                    bail!(
                        "repos.{name}: {} has no repodata/repomd.xml; run `createrepo_c` on it first",
                        host.display()
                    );
                }
                let in_container = format!("{CONTAINER_FEEDS_DIR}/paths/{name}");
                feeds.push(common(
                    FeedKind::Path,
                    format!("file://{in_container}"),
                    Locality::ProjectLocal,
                    Some(host),
                    false,
                ));
            }
        }

        // Built-in re-scopes ride along at the distro feed's slot for the record.
        if let Some(def) = repos.get(BUILTIN_EXT_FEED) {
            let distro_prio = feeds
                .iter()
                .find(|f| f.kind == FeedKind::Distro)
                .map(|f| f.priority)
                .unwrap_or(PRIORITY_STEP);
            feeds.push(ResolvedFeed {
                name: BUILTIN_EXT_FEED.into(),
                kind: FeedKind::Builtin,
                baseurl: BUILTIN_EXT_REPO_GLOB.into(),
                priority: distro_prio + 4,
                gpgcheck: false,
                gpgkey: None,
                stages: def.stages.iter().flatten().copied().collect(),
                locality: Locality::Shared,
                credential_identity: "none".into(),
                tls_verify: true,
                credential: None,
                ca: None,
                mount: None,
                loopback_rewritten: false,
            });
        }

        Ok(Some(Self {
            version: CANONICAL_VERSION,
            target: target.to_string(),
            any_project_local: feeds.iter().any(|f| f.locality == Locality::ProjectLocal),
            any_credentialed: feeds.iter().any(|f| f.credential.is_some()),
            feeds,
            distro_priority_base,
        }))
    }

    /// The canonical document: deterministic, secret-free, what the cache hashes.
    pub fn canonical_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serializing feed set")
    }

    /// The per-stage view the build cache hashes: the feeds visible at `stage`,
    /// with each feed's `stages` field dropped so two stages that see the same
    /// feed set hash identically. Same serialization as [`Self::canonical_json`];
    /// the drop-`stages` rule lives here, next to the struct whose field order
    /// defines it (fast-rebuilds/plan.md §4.0).
    pub fn stage_projection_json(&self, stage: FeedStage) -> Result<String> {
        let mut v = serde_json::to_value(self).context("serializing feed set")?;
        let stage_name = stage.to_string();
        if let Some(feeds) = v.get_mut("feeds").and_then(|f| f.as_array_mut()) {
            feeds.retain(|f| {
                f.get("stages").and_then(|s| s.as_array()).is_none_or(|s| {
                    s.is_empty() || s.iter().any(|x| x.as_str() == Some(&stage_name))
                })
            });
            for f in feeds.iter_mut() {
                if let Some(o) = f.as_object_mut() {
                    o.remove("stages");
                }
            }
        }
        serde_json::to_string_pretty(&v).context("serializing feed projection")
    }

    /// Write the canonical document to `<config_dir>/.avocado/feeds/<target>.json`.
    pub fn write_canonical(&self, config_dir: &Path) -> Result<PathBuf> {
        let dir = config_dir.join(".avocado").join("feeds");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.json", self.target));
        fs::write(&path, self.canonical_json()?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Generate the per-stage `.repo` files and everything the container run
    /// needs to see them.
    pub fn materialize(&self, stage: FeedStage) -> Result<FeedMaterialization> {
        let tempdir = tempfile::Builder::new()
            .prefix("avocado-feeds-")
            .tempdir()
            .context("creating feeds tempdir")?;
        let scope = if stage.is_host() { "host" } else { "target" };
        let dir = tempdir.path().join(scope);
        fs::create_dir_all(&dir)?;

        let mut mounts = vec![(
            tempdir.path().to_path_buf(),
            CONTAINER_FEEDS_DIR.to_string(),
        )];
        let mut env = vec![(
            "AVOCADO_FEEDS_DIR".to_string(),
            CONTAINER_FEEDS_DIR.to_string(),
        )];
        let mut dnf_args = Vec::new();
        let mut add_hosts = Vec::new();

        if let Some(base) = self.distro_priority_base {
            env.push(("AVOCADO_DISTRO_PRIORITY_BASE".to_string(), base.to_string()));
        }
        for feed in &self.feeds {
            match feed.kind {
                FeedKind::Distro => continue,
                FeedKind::Builtin => {
                    if !feed.applies_to(stage) {
                        dnf_args.push(format!("--disablerepo={}", feed.baseurl));
                    }
                    continue;
                }
                FeedKind::Url | FeedKind::Path => {}
            }
            if !feed.applies_to(stage) {
                continue;
            }
            let ca_in_container = match &feed.ca {
                Some(ca) => {
                    let fname = format!("{GENERATED_PREFIX}{}.ca.pem", feed.name);
                    fs::copy(ca, dir.join(&fname)).with_context(|| {
                        format!("repos.{}: reading ca {}", feed.name, ca.display())
                    })?;
                    Some(format!("{CONTAINER_FEEDS_DIR}/{scope}/{fname}"))
                }
                None => None,
            };
            let repo_path = dir.join(format!("{GENERATED_PREFIX}{}.repo", feed.name));
            fs::write(&repo_path, feed.repo_file(ca_in_container.as_deref()))?;
            if feed.credential.is_some() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&repo_path, fs::Permissions::from_mode(0o600))?;
            }
            if let Some(host) = &feed.mount {
                // The outer mount is read-only, so docker cannot create this
                // mountpoint itself; it has to exist in the tempdir already.
                fs::create_dir_all(tempdir.path().join("paths").join(&feed.name))?;
                mounts.push((
                    host.clone(),
                    format!("{CONTAINER_FEEDS_DIR}/paths/{}", feed.name),
                ));
            }
            if feed.loopback_rewritten && add_hosts.is_empty() {
                add_hosts.push(format!("{HOST_GATEWAY_ALIAS}:host-gateway"));
            }
        }

        Ok(FeedMaterialization {
            _tempdir: Arc::new(tempdir),
            mounts,
            env,
            dnf_args,
            add_hosts,
        })
    }
}

/// Relative to the project root, and always absolute: these paths become
/// docker bind-mount sources, which must not depend on the CLI's cwd.
fn resolve_relative(config_dir: &Path, p: &str) -> PathBuf {
    let joined = config_dir.join(p);
    std::path::absolute(&joined).unwrap_or(joined)
}

/// dnf runs inside the container, so a developer's `http://localhost:8080`
/// would resolve to the container itself. Rewrite loopback hosts to the
/// host-gateway alias and tell the caller so it can add the `--add-host`.
pub fn rewrite_loopback(url: &str) -> (String, bool) {
    let Some(scheme_end) = url.find("://") else {
        return (url.to_string(), false);
    };
    let rest = &url[scheme_end + 3..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..=i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        match stripped.find(']') {
            Some(i) => (&hostport[..=i + 1], &hostport[i + 2..]),
            None => (hostport, ""),
        }
    } else {
        match hostport.find(':') {
            Some(i) => (&hostport[..i], &hostport[i..]),
            None => (hostport, ""),
        }
    };
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]") {
        return (url.to_string(), false);
    }
    let rewritten = format!(
        "{}{userinfo}{HOST_GATEWAY_ALIAS}{port}{}",
        &url[..scheme_end + 3],
        &rest[host_end..]
    );
    (rewritten, true)
}

/// Shell appended to the container entrypoint after `$DNF_SDK_HOST` and the
/// reposdirs exist and before the first dnf call. Driven by env from
/// [`FeedMaterialization`]. The entrypoint has no `set -e`, so every write
/// fails closed explicitly.
pub const FEEDS_SETUP_SNIPPET: &str = r##"
# --- feed identity: every dnf request carries the CLI version and, when
# logged in, a non-secret key id (see utils::feeds::user_agent) ---
if [ -n "${AVOCADO_FEED_UA:-}" ]; then
    export DNF_SDK_HOST="${DNF_SDK_HOST} --setopt=user_agent=${AVOCADO_FEED_UA}"
fi
# --- named feeds (repos: / distro.feeds) ---
# Always drop last run's generated files: a feed removed from config must not
# survive in the sysroot's yum.repos.d (which persists in the docker volume).
rm -f "${DNF_SDK_HOST_PREFIX}"/etc/yum.repos.d/avocado-feed-*.repo \
      "${DNF_SDK_HOST_PREFIX}"/etc/yum.repos.d/avocado-feed-*.ca.pem \
      "${DNF_SDK_TARGET_PREFIX}"/etc/yum.repos.d/avocado-feed-*.repo \
      "${DNF_SDK_TARGET_PREFIX}"/etc/yum.repos.d/avocado-feed-*.ca.pem 2>/dev/null
if [ -n "${AVOCADO_DISTRO_PRIORITY_BASE:-}" ]; then
    # Something precedes the distro feed: lift every built-in repo above it,
    # preserving their relative order (sdk, target, tune, noarch, ext).
    _i=0
    for _f in "${DNF_SDK_HOST_PREFIX}"/etc/yum.repos.d/*.repo "${DNF_SDK_TARGET_PREFIX}"/etc/yum.repos.d/*.repo; do
        [ -e "$_f" ] || continue
        for _id in $(grep -o '^\[[^]]*\]' "$_f" | tr -d '[]'); do
            export DNF_SDK_HOST="${DNF_SDK_HOST} --setopt=${_id}.priority=$((AVOCADO_DISTRO_PRIORITY_BASE + _i))"
            _i=$((_i + 1))
        done
    done
fi
if [ -n "${AVOCADO_FEEDS_DIR:-}" ] && [ -d "${AVOCADO_FEEDS_DIR}" ]; then
    mkdir -p "${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d" "${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d" || exit 1
    for _f in "${AVOCADO_FEEDS_DIR}"/host/avocado-feed-*; do
        [ -e "$_f" ] || continue
        cp "$_f" "${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d/" || exit 1
    done
    for _f in "${AVOCADO_FEEDS_DIR}"/target/avocado-feed-*; do
        [ -e "$_f" ] || continue
        cp "$_f" "${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d/" || exit 1
    done
fi
"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn load(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("yaml parses")
    }

    const BASE: &str = r#"
distro:
  release: 2026
  channel: next
"#;

    #[test]
    fn no_feeds_is_none() {
        let c = load(BASE);
        assert!(ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."))
            .unwrap()
            .is_none());
    }

    #[test]
    fn order_is_priority_and_distro_is_implicitly_first() {
        let c = load(&format!(
            "{BASE}  feeds: [vendor]\nrepos:\n  vendor:\n    url: https://v.example/$releasever/$target\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."))
            .unwrap()
            .unwrap();
        let names: Vec<_> = set
            .feeds
            .iter()
            .map(|f| (f.name.as_str(), f.priority))
            .collect();
        assert_eq!(names, vec![("avocado", 10), ("vendor", 20)]);
        assert_eq!(
            set.feeds[1].baseurl,
            "https://v.example/2026/next/qemux86-64"
        );
        assert!(set.distro_priority_base.is_none());
    }

    #[test]
    fn feed_ahead_of_distro_renumbers_builtins() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out/repodata")).unwrap();
        fs::write(dir.path().join("out/repodata/repomd.xml"), "<repomd/>").unwrap();
        let c = load(&format!(
            "{BASE}  feeds: [local, avocado]\nrepos:\n  local:\n    path: ./out\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(set.feeds[0].kind, FeedKind::Path);
        assert_eq!(
            set.feeds[0].baseurl,
            "file:///run/avocado-feeds/paths/local"
        );
        assert_eq!(set.distro_priority_base, Some(20));
        assert!(set.any_project_local);
        let m = set.materialize(FeedStage::Rootfs).unwrap();
        assert!(m
            .env
            .contains(&("AVOCADO_DISTRO_PRIORITY_BASE".into(), "20".into())));
        assert_eq!(m.mounts.len(), 2);
    }

    #[test]
    fn stage_scoping_filters_files_and_disables_builtin() {
        let c = load(&format!(
            "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v.example\n    stages: [ext]\n  avocado-ext:\n    stages: [ext, runtime]\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."))
            .unwrap()
            .unwrap();
        let ext = set.materialize(FeedStage::Ext).unwrap();
        assert!(ext.dnf_args.is_empty());
        assert!(ext.mounts[0].0.join("target/avocado-feed-v.repo").is_file());
        let rootfs = set.materialize(FeedStage::Rootfs).unwrap();
        assert_eq!(rootfs.dnf_args, vec!["--disablerepo=*-target-ext"]);
        assert!(!rootfs.mounts[0]
            .0
            .join("target/avocado-feed-v.repo")
            .exists());
        let sdk = set.materialize(FeedStage::Sdk).unwrap();
        assert!(!sdk.mounts[0].0.join("host/avocado-feed-v.repo").exists());
    }

    #[test]
    fn canonical_document_excludes_secrets_and_is_stable() {
        let c = load(&format!(
            "{BASE}  feeds: [n]\nrepos:\n  n:\n    url: http://localhost:8080/r\n    username: bob\n    password: hunter2\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."))
            .unwrap()
            .unwrap();
        let json = set.canonical_json().unwrap();
        assert!(!json.contains("hunter2"));
        assert!(json.contains("\"credential_identity\": \"bob\""));
        assert!(json.contains("\"any_credentialed\": true"));
        assert!(json.contains("http://host.docker.internal:8080/r"));
        assert_eq!(json, set.canonical_json().unwrap());
        let m = set.materialize(FeedStage::Rootfs).unwrap();
        assert_eq!(m.add_hosts, vec!["host.docker.internal:host-gateway"]);
        let repo = fs::read_to_string(m.mounts[0].0.join("target/avocado-feed-n.repo")).unwrap();
        assert!(repo.contains("password=hunter2"));
        assert!(repo.contains("priority=20"));
    }

    #[test]
    fn stage_projection_filters_and_drops_stages() {
        let c = load(&format!(
            "{BASE}  feeds: [a, b]\nrepos:\n  a:\n    url: https://a\n  b:\n    url: https://b\n    stages: [ext]\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."))
            .unwrap()
            .unwrap();
        let ext = set.stage_projection_json(FeedStage::Ext).unwrap();
        let rootfs = set.stage_projection_json(FeedStage::Rootfs).unwrap();
        assert!(ext.contains("\"name\": \"b\"") && !rootfs.contains("\"name\": \"b\""));
        assert!(!ext.contains("\"stages\"") && !rootfs.contains("\"stages\""));
        assert!(rootfs.contains("\"version\": 1"));
        // Order is priority, so reordering must change the projection.
        let c2 = load(&format!(
            "{BASE}  feeds: [b, a]\nrepos:\n  a:\n    url: https://a\n  b:\n    url: https://b\n"
        ));
        let set2 = ResolvedFeedSet::resolve(&c2, "t", Path::new("."))
            .unwrap()
            .unwrap();
        assert_ne!(
            set.stage_projection_json(FeedStage::Ext).unwrap(),
            set2.stage_projection_json(FeedStage::Ext).unwrap()
        );
    }

    #[test]
    fn validation_errors() {
        let two = load(&format!(
            "{BASE}repos:\n  x:\n    url: https://a\n    path: ./b\n"
        ));
        assert!(ResolvedFeedSet::resolve(&two, "t", Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
        let unknown = load(&format!("{BASE}  feeds: [ghost]\n"));
        assert!(ResolvedFeedSet::resolve(&unknown, "t", Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("ghost"));
        let dup = load(&format!(
            "{BASE}  feeds: [a, a]\nrepos:\n  a:\n    url: https://a\n"
        ));
        assert!(ResolvedFeedSet::resolve(&dup, "t", Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("more than once"));
        let org = load(&format!("{BASE}repos:\n  acme:\n    org: acme\n"));
        assert!(ResolvedFeedSet::resolve(&org, "t", Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("Connect"));
        let named = load("distro:\n  repo: nope\n");
        assert!(ResolvedFeedSet::resolve(&named, "t", Path::new("."))
            .unwrap_err()
            .to_string()
            .contains("nope"));
    }

    #[test]
    fn targets_filter_and_named_distro_ref() {
        let c = load(
            "distro:\n  release: 2026\n  channel: next\n  repo: mirror\n  feeds: [only-thor]\nrepos:\n  mirror:\n    url: http://127.0.0.1:9000\n  only-thor:\n    url: https://t.example\n    targets: [jetson-agx-thor]\n",
        );
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."))
            .unwrap()
            .unwrap();
        assert_eq!(set.feeds.len(), 1);
        assert_eq!(set.feeds[0].name, "mirror");
        assert_eq!(set.feeds[0].kind, FeedKind::Distro);
    }

    #[test]
    fn loopback_rewrite_cases() {
        assert_eq!(
            rewrite_loopback("http://localhost:8080/x").0,
            "http://host.docker.internal:8080/x"
        );
        assert_eq!(
            rewrite_loopback("https://u:p@127.0.0.1/x").0,
            "https://u:p@host.docker.internal/x"
        );
        assert_eq!(
            rewrite_loopback("http://[::1]:80/").0,
            "http://host.docker.internal:80/"
        );
        assert!(!rewrite_loopback("https://repo.avocadolinux.org/x").1);
        assert!(!rewrite_loopback("file:///opt/x").1);
    }

    #[test]
    fn user_agent_is_space_free_and_versioned() {
        let ua = user_agent();
        assert!(ua.starts_with(concat!("avocado-cli/", env!("CARGO_PKG_VERSION"))));
        assert!(
            !ua.contains(' '),
            "UA rides in a word-split shell var: {ua}"
        );
    }

    #[test]
    fn snippet_parses() {
        let out = std::process::Command::new("sh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .take()
                    .unwrap()
                    .write_all(FEEDS_SETUP_SNIPPET.as_bytes())?;
                c.wait()
            })
            .expect("sh available");
        assert!(out.success(), "FEEDS_SETUP_SNIPPET failed sh -n");
    }
}
