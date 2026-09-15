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
    user_agent_with_tier(None)
}

/// The identity header, carrying the tier Connect actually issued.
///
/// `tier/1` means "authenticated, tier not yet assigned": it is the floor for a
/// logged-in client, not a claim. The edge routes rate limits on `tier/<n>`, so
/// a client that omits it shares the anonymous bucket — and one that reports the
/// wrong tier lands in the wrong bucket, which is why the value has to come from
/// the mint rather than from a constant. `None` keeps the floor, for a project
/// with no `org:` feed and therefore no minted tier.
pub fn user_agent_with_tier(tier: Option<u32>) -> String {
    user_agent_for(None, tier)
}

/// The identity header for a specific credential.
///
/// `key_id` names the credential that actually made the request. It matters when
/// an `org:` feed resolved through a non-default Connect profile: the default
/// profile's id would attribute those requests, in both the rate limiter and the
/// access log, to a credential that never made them.
pub fn user_agent_for(key_id: Option<&str>, tier: Option<u32>) -> String {
    let base = concat!("avocado-cli/", env!("CARGO_PKG_VERSION"));
    let id = key_id.map(str::to_string).or_else(feed_key_id);
    match id {
        // Clamped here, at the point the header is rendered, so the floor holds
        // whatever the tier came from. `tier/0` on an authenticated request would
        // place it in the anonymous bucket, which is worse than an unassigned
        // tier — and `tier/1` already means "authenticated, not yet assigned".
        Some(id) => format!("{base};key/{id};tier/{}", tier.unwrap_or(1).max(1)),
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

    /// The stage's subdirectory under the invocation's feeds root. Stages share
    /// one mount so every container has the same shape; this is what keeps their
    /// `.repo` sets apart inside it, which is what makes `stages:` scoping real
    /// rather than advisory.
    fn dir_name(self) -> &'static str {
        match self {
            FeedStage::Sdk => "sdk",
            FeedStage::Rootfs => "rootfs",
            FeedStage::Runtime => "runtime",
            FeedStage::Ext => "ext",
            FeedStage::Initramfs => "initramfs",
        }
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
    /// A private feed hosted by Connect, addressed by `org:`. Its URL and its
    /// credential are both issued at materialize time, so neither is a config input.
    Connect,
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

/// What the feed-token mint returns. `tier` is an entitlement the server
/// computes from the organization; the CLI mirrors it into the User-Agent and
/// never requests one.
#[derive(Debug, serde::Deserialize)]
struct MintedFeedToken {
    token: String,
    #[serde(default)]
    tier: Option<u32>,
    feed_url: String,
    #[serde(default)]
    #[allow(dead_code)]
    expires_at: Option<i64>,
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
    /// The organization for an `org:` feed. Recorded because it is the stable
    /// identity of the feed; the URL and token it resolves to are not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
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
    /// The locator as configured, for the lock: a `url:` with `$releasever` and
    /// `$target` expanded but WITHOUT the loopback rewrite, or a `path:` exactly
    /// as written. `baseurl` is the container's view — `host.docker.internal` on
    /// an ephemeral port, or a `file://` path inside the mount — which is
    /// plumbing, not provenance, and would make the lock churn on every run.
    /// `#[serde(skip)]`: the canonical document records `baseurl` and feeds the
    /// stamp hash, so adding a field there would move every hash.
    #[serde(skip)]
    configured_locator: String,
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
    /// Key id of the account credential the mint actually used. When an `org:`
    /// feed resolves through a non-default profile, that is a different token
    /// than `feed_key_id()` would find, and reporting the default profile's id
    /// would attribute the requests to a credential that never made them.
    #[serde(skip)]
    minted_key_id: Option<String>,
    /// Lowest tier the mint issued this invocation. Deliberately not serialized:
    /// it is assigned by the server and can change between builds, so it must not
    /// reach the canonical document or the stamp hash.
    #[serde(skip)]
    minted_tier: Option<u32>,
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
    /// The tier the mint issued, for the identity header. `None` when nothing was
    /// minted, which keeps the authenticated floor.
    pub tier: Option<u32>,
    /// Key id of the credential the mint used, when it differs from the default
    /// profile's.
    pub key_id: Option<String>,
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
                // The distro feed's URL is a *base*: the CLI appends the
                // releasever and the baked .repo files hang their own paths off
                // it. A `$releasever` or `$target` written here is not expanded —
                // the substitution only runs for named feeds — so it survives
                // verbatim into a baseurl like `.../$releasever/.../2026/next`.
                // Easy to write by copying a `repos:` example, and it fails as a
                // 404 rather than as a config error, so refuse it here.
                if let Some(url) = &def.url {
                    if let Some(var) = ["$releasever", "$target"]
                        .into_iter()
                        .find(|v| url.contains(v))
                    {
                        bail!(
                            "repos.{name}: the distro feed's `url` is a base URL and must not contain `{var}` —                              the release path is appended and the SDK image's baked .repo files add the rest.                              Write just the host and any prefix, e.g. https://repo.avocadolinux.org"
                        );
                    }
                }
            }
            if def.stages.as_ref().is_some_and(|s| s.is_empty()) {
                bail!("repos.{name}: `stages` must not be empty; omit it to enable the feed at every stage");
            }
            // Both halves, not just the empty one: a `username` with no `password`
            // key at all reaches `credential` as `(user, String::new())` and writes
            // a bare `password=` into the .repo, which is the same broken auth the
            // empty check exists to prevent — just arrived at differently.
            if def.org.is_some() && (def.username.is_some() || def.password.is_some()) {
                // Before the generic credential rules: "remove username/password,
                // Connect provides it" is more use than "password is missing".
                // Minting is skipped for a feed that already has a credential, so
                // an `org:` feed carrying one would keep its `connect://<org>/...`
                // placeholder baseurl and dnf would fail to resolve it.
                bail!(
                    "repos.{name}: an `org:` feed gets its credential from Connect; \
                     remove `username`/`password` (they are for feeds Connect knows \
                     nothing about)"
                );
            }
            if def.username.is_some() && def.password.as_deref().is_none_or(str::is_empty) {
                bail!(
                    "repos.{name}: `username` is set but `password` is empty or missing — \
                     an unset environment variable interpolates to \"\""
                );
            }
            // `org` is interpolated into the mint URL, into the generated baseurl,
            // and into the .repo file. A value with a slash, whitespace or a
            // newline could reshape the request path or inject an extra line into
            // the .repo, so it has to be one URL-safe segment. Same rule as feed
            // names, for the same reason.
            if def.org.is_some() {
                // `channel` becomes the branch segment in
                // `.../orgs/<org>/<branch>/...`, so it needs exactly the same rule
                // as the org: a slash or a space produces a malformed path, and the
                // server parses those segments.
                if let Some(branch) = &def.channel {
                    if !is_valid_feed_name(branch) {
                        bail!(
                            "repos.{name}: `channel: {branch}` is the branch segment of a \
                             Connect feed path and must match [A-Za-z0-9][A-Za-z0-9._-]*"
                        );
                    }
                }
            }
            if let Some(org) = &def.org {
                if !is_valid_feed_name(org) {
                    bail!(
                        "repos.{name}: `org: {org}` must be a single path segment matching \
                         [A-Za-z0-9][A-Za-z0-9._-]* — it is interpolated into a URL and into \
                         the generated .repo"
                    );
                }
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
                    configured_locator: match &distro_releasever {
                        Some(rv) => format!("{}/{rv}", repo_url.trim_end_matches('/')),
                        None => repo_url.clone(),
                    },
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
                    org: None,
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
            let credential_identity = match (&def.org, &def.username) {
                // The org is the stable identity of a Connect feed. The token it
                // resolves to changes every build and must never appear here: this
                // string goes into the canonical document and the stamp hash.
                (Some(o), _) => format!("connect:{o}"),
                (None, Some(u)) => format!("basic:{}", short_sha256(u.as_bytes())),
                (None, None) => "none".to_string(),
            };
            let common = |kind,
                          baseurl,
                          locality,
                          mount: Option<PathBuf>,
                          loopback_rewritten,
                          configured_locator: String| {
                ResolvedFeed {
                    configured_locator,
                    org: def.org.clone(),
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
                // The pre-rewrite form is what the lock records: the rewrite is
                // container reachability, and on an ephemeral port it would make
                // the lock churn every run.
                let configured = expanded.clone();
                let (expanded, rewritten) = rewrite_loopback(&expanded);
                feeds.push(common(
                    FeedKind::Url,
                    expanded,
                    Locality::Shared,
                    None,
                    rewritten,
                    configured,
                ));
            } else if let Some(org) = &def.org {
                // The private tree mirrors the public one, so an org feed is just a
                // distro-shaped feed whose releasever is `<rel>/orgs/<org>/<branch>`.
                // That is why no new path construction is needed here.
                let rel = def
                    .release
                    .clone()
                    .or_else(|| config.get_distro_release())
                    .unwrap_or_else(|| "2026".to_string());
                let branch = def.channel.clone().unwrap_or_else(|| "main".to_string());
                let path = format!("{rel}/orgs/{org}/{branch}/target/{target}");
                // A placeholder host, replaced with the minted `feed_url` at
                // materialize time. Recording it rather than the real URL keeps the
                // canonical document stable across builds and keeps a server-side
                // URL change out of the stamp hash — the org is the input, the host
                // is a detail of how it was served today.
                let placeholder = format!("connect://{org}/{path}");
                feeds.push(common(
                    FeedKind::Connect,
                    placeholder.clone(),
                    Locality::Shared,
                    None,
                    false,
                    placeholder,
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
                    // As written in the config, not the in-container mount path.
                    p.clone(),
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
                configured_locator: BUILTIN_EXT_REPO_GLOB.into(),
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
                org: None,
                source: None,
                content_digest: None,
                credential: None,
                ca: None,
                mount: None,
                loopback_rewritten: false,
            });
        }

        Ok(Some(Self {
            minted_tier: None,
            minted_key_id: None,
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

    /// Exchange the account credential for a short-lived feed token for every
    /// `org:` feed, and adopt the base URL the mint hands back.
    ///
    /// Deliberately **not** part of `resolve`. The token changes on every mint, so
    /// letting it near the stamp hash or the canonical document would invalidate
    /// every cached sysroot once per build. `resolve` records `connect:<org>` as
    /// the credential identity and a `connect://` placeholder as the URL; this
    /// fills in the real host and the secret, in memory, for the life of one
    /// invocation. The canonical document is written before this runs.
    ///
    /// Runs per stage: only feeds in scope for `stage` are minted, and only when
    /// they have no token yet.
    ///
    /// Returns the **lowest** tier issued, which the caller mirrors into the
    /// User-Agent so the edge can pick a rate-limit bucket. Lowest, not highest:
    /// dnf sends one header for every feed in a run, so claiming the best tier
    /// would ask for a ceiling one of the feeds was never granted. The CLI never
    /// *asks* for a tier either way — it is an entitlement the server computes.
    pub async fn resolve_connect_credentials(&mut self, stage: FeedStage) -> Result<Option<u32>> {
        // Only what this stage will actually use, and only once per feed. Minting
        // every `org:` feed regardless of stage made `stages:` mean less for a
        // private feed than for any other kind: a command whose stage excluded the
        // feed still needed a login and a network round trip. Later stages reuse
        // what earlier ones minted, so a feed is minted at most once per
        // invocation, and never at all if no stage needs it.
        if !self
            .feeds
            .iter()
            .any(|f| f.kind == FeedKind::Connect && f.applies_to(stage) && f.credential.is_none())
        {
            return Ok(self.minted_tier);
        }
        let profiles = crate::commands::connect::client::load_config()
            .ok()
            .flatten();
        // No fixed User-Agent on the client: the header is set per request, from
        // the credential that request actually uses. A client-wide `user_agent()`
        // reads the default profile, so a mint performed with an org-specific
        // profile would be attributed — and rate limited — as the default one.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building the HTTP client for the feed-token mint")?;

        // The lowest tier, not the highest: the tier is a claim the edge uses to
        // pick a rate-limit bucket, and over-claiming would ask for a ceiling one
        // of the feeds was never granted. Under-claiming only costs throughput.
        // Seeded from whatever an earlier stage already minted, so a later stage
        // does not forget it.
        let mut lowest: Option<u32> = self.minted_tier;
        let mut self_key_id: Option<String> = self.minted_key_id.clone();
        let mut key_seen = self.minted_key_id.is_some();
        for feed in self.feeds.iter_mut().filter(|f| {
            f.kind == FeedKind::Connect && f.applies_to(stage) && f.credential.is_none()
        }) {
            let org = feed.org.clone().ok_or_else(|| {
                anyhow::anyhow!("repos.{}: a Connect feed without an org", feed.name)
            })?;

            // Env wins, so CI can authenticate without a stored profile — the same
            // precedence the User-Agent key id already uses.
            let env_token = std::env::var("AVOCADO_CONNECT_TOKEN")
                .ok()
                .filter(|t| !t.is_empty());
            let (api_url, account_token) = match env_token {
                Some(t) => (
                    std::env::var("AVOCADO_CONNECT_URL")
                        .unwrap_or_else(|_| "https://connect.peridio.com".to_string()),
                    t,
                ),
                None => {
                    let cfg = profiles.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "repos.{}: `org: {org}` is a private feed and you are not logged in.\n\
                             Run `avocado login`, or set AVOCADO_CONNECT_TOKEN for CI.",
                            feed.name
                        )
                    })?;
                    let (_, profile) = cfg
                        .find_profile_by_org(&org)
                        .or_else(|| cfg.resolve_profile(None, None).ok())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "repos.{}: no Connect profile for org '{org}'.\n\
                                 Run `avocado login --org {org}`.",
                                feed.name
                            )
                        })?;
                    (profile.api_url.clone(), profile.token.clone())
                }
            };

            let url = format!(
                "{}/api/orgs/{org}/feed-tokens",
                api_url.trim_end_matches('/')
            );
            let resp = client
                .post(&url)
                .header(
                    reqwest::header::USER_AGENT,
                    user_agent_for(Some(&short_sha256(account_token.as_bytes())), None),
                )
                .bearer_auth(&account_token)
                .json(&serde_json::json!({}))
                .send()
                .await
                .with_context(|| {
                    format!("repos.{}: requesting a feed token from {url}", feed.name)
                })?;

            let status = resp.status();
            if !status.is_success() {
                let hint = match status.as_u16() {
                    401 => {
                        " — the stored credential was rejected. Run `avocado login` to refresh it."
                    }
                    403 => " — this account is not entitled to that org's private feed",
                    404 => " — this Connect deployment does not serve feed tokens yet",
                    _ => "",
                };
                bail!(
                    "repos.{}: feed-token request returned {status}{hint}",
                    feed.name
                );
            }
            let minted: MintedFeedToken = resp
                .json()
                .await
                .with_context(|| format!("repos.{}: parsing the feed-token response", feed.name))?;

            // `connect://<org>/<path>` -> `<feed_url>/<path>`.
            // Fail loudly. `unwrap_or_default()` here would yield an empty path and
            // a baseurl pointing at the feed ROOT rather than the org's subtree —
            // dnf would then read someone else's metadata, or nothing, and report
            // it as a broken feed. A Connect feed reaching this point without its
            // placeholder is a logic error in resolve, not a user mistake.
            let prefix = format!("connect://{org}/");
            let path = feed
                .baseurl
                .strip_prefix(&prefix)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "repos.{}: internal error — expected the placeholder {prefix:?} but found {:?}",
                        feed.name,
                        feed.baseurl
                    )
                })?
                .to_string();
            // The mint hands back a host the CLI can reach; dnf reaches it from
            // inside the container, where loopback means the container itself.
            // `url:` feeds are rewritten at resolve time, but a Connect feed has no
            // URL until now, so it has to happen here or a local Connect silently
            // resolves to nothing.
            let (baseurl, rewritten) =
                rewrite_loopback(&format!("{}/{path}", minted.feed_url.trim_end_matches('/')));
            feed.baseurl = baseurl;
            feed.loopback_rewritten = rewritten;
            // The username is for log correlation only; the verifier reads the
            // password. Using the same key id the User-Agent carries lets an
            // operator join a feed request to the client that made it.
            let key_id = short_sha256(account_token.as_bytes());
            // The identity header must name the credential that actually made the
            // request. `feed_key_id()` reads the default profile; an `org:` feed
            // may have resolved through a different one, and then the rate limiter
            // and the access log would both be attributing to the wrong client.
            //
            // dnf sends ONE User-Agent for every feed in a run, so with two `org:`
            // feeds minted under different credentials there is no honest single
            // answer. Claiming the last one attributes the other's traffic to a
            // credential that never made it, so a disagreement clears the field
            // and the header falls back to the default profile.
            match &self_key_id {
                None if !key_seen => self_key_id = Some(key_id.clone()),
                Some(seen) if seen == &key_id => {}
                _ => self_key_id = None,
            }
            key_seen = true;
            feed.credential = Some((key_id, minted.token));
            if let Some(t) = minted.tier {
                lowest = Some(lowest.map_or(t, |l: u32| l.min(t)));
            }
        }
        self.minted_tier = lowest;
        self.minted_key_id = self_key_id;
        Ok(lowest)
    }

    /// The feed set as a lockfile record, in priority order.
    ///
    /// The whole set, not the stage-filtered view: `stages:` decides which feeds a
    /// given step sees, but the lock answers "what was this target resolved
    /// against", so each feed carries its own stage list instead.
    ///
    /// Built-in re-scopes are left out. They are served by the `.repo` files baked
    /// into the SDK image and have a repoid glob rather than a URL, so recording
    /// one as a source would describe something that is not one. Their effect on
    /// resolution is already in the stamp projection.
    pub fn locked_feeds(&self) -> Vec<crate::utils::lockfile::LockedFeed> {
        self.feeds
            .iter()
            .filter(|f| f.kind != FeedKind::Builtin)
            .map(|f| crate::utils::lockfile::LockedFeed {
                name: f.name.clone(),
                position: f.priority,
                url: f.configured_locator.clone(),
                digest: f.content_digest.clone(),
                stages: if f.stages.is_empty() {
                    None
                } else {
                    Some(f.stages.iter().map(|s| s.to_string()).collect())
                },
            })
            .collect()
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
    /// Materialize into a fresh throwaway root. Tests want isolation; production
    /// deliberately shares one root per invocation (see `materialize_in`).
    #[cfg(test)]
    pub fn materialize(&self, stage: FeedStage) -> Result<FeedMaterialization> {
        let root = Arc::new(
            tempfile::Builder::new()
                .prefix("avocado-feeds-test-")
                .tempdir()
                .context("creating a test feeds dir")?,
        );
        self.materialize_in(stage, &root)
    }

    /// Write this stage's `.repo` files under `root`, and describe the mount and
    /// environment a container needs to see them.
    ///
    /// `root` is one directory per **invocation**, not per step, and it is the
    /// mount for every container the invocation starts. That matters for more
    /// than tidiness: the mount list is part of a container's shape, so a
    /// per-step directory would give every dnf step a unique shape and defeat
    /// container reuse exactly where it is most valuable. Each stage gets its own
    /// subdirectory and `AVOCADO_FEEDS_DIR` selects it, so the shape is identical
    /// across steps while dnf still sees only the feeds scoped to its stage.
    ///
    /// The trade this makes, deliberately: every step in an invocation can read
    /// every stage's credentials from the mount, where a per-step directory
    /// showed each step only its own. For a developer building their own project
    /// that is the same trust boundary as the source tree already being executed.
    /// It would not be acceptable if extension builds ever run untrusted code in
    /// that container, which is the condition that would invalidate this.
    pub fn materialize_in(
        &self,
        stage: FeedStage,
        root: &Arc<tempfile::TempDir>,
    ) -> Result<FeedMaterialization> {
        let root = root.clone();
        let root_path = root.path();
        let stage_dir = root_path.join(stage.dir_name());
        // Both scope dirs always exist: the entrypoint appends them to the dnf
        // reposdir lists unconditionally whenever AVOCADO_FEEDS_DIR is set.
        let scope = if stage.is_host() { "host" } else { "target" };
        fs::create_dir_all(stage_dir.join("host"))?;
        fs::create_dir_all(stage_dir.join("target"))?;
        let dir = stage_dir.join(scope);

        let container_stage_dir = format!("{CONTAINER_FEEDS_DIR}/{}", stage.dir_name());
        let mut mounts = vec![(root_path.to_path_buf(), CONTAINER_FEEDS_DIR.to_string())];
        let mut env = vec![("AVOCADO_FEEDS_DIR".to_string(), container_stage_dir.clone())];
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
                FeedKind::Connect => {
                    // Checked only for a feed this stage will actually write. One
                    // scoped away from this stage is never materialized and is
                    // deliberately never minted, so demanding a token for it would
                    // make `stages:` weaker for a private feed than for any other.
                    //
                    // For a feed that IS in scope, a missing token is a bug rather
                    // than a user error: minting runs first and fails loudly. A
                    // .repo pointing at `connect://<org>` would simply fail to
                    // resolve, reading as a broken feed rather than a broken CLI.
                    if feed.applies_to(stage) && feed.credential.is_none() {
                        bail!(
                            "repos.{}: no feed token was issued before materialization \
                             (internal error: resolve_connect_credentials did not run)",
                            feed.name
                        );
                    }
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
                    Some(format!("{container_stage_dir}/{scope}/{fname}"))
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
                // At the root, not under the stage dir: a path feed's bind is the
                // same for every stage, so keeping it out of the per-stage tree
                // keeps the mount list identical across steps.
                fs::create_dir_all(root_path.join("paths").join(&feed.name))?;
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
            tier: self.minted_tier,
            key_id: self.minted_key_id.clone(),
            _tempdir: root,
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
pub fn url_has_userinfo(url: &str) -> bool {
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

    /// dnf sends one User-Agent for every feed in a run, so two `org:` feeds
    /// minted under different credentials have no honest single identity, and two
    /// different tiers have no honest single claim. The header must not attribute
    /// one feed's traffic to the other's credential, and must not ask for a
    /// ceiling a feed was never granted.
    #[test]
    fn a_single_header_never_over_claims_across_feeds() {
        // Same credential and tier: the header can speak for both.
        assert!(user_agent_for(Some("abc"), Some(3)).contains("tier/3"));
        // No minted tier at all keeps the authenticated floor rather than
        // inventing one.
        assert!(user_agent_for(Some("abc"), None).contains("tier/1"));
        // The identity is the credential's, not the default profile's.
        assert!(user_agent_for(Some("abc"), Some(2)).contains("key/abc"));
    }

    /// `org` reaches a URL and the generated .repo, so it has to be one URL-safe
    /// segment. A slash reshapes the request path; a newline injects a line into
    /// the .repo.
    #[test]
    fn org_must_be_a_single_url_safe_segment() {
        for bad in ["a/b", "a b", "a\nb", "../x", ""] {
            let c = load(&format!(
                "{BASE}  feeds: [f]\nrepos:\n  f:\n    org: {:?}\n",
                bad
            ));
            assert!(
                ResolvedFeedSet::resolve(&c, "t", Path::new("."), None).is_err(),
                "org {bad:?} should be rejected"
            );
        }
    }

    /// An `org:` feed resolves without contacting anything: a placeholder URL that
    /// shows the layout, and the org as the credential identity. Both are stable
    /// across builds, which is what keeps a per-build token out of the stamp hash.
    #[test]
    fn org_feed_resolves_to_a_placeholder_and_records_the_org() {
        let c = load(&format!(
            "{BASE}  feeds: [acme]\nrepos:\n  acme:\n    org: 01a071ea\n    channel: main\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."), None)
            .unwrap()
            .unwrap();
        let feed = set.feeds.iter().find(|f| f.name == "acme").unwrap();
        assert_eq!(feed.kind, FeedKind::Connect);
        assert_eq!(feed.credential_identity, "connect:01a071ea");
        // release before org, mirroring the public tree — see edge-contract.md S3.
        assert_eq!(
            feed.baseurl,
            "connect://01a071ea/2026/orgs/01a071ea/main/target/qemux86-64"
        );
        assert!(feed.credential.is_none(), "no token before the mint runs");
    }

    /// The canonical document is written before the mint and must never carry a
    /// token, a host, or anything else that changes per build.
    #[test]
    fn org_feed_canonical_document_carries_no_secret() {
        let c = load(&format!(
            "{BASE}  feeds: [acme]\nrepos:\n  acme:\n    org: acme\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "qemux86-64", Path::new("."), None)
            .unwrap()
            .unwrap();
        let doc = set.canonical_json().unwrap();
        assert!(
            doc.contains("\"connect:acme\""),
            "records the org identity: {doc}"
        );
        assert!(
            doc.contains("connect://acme/"),
            "records the placeholder: {doc}"
        );
        for leak in ["token", "Bearer", "password", "eyJ"] {
            assert!(
                !doc.contains(leak),
                "canonical document leaked {leak:?}: {doc}"
            );
        }
    }

    /// Defaults: no `channel:` means the org's `main` branch, and the release
    /// falls back to the distro's.
    #[test]
    fn org_feed_defaults_to_the_main_branch() {
        let c = load(&format!("{BASE}  feeds: [a]\nrepos:\n  a:\n    org: o\n"));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        let feed = set.feeds.iter().find(|f| f.name == "a").unwrap();
        assert_eq!(feed.baseurl, "connect://o/2026/orgs/o/main/target/t");
    }

    /// An `org:` feed with its own credential would skip minting and keep the
    /// `connect://` placeholder as its baseurl, so dnf would fail to resolve a
    /// config that looks perfectly reasonable.
    #[test]
    fn an_org_feed_cannot_carry_its_own_credential() {
        for extra in ["username: u\n    password: p", "password: p", "username: u"] {
            let c = load(&format!(
                "{BASE}  feeds: [p]\nrepos:\n  p:\n    org: o\n    {extra}\n"
            ));
            let err = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("credential from Connect"),
                "{extra:?} should be refused: {err}"
            );
        }
    }

    /// `channel` is the branch segment of a Connect feed path, so it needs the
    /// same rule as the org: a slash or a space makes a malformed path, and the
    /// server parses those segments.
    #[test]
    fn an_org_feeds_branch_must_be_a_single_segment() {
        for bad in ["a/b", "a b", "a\nb", ".."] {
            let c = load(&format!(
                "{BASE}  feeds: [p]\nrepos:\n  p:\n    org: o\n    channel: {:?}\n",
                bad
            ));
            assert!(
                ResolvedFeedSet::resolve(&c, "t", Path::new("."), None).is_err(),
                "channel {bad:?} should be rejected"
            );
        }
        let ok = load(&format!(
            "{BASE}  feeds: [p]\nrepos:\n  p:\n    org: o\n    channel: main\n"
        ));
        assert!(ResolvedFeedSet::resolve(&ok, "t", Path::new("."), None).is_ok());
    }

    /// `tier/1` is the authenticated floor. A mint returning 0 must not drop an
    /// authenticated client into the anonymous bucket.
    #[test]
    fn the_authenticated_tier_floor_holds() {
        assert!(user_agent_for(Some("k"), Some(0)).contains("tier/1"));
        assert!(user_agent_for(Some("k"), None).contains("tier/1"));
        assert!(user_agent_for(Some("k"), Some(4)).contains("tier/4"));
    }

    /// The lock records the source as configured, not the container's view of it.
    /// A loopback URL is rewritten to `host.docker.internal` for reachability and
    /// a `path:` feed becomes a `file://` path inside the mount — both are
    /// plumbing. Recording those would put an ephemeral port and an internal
    /// mount path into a file people commit and diff.
    #[test]
    fn the_lock_records_the_configured_source_not_the_container_view() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("feed/repodata")).unwrap();
        std::fs::write(dir.path().join("feed/repodata/repomd.xml"), b"x").unwrap();
        let c = load(&format!(
            "{BASE}  feeds: [ondisk, local]\nrepos:\n  ondisk:\n    path: ./feed\n  local:\n    url: http://localhost:8080\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", dir.path(), None)
            .unwrap()
            .unwrap();
        let locked = set.locked_feeds();
        let by = |n: &str| locked.iter().find(|f| f.name == n).unwrap();

        assert_eq!(by("ondisk").url, "./feed", "as written, not the mount path");
        assert!(
            by("ondisk").digest.is_some(),
            "an on-disk feed is the one case content can be pinned"
        );
        assert_eq!(
            by("local").url,
            "http://localhost:8080",
            "the user's URL, not the loopback rewrite"
        );
        // The container still sees the rewritten form — the two views differ, and
        // that is the point.
        let feed = set.feeds.iter().find(|f| f.name == "local").unwrap();
        assert!(feed.baseurl.contains("host.docker.internal"));

        // Order is recorded, because order is a strict priority override.
        assert!(by("ondisk").position < by("local").position);
        // Built-ins are not sources and are left out.
        assert!(!locked.iter().any(|f| f.name == BUILTIN_EXT_FEED));
    }

    /// A private feed scoped away from a stage must not require a token there.
    /// Otherwise `stages:` means less for an `org:` feed than for any other kind:
    /// the command would demand a login and a network round trip for a feed it is
    /// never going to write.
    #[test]
    fn a_connect_feed_out_of_scope_needs_no_token() {
        let c = load(&format!(
            "{BASE}  feeds: [priv]\nrepos:\n  priv:\n    org: o\n    stages: [ext]\n"
        ));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        // sdk is out of scope: materializes fine with no token at all.
        assert!(set.materialize(FeedStage::Sdk).is_ok());
        // ext is in scope, so a missing token there is still the internal error.
        let err = set.materialize(FeedStage::Ext).unwrap_err().to_string();
        assert!(err.contains("no feed token was issued"), "got: {err}");
    }

    /// Materializing a Connect feed that never got a token is an internal error,
    /// not a silently broken `.repo`: dnf would report "no more mirrors" and the
    /// cause would look like a broken feed rather than a CLI bug.
    #[test]
    fn materialize_refuses_a_connect_feed_without_a_token() {
        let c = load(&format!("{BASE}  feeds: [a]\nrepos:\n  a:\n    org: o\n"));
        let set = ResolvedFeedSet::resolve(&c, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        let err = set.materialize(FeedStage::Sdk).unwrap_err().to_string();
        assert!(
            err.contains("no feed token was issued"),
            "expected the internal-error message, got: {err}"
        );
    }

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
        assert!(ext.mounts[0]
            .0
            .join("ext/target/avocado-feed-v.repo")
            .is_file());
        let rootfs = set.materialize(FeedStage::Rootfs).unwrap();
        assert_eq!(rootfs.dnf_args, vec!["--disablerepo=*-target-ext"]);
        assert!(!rootfs.mounts[0]
            .0
            .join("target/avocado-feed-v.repo")
            .exists());
        let sdk = set.materialize(FeedStage::Sdk).unwrap();
        assert!(!sdk.mounts[0]
            .0
            .join("sdk/host/avocado-feed-v.repo")
            .exists());
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
        let repo =
            fs::read_to_string(m.mounts[0].0.join("rootfs/target/avocado-feed-n.repo")).unwrap();
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
        // `org:` is supported now, but it is still exactly one locator: naming a
        // url alongside it is ambiguous about who decides the host, and the mint
        // is the answer.
        let both = load(&format!(
            "{BASE}repos:\n  acme:\n    org: acme\n    url: https://elsewhere\n"
        ));
        assert!(ResolvedFeedSet::resolve(&both, "t", Path::new("."), None)
            .unwrap_err()
            .to_string()
            .contains("exactly one of"));
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

        // The distro URL is a base: the releasever is appended and the baked
        // .repo files add the rest. A `$releasever` or `$target` copied from a
        // `repos:` example is never expanded here, so it would survive into the
        // baseurl and fail as a 404 rather than as a config error.
        for var in ["$releasever", "$target"] {
            let e = err(&format!(
                "distro:\n  release: 2026\n  channel: next\n  repo: m\nrepos:\n  m:\n    url: https://m/{var}\n"
            ));
            assert!(e.contains("base URL") && e.contains(var), "{e}");
        }
        // And a named feed still requires the placeholder, so the two rules do
        // not quietly contradict each other.
        let named = load(
            "distro:\n  release: 2026\n  channel: next\n  feeds: [v]\nrepos:\n  v:\n    url: https://v/$releasever\n",
        );
        let set = ResolvedFeedSet::resolve(&named, "t", Path::new("."), None)
            .unwrap()
            .unwrap();
        assert!(
            set.feeds.iter().any(|f| f.baseurl == "https://v/2026/next"),
            "a named feed still expands the placeholder"
        );
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
            fs::read_to_string(
                m.mounts[0]
                    .0
                    .join(format!("rootfs/target/avocado-feed-{n}.repo")),
            )
            .unwrap()
        };
        let signed = read("signed");
        assert!(
            signed.contains("baseurl=https://s/t\n"),
            "$target expands: {signed}"
        );
        assert!(signed.contains("gpgcheck=1\n") && signed.contains("gpgkey=https://s/KEY\n"));
        assert!(signed
            .contains("sslcacert=/run/avocado-feeds/rootfs/target/avocado-feed-signed.ca.pem\n"));
        assert!(signed.contains("sslverify=0\n"));
        assert!(m.mounts[0]
            .0
            .join("rootfs/target/avocado-feed-signed.ca.pem")
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
