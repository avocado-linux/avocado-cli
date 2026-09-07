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
//! the container and dropped when it exits. dnf reads them from that mount
//! directly — it is appended to every `reposdir` list — so nothing is copied
//! into the shared, persistent sysroot: concurrent stages (`sdk install` runs
//! the rootfs and initramfs installs at once) cannot race on one directory,
//! and credentials never land in the docker volume. The canonical document written to
//! `.avocado/feeds/<target>.json` never contains them — it is what the build
//! cache hashes (fast-rebuilds/plan.md §4.0), so it records credential
//! *identity*, never the credential.

use std::collections::{BTreeSet, HashMap};
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

/// Prefix of every generated `.repo`/CA file. Earlier builds copied these into
/// the sysroot's `yum.repos.d`; the entrypoint still purges that prefix there
/// so volumes from those builds don't keep serving a stale feed set.
const GENERATED_PREFIX: &str = "avocado-feed-";

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
/// new endpoint, and reveals nothing about the token. `AVOCADO_CONNECT_TOKEN`
/// takes precedence over the profile store so CI runners identify without a
/// `credentials.json`.
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
    let token = match std::env::var("AVOCADO_CONNECT_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            let cfg = crate::commands::connect::client::load_config().ok()??;
            let (_, profile) = cfg.resolve_profile(None, None).ok()?;
            profile.token.clone()
        }
    };
    Some(short_sha256(token.as_bytes()))
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
    /// For a built-in re-scope: the repoid glob passed to `--disablerepo`.
    pub baseurl: String,
    pub priority: u32,
    pub gpgcheck: bool,
    pub gpgkey: Option<String>,
    /// Empty = every stage.
    pub stages: BTreeSet<FeedStage>,
    pub locality: Locality,
    /// Who resolves this feed: `none`, `basic:<sha256(username)[:12]>`, or (Phase 3)
    /// an org/profile name. Identity for cache keys — never the secret, and not the
    /// raw username either, which is often an email or a token.
    pub credential_identity: String,
    pub tls_verify: bool,
    /// `path:` feeds only — the path as written in config (project-relative) and
    /// the full sha256 of its `repodata/repomd.xml`. The local analogue of the
    /// snapshot pin: a different directory or new RPMs must move the stamp hash.
    ///
    /// Full, not the 12-character prefix used for identifiers elsewhere in this
    /// module. A truncated identifier only has to be unlikely to collide by
    /// accident; this one decides whether a rebuild happens, and the content it
    /// digests can come from a feed someone else controls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
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
    /// SHA-256 of the stage projection this was materialized from. Lets
    /// process-level caches keyed on the feed identity (kernel_resolver) tell
    /// two feed sets apart without re-deriving anything.
    pub fingerprint: String,
}

impl ResolvedFeedSet {
    /// Resolve the feed set, or `None` when the project declares no feeds
    /// (the zero-cost path: behaviour is exactly today's single implicit feed).
    ///
    /// `releasever` overrides what `Config::get_releasever()` would return. It
    /// exists for the stamp hash: install commands export the snapshot pin into
    /// `AVOCADO_RELEASEVER` before hashing, `runtime build` does not — it reads
    /// the pin from the lock and passes it explicitly. Both sides must expand
    /// `$releasever` identically or every pinned project reads as stale at build
    /// time right after install declared it current. Materialization passes
    /// `None` (env, which at install time *is* the pin).
    pub fn resolve(
        config: &Config,
        target: &str,
        config_dir: &Path,
        releasever: Option<&str>,
    ) -> Result<Option<Self>> {
        let repos = config.repos.as_ref();
        let distro = config.distro.as_ref();
        let feeds_list = distro.and_then(|d| d.feeds.as_ref());
        let distro_name = match distro.and_then(|d| d.repo.as_ref()) {
            Some(DistroRepoRef::Named(n)) => n.as_str(),
            _ => DEFAULT_DISTRO_FEED_NAME,
        };
        // Before the early return, so it covers every project that uses the feeds
        // system. The distro feed's URL arrives by a different route than
        // `repos.*.url` — the inline `distro.repo` block, `AVOCADO_REPO_URL`, or
        // the legacy `sdk.repo_url` — and none of those passed the userinfo check
        // named feeds get. A URL reaches the canonical document, the stamp hash
        // and dnf's own logs, so the rule has to hold wherever it came from.
        // The distro releasever is substituted into every distro-shaped baseurl, so
        // it is an INI-injection vector even though it never appears in `repos:`.
        // It can arrive from `distro.release`/`channel`, an explicit `releasever`,
        // or `AVOCADO_RELEASEVER`.
        if let Some(rv) = releasever
            .map(str::to_string)
            .or_else(|| config.get_releasever())
        {
            if rv.contains(['\n', '\r']) {
                bail!("the distro releasever must not contain a newline — it is substituted into every generated .repo");
            }
        }
        if url_has_userinfo(&config.effective_repo_url()) {
            bail!(
                "the distro feed URL carries credentials in the URL — put them in \
                 `username`/`password` on a named feed instead. A URL is recorded in \
                 the canonical document, the stamp hash and dnf's logs."
            );
        }
        if repos.is_none() && feeds_list.is_none() && distro_name == DEFAULT_DISTRO_FEED_NAME {
            return Ok(None);
        }
        let empty = HashMap::new();
        let repos = repos.unwrap_or(&empty);

        // Validate definitions once, independent of enablement — in name order so
        // the first error reported is the same every run.
        let mut names: Vec<&String> = repos.keys().collect();
        names.sort();
        for name in names {
            let def = &repos[name];
            if !is_valid_feed_name(name) {
                bail!("repos.{name}: feed names must match [A-Za-z0-9][A-Za-z0-9._-]* (they become dnf repo ids and file names)");
            }
            // Values are written verbatim into an INI file: a newline would inject options.
            for (field, value) in [
                ("url", &def.url),
                ("gpgkey", &def.gpgkey),
                ("username", &def.username),
                ("password", &def.password),
                ("ca", &def.ca),
                ("path", &def.path),
                // These reach the .repo through `$releasever` substitution in the
                // baseurl, so a newline in one injects an option just as surely as
                // a newline in the url itself.
                ("channel", &def.channel),
                ("releasever", &def.releasever),
            ] {
                if value.as_deref().is_some_and(|v| v.contains(['\n', '\r'])) {
                    bail!("repos.{name}: `{field}` must not contain a newline");
                }
            }
            if def
                .release
                .as_deref()
                .is_some_and(|v| v.contains(['\n', '\r']))
            {
                bail!("repos.{name}: `release` must not contain a newline");
            }
            let locators = [def.url.is_some(), def.org.is_some(), def.path.is_some()]
                .iter()
                .filter(|b| **b)
                .count();
            if name == BUILTIN_EXT_FEED {
                // The message said "only `stages` may be set" while the check only
                // looked at locators, so username, gpgkey, targets and the rest were
                // accepted and then silently ignored — the built-in's .repo file is
                // baked into the SDK image and the CLI never writes one. Silently
                // ignoring a field a user deliberately set is worse than refusing it.
                let ignored: Vec<&str> = [
                    ("url", def.url.is_some()),
                    ("org", def.org.is_some()),
                    ("path", def.path.is_some()),
                    ("release", def.release.is_some()),
                    ("channel", def.channel.is_some()),
                    ("releasever", def.releasever.is_some()),
                    ("gpgkey", def.gpgkey.is_some()),
                    ("gpgcheck", def.gpgcheck.is_some()),
                    ("targets", def.targets.is_some()),
                    ("username", def.username.is_some()),
                    ("password", def.password.is_some()),
                    ("ca", def.ca.is_some()),
                    ("tls_verify", def.tls_verify.is_some()),
                ]
                .into_iter()
                .filter_map(|(k, set)| set.then_some(k))
                .collect();
                if !ignored.is_empty() {
                    bail!(
                        "repos.{name}: '{BUILTIN_EXT_FEED}' is a built-in feed served by the SDK \
                         image's baked .repo files; only `stages` may be set on it, but {} {} set",
                        ignored.join(", "),
                        if ignored.len() == 1 { "is" } else { "are" }
                    );
                }
                continue;
            }
            if locators != 1 {
                bail!("repos.{name}: exactly one of `url`, `org`, or `path` is required");
            }
            if name == DEFAULT_DISTRO_FEED_NAME
                && matches!(
                    distro.and_then(|d| d.repo.as_ref()),
                    Some(DistroRepoRef::Inline(_))
                )
            {
                bail!("repos.{name}: conflicts with the inline `distro.repo` block; use `distro.repo: {name}` or drop one");
            }
            if let Some(url) = &def.url {
                if name != distro_name
                    && (def.release.is_some() || def.channel.is_some() || def.releasever.is_some())
                    && !url.contains("$releasever")
                {
                    bail!(
                        "repos.{name}: `release`/`channel` are set but `url` has no `$releasever` — write the layout explicitly, e.g. {url}/$releasever/target/$target"
                    );
                }
            }
            if def.url.as_deref().is_some_and(url_has_userinfo) {
                bail!("repos.{name}: put credentials in `username`/`password`, not in the URL — a URL is recorded in the canonical document, the stamp hash and dnf's logs");
            }
            if name == distro_name {
                let unsupported: Vec<&str> = [
                    ("path", def.path.is_some()),
                    ("username", def.username.is_some()),
                    ("password", def.password.is_some()),
                    ("gpgkey", def.gpgkey.is_some()),
                    ("gpgcheck", def.gpgcheck.is_some()),
                    ("targets", def.targets.is_some()),
                    ("stages", def.stages.is_some()),
                ]
                .into_iter()
                .filter_map(|(k, set)| set.then_some(k))
                .collect();
                if !unsupported.is_empty() {
                    bail!(
                        "repos.{name}: the distro feed is served by the SDK image's baked .repo files and supports only url/release/channel/releasever/ca/tls_verify; unsupported here: {}",
                        unsupported.join(", ")
                    );
                }
            }
            if def.stages.as_ref().is_some_and(|s| s.is_empty()) {
                bail!("repos.{name}: `stages` must not be empty; omit it to enable the feed at every stage");
            }
            // Both halves, not just the empty one: a `username` with no `password`
            // key at all reaches `credential` as `(user, String::new())` and writes
            // a bare `password=` into the .repo, which is the same broken auth the
            // empty check exists to prevent — just arrived at differently.
            if def.username.is_some() && def.password.as_deref().is_none_or(str::is_empty) {
                bail!(
                    "repos.{name}: `username` is set but `password` is empty or missing — \
                     an unset environment variable interpolates to \"\""
                );
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
        let distro_releasever = releasever
            .map(str::to_string)
            .or_else(|| config.get_releasever());
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
                    source: None,
                    content_digest: None,
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
            let credential_identity = match &def.username {
                Some(u) => format!("basic:{}", short_sha256(u.as_bytes())),
                None => "none".to_string(),
            };
            let common = |kind, baseurl, locality, mount: Option<PathBuf>, loopback_rewritten| {
                ResolvedFeed {
                    source: def.path.clone(),
                    // Digest of the repodata as it stands now. Absent repodata is reported
                    // at materialize time; here it simply leaves the digest unset.
                    content_digest: mount
                        .as_ref()
                        .and_then(|m| fs::read(m.join("repodata").join("repomd.xml")).ok())
                        .map(|b| full_sha256(&b)),
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
                }
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
                source: None,
                content_digest: None,
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
            // A built-in that does NOT apply to this stage is not absent from the
            // build — it is actively disabled with `--disablerepo`, which changes
            // what dnf resolves. Dropping it here would make "avocado-ext scoped
            // away from rootfs" hash identically to "no re-scope at all", so the
            // rootfs sysroot would not rebuild when a user scopes the extension
            // repositories out of it. Mark it instead of removing it.
            for f in feeds.iter_mut() {
                let applies = f.get("stages").and_then(|s| s.as_array()).is_none_or(|s| {
                    s.is_empty() || s.iter().any(|x| x.as_str() == Some(&stage_name))
                });
                let builtin = f.get("kind").and_then(|k| k.as_str()) == Some("builtin");
                if builtin && !applies {
                    if let Some(o) = f.as_object_mut() {
                        o.insert("disabled_at_stage".into(), serde_json::Value::Bool(true));
                    }
                }
            }
            feeds.retain(|f| {
                if f.get("disabled_at_stage").is_some() {
                    return true;
                }
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
        // Write-then-rename, with a UNIQUE temp name. A fixed `<target>.json.tmp`
        // was not actually atomic across processes: two avocado runs resolving the
        // same target would write the same temp path and one could rename the
        // other's half-written file into place, or rename it out from under the
        // other's rename. `NamedTempFile` gives each writer its own.
        let mut tmp = tempfile::Builder::new()
            .prefix(&format!("{}.json.", self.target))
            .suffix(".tmp")
            .tempfile_in(&dir)
            .with_context(|| format!("creating a temp file in {}", dir.display()))?;
        std::io::Write::write_all(tmp.as_file_mut(), self.canonical_json()?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        tmp.persist(&path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(path)
    }

    /// Generate the per-stage `.repo` files and everything the container run
    /// needs to see them.
    pub fn materialize(&self, stage: FeedStage) -> Result<FeedMaterialization> {
        let tempdir = tempfile::Builder::new()
            .prefix("avocado-feeds-")
            .tempdir()
            .context("creating feeds tempdir")?;
        // Both scope dirs always exist: the entrypoint appends them to the dnf
        // reposdir lists unconditionally whenever AVOCADO_FEEDS_DIR is set.
        let scope = if stage.is_host() { "host" } else { "target" };
        fs::create_dir_all(tempdir.path().join("host"))?;
        fs::create_dir_all(tempdir.path().join("target"))?;
        let dir = tempdir.path().join(scope);

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
            #[cfg(unix)]
            if feed.credential.is_some() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&repo_path, fs::Permissions::from_mode(0o600))?;
            }
            if let Some(host) = &feed.mount {
                // Checked here, not at resolve time: the stamp hash resolves too, and a
                // `runtime build` over a cleaned output dir must compare stamps, not die.
                if !host.join("repodata").join("repomd.xml").is_file() {
                    bail!(
                        "repos.{}: {} has no repodata/repomd.xml; run `createrepo_c` on it first",
                        feed.name,
                        host.display()
                    );
                }
                // The outer mount is read-only, so docker cannot create this
                // mountpoint itself; it has to exist in the tempdir already.
                fs::create_dir_all(tempdir.path().join("paths").join(&feed.name))?;
                mounts.push((
                    host.clone(),
                    format!("{CONTAINER_FEEDS_DIR}/paths/{}", feed.name),
                ));
            }
            if feed.loopback_rewritten {
                crate::utils::output::print_info(
                    &format!(
                        "feed '{}': loopback URL rewritten to {HOST_GATEWAY_ALIAS} so the container reaches this machine",
                        feed.name
                    ),
                    crate::utils::output::OutputLevel::Normal,
                );
                if add_hosts.is_empty() {
                    add_hosts.push(format!("{HOST_GATEWAY_ALIAS}:host-gateway"));
                }
            }
        }

        let fingerprint = {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(self.stage_projection_json(stage)?.as_bytes());
            digest.iter().map(|b| format!("{b:02x}")).collect()
        };
        Ok(FeedMaterialization {
            _tempdir: Arc::new(tempdir),
            mounts,
            env,
            dnf_args,
            add_hosts,
            fingerprint,
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
/// Rewrite a loopback URL for container reachability and say so — once.
///
/// The distro feed's URL is rewritten at four container-start sites, so logging
/// at each would repeat the same line several times in one build. Named feeds
/// report their rewrites at materialization, which happens once; this gives the
/// distro feed the same single report. Saying nothing was the previous
/// behaviour, and it made an unreachable localhost feed look like a feed that
/// was simply down.
pub fn rewrite_loopback_reported(url: &str) -> String {
    static REPORTED: std::sync::Once = std::sync::Once::new();
    let (rewritten, changed) = rewrite_loopback(url);
    if changed {
        let msg = rewritten.clone();
        REPORTED.call_once(|| {
            crate::utils::output::print_info(
                &format!(
                    "distro feed: loopback URL rewritten to {msg} so the container can reach the host"
                ),
                crate::utils::output::OutputLevel::Normal,
            );
        });
    }
    rewritten
}

/// Full sha256, hex. For digests that gate a rebuild, where a truncation would
/// be a correctness question rather than a readability one.
fn full_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
    if !is_loopback_host(host) {
        return (url.to_string(), false);
    }
    let rewritten = format!(
        "{}{userinfo}{HOST_GATEWAY_ALIAS}{port}{}",
        &url[..scheme_end + 3],
        &rest[host_end..]
    );
    (rewritten, true)
}

fn short_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_end_matches('.');
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// Feed names become dnf repo ids, `.repo` file names and mount path segments.
fn is_valid_feed_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `https://user:pass@host/...` — credentials belong in `username`/`password`.
fn url_has_userinfo(url: &str) -> bool {
    let Some(i) = url.find("://") else {
        return false;
    };
    let rest = &url[i + 3..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    authority.contains('@')
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
# Generated .repo files are served straight from the read-only per-run mount,
# which the reposdir lists above already include when AVOCADO_FEEDS_DIR is set.
# Earlier builds copied them into the sysroot's yum.repos.d; drop any leftovers
# so a volume from then does not keep serving a stale feed set.
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

    /// A newline anywhere that reaches the generated .repo injects an INI option.
    /// The url and credential fields were checked; the release fields were not,
    /// and they reach the baseurl through `$releasever` substitution.
    #[test]
    fn a_newline_cannot_reach_the_repo_file_through_the_release_fields() {
        for (field, value) in [
            ("channel", "main\nenabled=0"),
            ("releasever", "2026/next\nenabled=0"),
            ("release", "2026\nenabled=0"),
        ] {
            let c = load(&format!(
                "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v/$releasever\n    {field}: {value:?}\n"
            ));
            let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("newline"), "{field} should be refused: {err}");
        }
        // And the distro-wide releasever, which never appears under `repos:` but is
        // substituted into every distro-shaped baseurl.
        let c = load(&format!(
            "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v\n"
        ));
        let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), Some("2026\nenabled=0"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("newline"),
            "distro releasever should be refused: {err}"
        );
    }

    /// Two processes resolving the same target must not be able to rename each
    /// other's half-written document into place.
    #[test]
    fn the_canonical_document_write_uses_a_unique_temp_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let c = load(&format!(
            "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        let path = set.write_canonical(dir.path()).unwrap();
        assert!(path.is_file());
        // Writing twice leaves exactly one file and no stray temp files.
        set.write_canonical(dir.path()).unwrap();
        let feeds_dir = dir.path().join(".avocado").join("feeds");
        let names: Vec<String> = fs::read_dir(&feeds_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["t.json".to_string()],
            "left temp files: {names:?}"
        );
    }

    /// The built-in feed's error message promised that only `stages` may be set;
    /// the check only looked at locators, so everything else was accepted and then
    /// silently ignored. Silently ignoring a field a user deliberately set is the
    /// failure mode worth preventing.
    #[test]
    fn builtin_feed_rejects_every_field_it_cannot_honour() {
        for field in [
            "username: u\n    password: p",
            "gpgkey: https://k",
            "targets: [x]",
            "tls_verify: false",
            "release: 2026",
        ] {
            let c = load(&format!(
                "{BASE}repos:\n  avocado-ext:\n    stages: [ext]\n    {field}\n"
            ));
            let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("only `stages` may be set"),
                "field {field:?} should be refused, got: {err}"
            );
        }
        // stages alone is still the supported case
        let ok = load(&format!(
            "{BASE}repos:\n  avocado-ext:\n    stages: [ext]\n"
        ));
        assert!(ResolvedFeedSet::resolve(&ok, "t", Path::new("."), None).is_ok());
    }

    /// Review findings from #241, each as the behaviour rather than the mechanism.
    #[test]
    fn credentials_never_travel_in_a_url_whichever_path_the_url_came_from() {
        // Named feeds were already checked; the distro feed's URL arrives via the
        // inline block, an env override or the legacy sdk key, and was not.
        let c = load(
            "distro:\n  release: 2026\n  channel: next\n  feeds: [v]\n  repo:\n    url: https://u:p@example/r\nrepos:\n  v:\n    url: https://v\n",
        );
        let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("credentials in the URL"), "got: {err}");
    }

    /// A username with no password key at all reaches the .repo as a bare
    /// `password=`, which is the same broken auth the empty-string check exists
    /// to prevent.
    #[test]
    fn username_without_a_password_is_rejected() {
        for tail in ["", "\n    password: \"\""] {
            let c = load(&format!(
                "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v\n    username: u{tail}\n"
            ));
            let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("empty or missing"), "got: {err}");
        }
    }

    /// Scoping the built-in extension repositories away from a stage changes what
    /// dnf resolves there, so it must change that stage's hash. Dropping the
    /// builtin from the projection made it hash identically to a project that
    /// never re-scoped anything, and the sysroot would not rebuild.
    #[test]
    fn disabling_a_builtin_for_a_stage_moves_that_stages_projection() {
        let scoped = load(&format!(
            "{BASE}repos:\n  avocado-ext:\n    stages: [ext]\n"
        ));
        let plain = load(&format!("{BASE}  feeds: []\n"));
        let set = ResolvedFeedSet::resolve(&scoped, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        let rootfs = set.stage_projection_json(FeedStage::Rootfs).unwrap();
        let ext = set.stage_projection_json(FeedStage::Ext).unwrap();
        assert_ne!(
            rootfs, ext,
            "rootfs disables the ext repos and ext does not; the projections must differ"
        );
        assert!(
            rootfs.contains("disabled_at_stage"),
            "the rootfs projection must record the disable: {rootfs}"
        );
        if let Ok(Some(plain_set)) = ResolvedFeedSet::resolve(&plain, "t", Path::new("."), None) {
            assert_ne!(
                rootfs,
                plain_set.stage_projection_json(FeedStage::Rootfs).unwrap(),
                "a re-scoped build must not hash like one that never re-scoped"
            );
        }
    }

    #[test]
    fn no_feeds_is_none() {
        let c = load(BASE);
        assert!(
            ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn order_is_priority_and_distro_is_implicitly_first() {
        let c = load(&format!(
            "{BASE}  feeds: [vendor]\nrepos:\n  vendor:\n    url: https://v.example/$releasever/$target\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."), None)
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
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", dir.path(), None)
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
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
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
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        let json = set.canonical_json().unwrap();
        assert!(!json.contains("hunter2"));
        assert!(json.contains(&format!(
            "\"credential_identity\": \"basic:{}\"",
            short_sha256(b"bob")
        )));
        assert!(
            !json.contains("bob"),
            "raw username must not reach the canonical document"
        );
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
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
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
        let set2 = ResolvedFeedSet::resolve(&c2, "t", Path::new("."), None)
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
        assert!(ResolvedFeedSet::resolve(&two, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
        let unknown = load(&format!("{BASE}  feeds: [ghost]\n"));
        assert!(
            ResolvedFeedSet::resolve(&unknown, "t", Path::new("."), None)
                .unwrap_err()
                .to_string()
                .contains("ghost")
        );
        let dup = load(&format!(
            "{BASE}  feeds: [a, a]\nrepos:\n  a:\n    url: https://a\n"
        ));
        assert!(ResolvedFeedSet::resolve(&dup, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("more than once"));
        let org = load(&format!("{BASE}repos:\n  acme:\n    org: acme\n"));
        assert!(ResolvedFeedSet::resolve(&org, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("Connect"));
        let named = load("distro:\n  repo: nope\n");
        assert!(ResolvedFeedSet::resolve(&named, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("nope"));
    }

    #[test]
    fn targets_filter_and_named_distro_ref() {
        let c = load(
            "distro:\n  release: 2026\n  channel: next\n  repo: mirror\n  feeds: [only-thor]\nrepos:\n  mirror:\n    url: http://127.0.0.1:9000\n  only-thor:\n    url: https://t.example\n    targets: [jetson-agx-thor]\n",
        );
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert_eq!(set.feeds.len(), 1);
        assert_eq!(set.feeds[0].name, "mirror");
        assert_eq!(set.feeds[0].kind, FeedKind::Distro);
    }

    /// The stamp hash passes the pin-aware releasever explicitly; the projection
    /// must follow it, not the process env, or install-time and build-time hash
    /// different strings for the same config.
    #[test]
    fn explicit_releasever_drives_every_expansion() {
        let c = load(&format!(
            "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v.example/$releasever\n"
        ));
        let pin = Some("2026/next/snapshots/9");
        let pinned = ResolvedFeedSet::resolve(&c, "t", Path::new("."), pin)
            .unwrap()
            .unwrap();
        assert!(
            pinned.feeds[0].baseurl.ends_with("/2026/next/snapshots/9"),
            "{}",
            pinned.feeds[0].baseurl
        );
        assert_eq!(
            pinned.feeds[1].baseurl,
            "https://v.example/2026/next/snapshots/9"
        );
        let live = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert!(live.feeds[0].baseurl.ends_with("/2026/next"));
        assert_ne!(
            pinned.stage_projection_json(FeedStage::Rootfs).unwrap(),
            live.stage_projection_json(FeedStage::Rootfs).unwrap()
        );
        let again = ResolvedFeedSet::resolve(&c, "t", Path::new("."), pin)
            .unwrap()
            .unwrap();
        assert_eq!(
            pinned.stage_projection_json(FeedStage::Rootfs).unwrap(),
            again.stage_projection_json(FeedStage::Rootfs).unwrap()
        );
    }

    #[test]
    fn default_named_distro_feed_is_honoured_without_distro_repo() {
        // repos.avocado with no `distro.repo:` line is the distro feed, not ignored.
        let c = load("distro:\n  release: 2026\n  channel: next\nrepos:\n  avocado:\n    url: https://mirror.example\n");
        assert_eq!(c.get_repo_url().as_deref(), Some("https://mirror.example"));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert!(set.feeds[0].baseurl.starts_with("https://mirror.example/"));
        // …but an inline block plus repos.avocado is a conflict, not a silent winner.
        let both = load("distro:\n  release: 2026\n  channel: next\n  repo: {url: https://a}\nrepos:\n  avocado:\n    url: https://b\n");
        assert!(ResolvedFeedSet::resolve(&both, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("conflicts"));
    }

    #[test]
    fn validation_rejects_injection_and_ambiguity() {
        let err = |yaml: &str| {
            ResolvedFeedSet::resolve(&load(yaml), "t", Path::new("."), None)
                .unwrap_err()
                .to_string()
        };
        assert!(
            err(&format!("{BASE}repos:\n  \"a b\":\n    url: https://a\n")).contains("feed names")
        );
        assert!(
            err(&format!("{BASE}repos:\n  \"../x\":\n    url: https://a\n")).contains("feed names")
        );
        assert!(err(&format!("{BASE}repos:\n  a:\n    url: https://a\n    username: u\n    password: \"x\\nsslverify=0\"\n")).contains("newline"));
        assert!(
            err(&format!("{BASE}repos:\n  a:\n    url: https://u:p@h/\n")).contains("username")
        );
        assert!(err(&format!(
            "{BASE}repos:\n  a:\n    url: https://a\n    stages: []\n"
        ))
        .contains("must not be empty"));
        assert!(err(&format!(
            "{BASE}repos:\n  a:\n    url: https://a\n    username: u\n    password: \"\"\n"
        ))
        .contains("empty"));
        assert!(err(&format!(
            "{BASE}repos:\n  avocado-ext:\n    url: https://a\n"
        ))
        .contains("built-in"));
        assert!(err(&format!(
            "{BASE}  feeds: [avocado-ext]\nrepos:\n  avocado-ext:\n    stages: [ext]\n"
        ))
        .contains("re-scope"));
    }

    /// A local feed's identity in the hash is where it points and what it holds:
    /// another directory, or new RPMs in the same one, must move the projection.
    #[test]
    fn path_feed_projection_tracks_directory_and_repodata() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["a", "b"] {
            fs::create_dir_all(dir.path().join(d).join("repodata")).unwrap();
            fs::write(
                dir.path().join(d).join("repodata/repomd.xml"),
                format!("<repomd>{d}</repomd>"),
            )
            .unwrap();
        }
        let proj = |p: &str| {
            let c = load(&format!(
                "{BASE}  feeds: [local]\nrepos:\n  local:\n    path: ./{p}\n"
            ));
            ResolvedFeedSet::resolve(&c, "t", dir.path(), None)
                .unwrap()
                .unwrap()
                .stage_projection_json(FeedStage::Rootfs)
                .unwrap()
        };
        let a1 = proj("a");
        assert_ne!(
            a1,
            proj("b"),
            "a different directory must change the projection"
        );
        fs::write(
            dir.path().join("a/repodata/repomd.xml"),
            "<repomd>a2</repomd>",
        )
        .unwrap();
        assert_ne!(
            a1,
            proj("a"),
            "new repodata in the same directory must change the projection"
        );
        assert!(a1.contains("\"source\": \"./a\"") && a1.contains("\"content_digest\""));
    }

    #[test]
    fn distro_feed_rejects_shapes_the_baked_repos_cannot_serve() {
        let err = |yaml: &str| {
            ResolvedFeedSet::resolve(&load(yaml), "t", Path::new("."), None)
                .unwrap_err()
                .to_string()
        };
        let e = err("distro:\n  release: 2026\n  channel: next\n  repo: m\nrepos:\n  m:\n    url: https://m\n    username: u\n    password: p\n    gpgkey: https://m/K\n");
        assert!(
            e.contains("distro feed") && e.contains("username") && e.contains("gpgkey"),
            "{e}"
        );
        let e =
            err("distro:\n  release: 2026\n  channel: next\nrepos:\n  avocado:\n    path: ./x\n");
        assert!(e.contains("path"), "{e}");
        // ca / tls_verify on the distro feed are fine — the getters honour them.
        let c = load("distro:\n  release: 2026\n  channel: next\n  repo: m\nrepos:\n  m:\n    url: https://m\n    tls_verify: false\n");
        assert!(c.get_repo_insecure());
    }

    #[test]
    fn url_feed_with_release_needs_the_placeholder() {
        let err = |yaml: &str| {
            ResolvedFeedSet::resolve(&load(yaml), "t", Path::new("."), None)
                .unwrap_err()
                .to_string()
        };
        assert!(err(&format!(
            "{BASE}repos:\n  v:\n    url: https://v/repo\n    release: 2024\n    channel: edge\n"
        ))
        .contains("$releasever"));
        let ok = load(&format!("{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v/$releasever/target/$target\n    release: 2024\n    channel: edge\n"));
        let set = ResolvedFeedSet::resolve(&ok, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert_eq!(set.feeds[1].baseurl, "https://v/2024/edge/target/t");
    }

    #[test]
    fn path_feed_repodata_is_checked_at_materialize_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap(); // no repodata
        let c = load(&format!(
            "{BASE}  feeds: [local]\nrepos:\n  local:\n    path: ./out\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", dir.path(), None)
            .unwrap()
            .unwrap();
        assert!(set
            .materialize(FeedStage::Rootfs)
            .unwrap_err()
            .to_string()
            .contains("createrepo_c"));
    }

    #[test]
    fn repo_file_fields_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("c.pem"), "-----BEGIN CERTIFICATE-----\n").unwrap();
        let c = load(&format!(
            "{BASE}  feeds: [signed, plain, off]\nrepos:\n  signed:\n    url: https://s/$target\n    gpgkey: https://s/KEY\n    ca: ./c.pem\n    tls_verify: false\n    targets: [t]\n  plain:\n    url: https://p\n  off:\n    url: https://o\n    gpgkey: https://o/KEY\n    gpgcheck: false\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", dir.path(), None)
            .unwrap()
            .unwrap();
        let m = set.materialize(FeedStage::Rootfs).unwrap();
        let read = |n: &str| {
            fs::read_to_string(m.mounts[0].0.join(format!("target/avocado-feed-{n}.repo"))).unwrap()
        };
        let signed = read("signed");
        assert!(
            signed.contains("baseurl=https://s/t\n"),
            "$target expands: {signed}"
        );
        assert!(signed.contains("gpgcheck=1\n") && signed.contains("gpgkey=https://s/KEY\n"));
        assert!(signed.contains("sslcacert=/run/avocado-feeds/target/avocado-feed-signed.ca.pem\n"));
        assert!(signed.contains("sslverify=0\n"));
        assert!(m.mounts[0]
            .0
            .join("target/avocado-feed-signed.ca.pem")
            .is_file());
        assert!(read("plain").contains("gpgcheck=0\n"));
        assert!(
            read("off").contains("gpgcheck=0\n"),
            "explicit gpgcheck: false wins over gpgkey"
        );
        assert_eq!(
            m.fingerprint,
            set.materialize(FeedStage::Rootfs).unwrap().fingerprint
        );
        let scoped = load(&format!(
            "{BASE}  feeds: [v]\nrepos:\n  v:\n    url: https://v\n    stages: [ext]\n"
        ));
        let s2 = ResolvedFeedSet::resolve(&scoped, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert_ne!(
            s2.materialize(FeedStage::Ext).unwrap().fingerprint,
            s2.materialize(FeedStage::Rootfs).unwrap().fingerprint
        );
    }

    #[test]
    fn per_feed_release_overrides_distro_and_literal_stays_when_unresolvable() {
        let c = load(&format!("{BASE}  feeds: [old]\nrepos:\n  old:\n    url: https://o/$releasever\n    release: 2024\n    channel: edge\n"));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert_eq!(set.feeds[1].baseurl, "https://o/2024/edge");
        let none = load("distro:\n  feeds: [v]\nrepos:\n  v:\n    url: https://v/$releasever\n");
        let set = ResolvedFeedSet::resolve(&none, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            set.feeds[1].baseurl, "https://v/$releasever",
            "left for dnf when nothing resolves it"
        );
    }

    #[test]
    #[serial_test::serial]
    fn connect_token_env_drives_the_key_id() {
        use sha2::{Digest, Sha256};
        std::env::set_var("AVOCADO_CONNECT_TOKEN", "tok");
        let ua = user_agent();
        std::env::remove_var("AVOCADO_CONNECT_TOKEN");
        let want: String = Sha256::digest(b"tok")
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(ua.ends_with(&format!(";key/{want};tier/1")), "{ua}");
        assert!(!ua.contains(' '));
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
        assert!(rewrite_loopback("http://LOCALHOST:1/").1);
        assert!(rewrite_loopback("http://127.0.0.2:1/").1);
        assert!(rewrite_loopback("http://[0:0:0:0:0:0:0:1]:1/").1);
        assert!(rewrite_loopback("http://localhost./").1);
        assert!(!rewrite_loopback("http://127.example.com/").1);
    }

    #[test]
    #[serial_test::serial]
    fn user_agent_is_space_free_and_versioned() {
        std::env::set_var("AVOCADO_CONNECT_TOKEN", "x");
        let ua = user_agent();
        std::env::remove_var("AVOCADO_CONNECT_TOKEN");
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
