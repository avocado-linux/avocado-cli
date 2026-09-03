//! Extension fetching utilities for remote extensions.
//!
//! This module provides functionality to fetch extensions from various sources:
//! - Package repository (avocado extension repo)
//! - Git repositories (with optional sparse checkout)
//! - Local filesystem paths (mounted via bindfs at runtime)

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::utils::config::ExtensionSource;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, OutputLevel};

/// One package-source extension to materialize in a batched fetch.
///
/// `package_spec` is the fully-resolved NEVRA (or bare name for `version: '*'`)
/// the caller wants installed. It is passed to dnf explicitly so the depsolve
/// cannot substitute a different version than the lock file pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFetchEntry {
    /// Extension name — also the directory it lands in under `includes/`.
    pub ext_name: String,
    /// RPM package name. Usually the extension name, but `source.package` can
    /// rename it. Kept separate from `package_spec` because the layout query
    /// matches on `%{name}` and a spec cannot be split back into name and
    /// version unambiguously.
    pub package_name: String,
    /// Package spec handed to dnf (`<package_name>-<version>`, or just the name).
    pub package_spec: String,
    /// Optional `--repo=` restriction from `source.repo_name`.
    pub repo_name: Option<String>,
    /// This extension's DECLARED `depends_on` names (interpolated). Used only
    /// to order repo-restricted transaction groups so a provider declared in
    /// another repo installs first — a `--repo=` restriction hides every other
    /// repo from the depsolver, but an already-installed provide still
    /// satisfies a later restricted transaction. Empty for lock-pinned
    /// replays (their dependencies are pinned entries themselves).
    pub depends_on: Vec<String>,
}

/// Reject values that would be unsafe or ambiguous once interpolated into the
/// fetch shell script.
///
/// The script builds `<ext>|<pkg>|<spec>` triples and splits them in shell, so
/// a value containing whitespace, a quote, `|`, or a shell metacharacter would
/// at best corrupt the split and at worst execute. Validating is better than
/// escaping here: these are RPM package names and directory names under
/// `includes/`, so none of those characters is ever legitimate — a value
/// carrying one is a config error worth reporting, not something to quote and
/// pass through.
fn validate_shell_safe(field: &str, value: &str) -> Result<()> {
    validate_shell_safe_chars(field, value, false)
}

/// Like [`validate_shell_safe`], but permits `~`.
///
/// `~` is part of the canonical RPM version for a pre-release —
/// `to_rpm_version` turns `1.0.0-rc.1` into `1.0.0~rc.1`, and the rpmdb
/// reports that form back into the lock — so a resolved pre-release spec like
/// `app-1.0.0~rc.1-r0` must replay on the next fetch. It is shell-inert here:
/// the spec sits inside the generated quoted triple, is never re-evaluated
/// after parameter expansion, and tilde-expansion only ever applies at an
/// unquoted word start. Names and repo ids keep rejecting it — an RPM *name*
/// never legitimately carries one.
fn validate_shell_safe_spec(field: &str, value: &str) -> Result<()> {
    validate_shell_safe_chars(field, value, true)
}

fn validate_shell_safe_chars(field: &str, value: &str, allow_tilde: bool) -> Result<()> {
    const FORBIDDEN: &[char] = &[
        '|', '\'', '"', '`', '$', '\\', ';', '&', '<', '>', '(', ')', '{', '}', '[', ']', '*', '?',
        '!', '#', '~', '/', '\n', '\r', '\t',
    ];
    if value.is_empty() {
        anyhow::bail!("Extension {field} is empty.");
    }
    // `/` is in FORBIDDEN (an ext name reaches `includes/<name>`, and a path
    // separator would escape it), so `..` can only appear as the whole value —
    // where it walks OUT of includes/, which `--force` then rm -rf's.
    if value == "." || value == ".." {
        anyhow::bail!("Extension {field} '{value}' is a path component, not a name.");
    }
    // A leading `-` reads as an option once expanded into the dnf/rpm command
    // line inside the generated script.
    if value.starts_with('-') {
        anyhow::bail!(
            "Extension {field} '{value}' starts with '-', which would be read \
             as a command-line option."
        );
    }
    if let Some(bad) = value
        .chars()
        .find(|c| (c.is_whitespace() || FORBIDDEN.contains(c)) && !(allow_tilde && *c == '~'))
    {
        anyhow::bail!(
            "Extension {field} '{value}' contains the character {bad:?}, which is not \
             valid in an RPM package name or an extension directory name."
        );
    }
    Ok(())
}

impl PackageFetchEntry {
    /// Check every value that reaches the fetch script.
    ///
    /// Called once per entry before the script is assembled, so a bad name is
    /// reported by name rather than producing a confusing shell error inside
    /// the container.
    fn validate(&self) -> Result<()> {
        validate_shell_safe("name", &self.ext_name)?;
        validate_shell_safe("package name", &self.package_name)?;
        validate_shell_safe_spec("package spec", &self.package_spec)?;
        if let Some(repo) = &self.repo_name {
            validate_shell_safe("repo name", repo)?;
        }
        Ok(())
    }

    /// Build an entry from a `source: { type: package }` declaration.
    ///
    /// `package` overrides the RPM name when the extension is published under
    /// a different one; otherwise the extension name is the package name.
    /// `version: '*'` means "whatever the feed offers" and produces a bare
    /// name, letting the depsolver choose.
    pub fn from_package_source(
        ext_name: &str,
        package: Option<&str>,
        version: &str,
        repo_name: Option<&str>,
    ) -> Self {
        let package_name = package.unwrap_or(ext_name);
        let package_spec = if version == "*" {
            package_name.to_string()
        } else {
            // `version` must arrive in RPM form. A lock-replayed value is
            // (VERSION-RELEASE from the rpmdb); a config-declared semver is
            // converted by the caller BEFORE it reaches here — converting
            // in this constructor would also mangle lock NEVRAs, whose
            // `-r0` release suffix parses as a semver pre-release.
            format!("{package_name}-{version}")
        };
        Self {
            depends_on: Vec::new(),
            ext_name: ext_name.to_string(),
            package_name: package_name.to_string(),
            package_spec,
            repo_name: repo_name.map(str::to_string),
        }
    }

    /// Attach the declared dependency names (see the field docs).
    pub fn with_depends_on(mut self, deps: Vec<String>) -> Self {
        self.depends_on = deps;
        self
    }
}

/// Delimiters around the post-install rpmdb report, so it can be picked out of
/// dnf's own chatter on stdout.
const INSTALLED_REPORT_BEGIN: &str = "---avocado-installed-extensions-begin---";
const INSTALLED_REPORT_END: &str = "---avocado-installed-extensions-end---";

/// One package from the installed report: its resolved version and, when it
/// carries one, the extension name from its `avocado-ext(<name>)` provide.
///
/// The provide is the authoritative extension identity. A publisher can
/// rename the RPM via `source.package` — that rename is the whole reason the
/// virtual capability exists — so recording the RPM name as the extension
/// name would make the lock entry unaddressable by the graph/stamp code and
/// replay it into the wrong `includes/<name>` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub version: String,
    pub ext_name: Option<String>,
    /// Extension names this package Requires via `avocado-ext(<name>)` —
    /// the solver-visible edges. Used to tell a live implied dependency
    /// (something installed still requires it) from a leftover whose edge
    /// was removed, which must age out of the lock rather than replay
    /// forever.
    pub requires_exts: Vec<String>,
}

/// Parse the `NAME VERSION-RELEASE [provides…]` block emitted after a batched
/// install.
///
/// Returns package name -> resolved version + extension identity. Anything
/// outside the delimiters is dnf output and ignored.
fn parse_installed_report(stdout: &str) -> std::collections::HashMap<String, InstalledPackage> {
    let mut out = std::collections::HashMap::new();
    let mut inside = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line == INSTALLED_REPORT_BEGIN {
            inside = true;
            continue;
        }
        if line == INSTALLED_REPORT_END {
            break;
        }
        if !inside {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let mut ext_name = None;
        let mut requires_exts = Vec::new();
        for tok in parts {
            if let Some(cap) = tok
                .strip_prefix("p:avocado-ext(")
                .and_then(|s| s.strip_suffix(')'))
            {
                ext_name.get_or_insert_with(|| cap.to_string());
            } else if let Some(cap) = tok
                .strip_prefix("r:avocado-ext(")
                .and_then(|s| s.strip_suffix(')'))
            {
                requires_exts.push(cap.to_string());
            }
        }
        out.insert(
            name.to_string(),
            InstalledPackage {
                version: version.to_string(),
                ext_name,
                requires_exts,
            },
        );
    }
    out
}

/// Order repo-restricted transaction groups so declared cross-repo providers
/// install first.
///
/// `--repo=` restricts the WHOLE transaction, so if an app in repo A requires
/// a base declared in repo B and A's group ran first, the depsolver could not
/// see B's provider and the transaction failed — but only in one declaration
/// order, which is the worst kind of bug. An already-installed provide
/// satisfies a later restricted transaction, so running B first is
/// sufficient. First-seen order is the tiebreak; on a cycle the remaining
/// groups keep their original order (a cross-repo dependency cycle cannot be
/// satisfied under restrictions in any order, and dnf's own
/// unsatisfied-Requires error names the missing capability).
///
/// Only DECLARED `depends_on` edges are visible here. An *implied* cross-repo
/// dependency under a repo restriction remains unresolvable by ordering —
/// nothing names it before the depsolver runs.
fn order_repo_groups(
    entries: &[PackageFetchEntry],
    repos: Vec<Option<String>>,
) -> Vec<Option<String>> {
    let repo_of: std::collections::HashMap<&str, &Option<String>> = entries
        .iter()
        .map(|e| (e.ext_name.as_str(), &e.repo_name))
        .collect();
    let needs = |a: &Option<String>, b: &Option<String>| -> bool {
        entries
            .iter()
            .filter(|e| &e.repo_name == a)
            .flat_map(|e| e.depends_on.iter())
            .any(|dep| repo_of.get(dep.as_str()).map(|r| *r == b).unwrap_or(false))
    };
    let mut ordered: Vec<Option<String>> = Vec::new();
    let mut remaining = repos;
    while !remaining.is_empty() {
        let pick = remaining
            .iter()
            .position(|candidate| {
                !remaining
                    .iter()
                    .any(|other| other != candidate && needs(candidate, other))
            })
            .unwrap_or(0); // cycle: keep original order
        ordered.push(remaining.remove(pick));
    }
    ordered
}

/// Extension fetcher for downloading and installing remote extensions
pub struct ExtensionFetcher {
    /// Path to the main configuration file
    config_path: String,
    /// Target architecture
    target: String,
    /// Enable verbose output
    verbose: bool,
    /// Container image for running fetch operations
    container_image: String,
    /// Repository URL for package fetching
    repo_url: Option<String>,
    /// Repository release for package fetching
    repo_release: Option<String>,
    /// Container arguments
    container_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    sdk_arch: Option<String>,
    /// Source directory for resolving relative extension paths
    src_dir: Option<PathBuf>,
}

impl ExtensionFetcher {
    /// Create a new ExtensionFetcher
    pub fn new(
        config_path: String,
        target: String,
        container_image: String,
        verbose: bool,
    ) -> Self {
        Self {
            config_path,
            target,
            verbose,
            container_image,
            repo_url: None,
            repo_release: None,
            container_args: None,
            sdk_arch: None,
            src_dir: None,
        }
    }

    /// Set repository URL
    pub fn with_repo_url(mut self, repo_url: Option<String>) -> Self {
        self.repo_url = repo_url;
        self
    }

    /// Set repository release
    pub fn with_repo_release(mut self, repo_release: Option<String>) -> Self {
        self.repo_release = repo_release;
        self
    }

    /// Set container arguments
    pub fn with_container_args(mut self, container_args: Option<Vec<String>>) -> Self {
        self.container_args = container_args;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set source directory for resolving relative extension paths
    pub fn with_src_dir(mut self, src_dir: Option<PathBuf>) -> Self {
        self.src_dir = src_dir;
        self
    }

    /// Fetch an extension based on its source configuration
    ///
    /// Returns the path where the extension was installed
    pub async fn fetch(
        &self,
        ext_name: &str,
        source: &ExtensionSource,
        install_dir: &Path,
        force: bool,
    ) -> Result<PathBuf> {
        let ext_install_path = install_dir.join(ext_name);

        match source {
            ExtensionSource::Package {
                version,
                package,
                repo_name,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_repo(
                    ext_name,
                    version,
                    package.as_deref(),
                    repo_name.as_deref(),
                    &ext_install_path,
                    force,
                )
                .await?;
            }
            ExtensionSource::Git {
                url,
                git_ref,
                sparse_checkout,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_git(
                    ext_name,
                    url,
                    git_ref.as_deref(),
                    sparse_checkout.as_deref(),
                    &ext_install_path,
                )
                .await?;
            }
            ExtensionSource::Path { path, .. } => {
                self.fetch_from_path(ext_name, path, &ext_install_path)
                    .await?;
            }
        }

        Ok(ext_install_path)
    }

    /// Fetch several package-source extensions in a single DNF transaction.
    ///
    /// This is the path that makes inter-extension dependencies work. Because
    /// nested packages all install into the one shared `includes` installroot,
    /// a single `dnf install a b c` lets the **depsolver** pull in each
    /// extension's `Requires: avocado-ext(<dep>)` closure — dependencies are
    /// discovered, downloaded, and installed in one transaction rather than
    /// through repeated fetch/recompose/discover rounds. It also collapses N
    /// container spin-ups into one.
    ///
    /// Every requested extension is passed explicitly with its locked NEVRA, so
    /// the depsolve cannot drift off the lock file and a consumer's declared
    /// version always beats whatever range a dependent's `Requires:` allows.
    ///
    /// Layout is still detected per package, in-script: only packages providing
    /// `avocado-ext-layout(nested)` may share the installroot. Legacy packages
    /// (content at `/`) are installed one at a time into their own
    /// per-extension installroot exactly as before — they cannot be batched
    /// without colliding.
    /// A `--repo=` restriction applies to the whole transaction, so entries are
    /// grouped by `repo_name` and each group gets its own transaction. In the
    /// overwhelmingly common case every entry has `repo_name: None` and this is
    /// a single group.
    pub async fn fetch_packages(
        &self,
        entries: &[PackageFetchEntry],
        force: bool,
    ) -> Result<std::collections::HashMap<String, InstalledPackage>> {
        let mut resolved = std::collections::HashMap::new();
        let mut repos: Vec<Option<String>> = Vec::new();
        for e in entries {
            if !repos.contains(&e.repo_name) {
                repos.push(e.repo_name.clone());
            }
        }

        let ordered = order_repo_groups(entries, repos);

        for repo in ordered {
            let group: Vec<PackageFetchEntry> = entries
                .iter()
                .filter(|e| e.repo_name == repo)
                .cloned()
                .collect();
            resolved.extend(
                self.fetch_package_group(&group, repo.as_deref(), force)
                    .await?,
            );
        }

        Ok(resolved)
    }

    async fn fetch_package_group(
        &self,
        entries: &[PackageFetchEntry],
        repo_name: Option<&str>,
        force: bool,
    ) -> Result<std::collections::HashMap<String, InstalledPackage>> {
        if entries.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Everything below is interpolated into a shell script. Check it here,
        // once, rather than trusting that config values are well-formed.
        for entry in entries {
            entry.validate()?;
        }

        if self.verbose {
            print_info(
                &format!(
                    "Fetching {} package extension(s) in one transaction: {}",
                    entries.len(),
                    entries
                        .iter()
                        .map(|e| e.ext_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                OutputLevel::Normal,
            );
        }

        // `<ext_name>|<package_name>|<package_spec>` triples. The package name
        // is carried separately rather than parsed back out of the spec:
        // `foo-1.2.0` cannot be split into name and version unambiguously in
        // shell, and the layout query below matches on `%{name}`.
        let triples = entries
            .iter()
            .map(|e| format!("\"{}|{}|{}\"", e.ext_name, e.package_name, e.package_spec))
            .collect::<Vec<_>>()
            .join(" ");
        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();
        let force_str = if force { "true" } else { "false" };

        let script = format!(
            r#"
set -e

DNF="RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX"

# Layout detection for the WHOLE set in one query. Asking per package meant one
# dnf metadata load each, which dominated the fetch: five extensions paid five
# full repo loads just to learn where to put them. `--whatprovides` answers it
# for every package at once, still from repo metadata with no payload download.
# `name nvr` pairs, so the per-triple check below can match the SELECTED
# package rather than any version of the name: a locked legacy version and a
# newer nested-layout version can share a name, and keying on the name alone
# would route the locked legacy payload into the shared root, landing its
# content at / instead of includes/<ext>.
NESTED_PROBE=$(RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
    $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
    repoquery --whatprovides 'avocado-ext-layout(nested)' --qf '%{{name}} %{{name}}-%{{version}}-%{{release}}\n' 2>/dev/null | sort -u)

NESTED_SPECS=""
NESTED_NAMES_TO_REMOVE=""
NESTED_DIRS=""
LEGACY_REPORT=""

for triple in {triples}; do
    ext_name="${{triple%%|*}}"
    rest="${{triple#*|}}"
    pkg_name="${{rest%%|*}}"
    spec="${{rest#*|}}"
    ext_dir="$AVOCADO_PREFIX/includes/$ext_name"

    # A wildcard spec matches on name: any nested-providing version routes it
    # to the shared root — dnf selects the newest, and layout migrations run
    # legacy->nested, so the newest is the nested one. A pinned spec must
    # match its own NVR: exact for a full name-version-release, prefix for
    # name-version.
    if printf '%s\n' "$NESTED_PROBE" | awk -v p="$pkg_name" -v s="$spec" \
        '($1 == p && s == p) || $2 == s || index($2, s "-") == 1 {{ found = 1 }} END {{ exit !found }}'; then
        # Accumulate only — every dnf call for the nested set is batched below.
        NESTED_SPECS="$NESTED_SPECS $spec"
        NESTED_NAMES_TO_REMOVE="$NESTED_NAMES_TO_REMOVE $pkg_name"
        NESTED_DIRS="$NESTED_DIRS $ext_dir"
    else
        # Legacy layout: content at /, so it needs its own installroot and
        # cannot join the shared transaction. Its installroot is recorded so
        # the report below can query it — the shared-includes query cannot see
        # it, and skipping it left the legacy entry's lock source unwritten
        # (`version: "*"` stayed unpinned forever).
        LEGACY_REPORT="$LEGACY_REPORT $ext_dir|$pkg_name"
        echo "Extension '$ext_name': legacy layout -> per-extension installroot"
        if [ "{force_str}" = "true" ]; then
            rm -rf "$ext_dir"
        fi
        mkdir -p "$ext_dir"
        RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
        RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
            --installroot="$ext_dir" -y install "$spec"
    fi
done

# --force: clear the whole nested set in ONE transaction. Removing per
# extension meant a full dnf metadata load each, which is what dominated a
# forced re-fetch — the install itself was already batched.
if [ "{force_str}" = "true" ] && [ -n "$NESTED_NAMES_TO_REMOVE" ]; then
    echo "Removing nested extensions for re-fetch:$NESTED_NAMES_TO_REMOVE"
    # rpm -e --nodeps, NOT dnf remove: dnf's dependency-aware removal drags
    # every installed package that Requires a removed provider out with it, so
    # `--force` on a subset (say, just a shared base) silently erased its
    # dependent apps — and only the subset was reinstalled below. --nodeps is
    # safe here precisely because the same packages are reinstalled in this
    # script run: the window where dependents' Requires dangle never survives
    # the transaction, and nothing executes from this rpmdb meanwhile.
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        rpm --root="$AVOCADO_PREFIX/includes" -e --nodeps $NESTED_NAMES_TO_REMOVE 2>/dev/null || true
    for d in $NESTED_DIRS; do rm -rf "$d"; done
fi

if [ -n "$NESTED_SPECS" ]; then
    echo "Installing nested extensions into shared includes installroot:$NESTED_SPECS"
    mkdir -p "$AVOCADO_PREFIX/includes"

    # One transaction: dnf resolves each package's `Requires: avocado-ext(...)`
    # and pulls in any dependency extensions not named explicitly here.
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
    RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
    $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
        --installroot="$AVOCADO_PREFIX/includes" -y install $NESTED_SPECS
fi

# Report what the installroot actually holds now. The requested spec is not
# the answer: `version: "*"` requests nothing in particular, and the depsolver
# may have pulled in extensions nobody named. Only the rpmdb knows the truth,
# and the lock needs the truth to be reproducible.
# The provides ride along so the host can read each package's
# `avocado-ext(<name>)` capability — the extension identity that survives a
# `source.package` rename. Legacy installroots are queried too: the shared
# root cannot see them, and an unreported package never gets its lock pinned.
echo "{INSTALLED_REPORT_BEGIN}"
rpm --root="$AVOCADO_PREFIX/includes" -qa \
    --queryformat '%{{NAME}} %{{VERSION}}-%{{RELEASE}} [p:%{{PROVIDENAME}} ][r:%{{REQUIRENAME}} ]\n' 2>/dev/null | sort
for pair in $LEGACY_REPORT; do
    legacy_root="${{pair%%|*}}"
    legacy_pkg="${{pair#*|}}"
    rpm --root="$legacy_root" -q "$legacy_pkg" \
        --queryformat '%{{NAME}} %{{VERSION}}-%{{RELEASE}} [p:%{{PROVIDENAME}} ][r:%{{REQUIRENAME}} ]\n' 2>/dev/null || true
done
echo "{INSTALLED_REPORT_END}"
"#
        );

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let out = container_helper
            .run_in_container_capture(run_config)
            .await?;
        if !out.success {
            return Err(anyhow::anyhow!(
                "Failed to fetch package extension(s): {}{}",
                entries
                    .iter()
                    .map(|e| e.ext_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                crate::utils::container::container_failure_detail(
                    &out.stderr,
                    crate::utils::container::STDERR_TAIL_LINES
                )
                .map(|d| format!("\n{d}"))
                .unwrap_or_default()
            ));
        }

        // The container's own progress output is suppressed by capture, so
        // echo it when the user asked to see it.
        if self.verbose && !out.stdout.is_empty() {
            print_info(out.stdout.trim_end(), OutputLevel::Normal);
        }

        Ok(parse_installed_report(&out.stdout))
    }

    /// Fetch an extension from the avocado package repository
    ///
    /// Installs the extension package into the SHARED `$AVOCADO_PREFIX/includes`
    /// installroot using DNF with `--installroot`. Packages nest their content under a
    /// top-level `/<ext_name>/` dir, so the content lands at `includes/<ext_name>/` and a
    /// single rpmdb tracks every installed extension (proper tracking, clean upgrades,
    /// version management, no cross-extension file collisions).
    #[allow(dead_code)]
    async fn fetch_from_repo(
        &self,
        ext_name: &str,
        version: &str,
        package: Option<&str>,
        repo_name: Option<&str>,
        _install_path: &Path, // Host path - not used, we use container path instead
        force: bool,
    ) -> Result<()> {
        // Use explicit package name if provided, otherwise fall back to extension name
        let package_name = package.unwrap_or(ext_name);

        if self.verbose {
            print_info(
                &format!(
                    "Fetching extension '{ext_name}' (package: '{package_name}') version '{version}' from package repository"
                ),
                OutputLevel::Normal,
            );
        }

        // Build the package spec using the package name (not extension name).
        //
        // dnf matches against the RPM's own `Version:`, which is the RPM form, so
        // a pre-release has to be asked for as `1.0.0~rc.1` even though config
        // (and the payload's avocado.yaml) spell it `1.0.0-rc.1`. Without this a
        // published pre-release extension is uninstallable from config: the
        // `repoquery --provides` layout probe and the install below both match
        // nothing. Idempotent for a version that's already in RPM form, which is
        // what the lockfile branch passes.
        let package_spec = if version == "*" {
            package_name.to_string()
        } else {
            let rpm_version = crate::utils::version::to_rpm_version(version)
                .with_context(|| format!("Cannot resolve extension '{ext_name}' from a repo"))?;
            format!("{package_name}-{rpm_version}")
        };

        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();

        // The package self-describes its layout via `Provides: avocado-ext-layout(nested)`.
        // - NESTED (new): content under /<ext_name>/ -> install into the SHARED includes
        //   installroot, so it lands at includes/<ext_name>/ with one rpmdb tracking all exts.
        // - LEGACY (no such provide): content at / -> per-extension installroot includes/<ext_name>.
        // Either way the final content is includes/<ext_name>/, so consumers are unchanged. The
        // installroot is chosen at run time by repoquerying the package's provides.
        let ext_dir = format!("$AVOCADO_PREFIX/includes/{ext_name}");
        let force_str = if force { "true" } else { "false" };

        // Install the extension package using DNF with --installroot
        // Uses $DNF_SDK_COMBINED_REPO_CONF to access both SDK and target-specific repos
        let fetch_script = format!(
            r#"
set -e

# Detect the package's on-disk layout from its provides (repo metadata, no download).
if RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
   $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
   repoquery --provides {package_spec} 2>/dev/null | grep -q 'avocado-ext-layout(nested)'; then
    INSTALLROOT="$AVOCADO_PREFIX/includes"
    echo "Extension '{ext_name}': nested layout -> shared includes installroot"
else
    INSTALLROOT="{ext_dir}"
    echo "Extension '{ext_name}': legacy layout -> per-extension installroot"
fi

# Force: remove just this extension (rpmdb entry + content dir) for a clean reinstall,
# without disturbing other extensions sharing the installroot.
if [ "{force_str}" = "true" ]; then
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS --installroot="$INSTALLROOT" -y remove {package_name} 2>/dev/null || true
    rm -rf "{ext_dir}"
fi

mkdir -p "$INSTALLROOT"

RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    {repo_arg} \
    --installroot="$INSTALLROOT" \
    -y \
    install \
    {package_spec}

echo "Successfully installed extension '{ext_name}' (package: {package_spec}) to {ext_dir}"
"#
        );

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: fetch_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from package repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a git repository
    async fn fetch_from_git(
        &self,
        ext_name: &str,
        url: &str,
        git_ref: Option<&str>,
        sparse_checkout: Option<&[String]>,
        _install_path: &Path, // Host path - not used, we use container path instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Fetching extension '{ext_name}' from git: {url}"),
                OutputLevel::Normal,
            );
        }

        // Use container path $AVOCADO_PREFIX/includes/<ext_name> instead of host path
        let container_install_path = format!("$AVOCADO_PREFIX/includes/{ext_name}");
        let ref_arg = git_ref.unwrap_or("HEAD");

        // Build the git clone command
        let git_cmd = if let Some(sparse_paths) = sparse_checkout {
            // Use sparse checkout for specific paths
            let sparse_paths_str = sparse_paths.join(" ");
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
mkdir -p "{container_install_path}"
cd "{container_install_path}"
git init
git remote add origin "{url}"
git config core.sparseCheckout true
echo "{sparse_paths_str}" | tr ' ' '\n' > .git/info/sparse-checkout
git fetch --depth 1 origin {ref_arg}
git checkout FETCH_HEAD
# Move sparse checkout contents to root if needed
if [ -d "{sparse_paths_str}" ]; then
    mv {sparse_paths_str}/* . 2>/dev/null || true
    rm -rf {sparse_paths_str}
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        } else {
            // Full clone
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
git clone --depth 1 --branch {ref_arg} "{url}" "{container_install_path}" || \
git clone --depth 1 "{url}" "{container_install_path}"
cd "{container_install_path}"
if [ "{ref_arg}" != "HEAD" ]; then
    git checkout {ref_arg} 2>/dev/null || true
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        };

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: git_cmd,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from git repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a local filesystem path
    ///
    /// Instead of copying files, this validates the path exists and stores the
    /// mapping for bindfs mounting at container runtime. The extension source
    /// will be mounted at `/mnt/ext/<ext_name>` and bindfs'd to
    /// `$AVOCADO_PREFIX/includes/<ext_name>`.
    async fn fetch_from_path(
        &self,
        ext_name: &str,
        source_path: &str,
        _install_path: &Path, // Host path - not used, we use bindfs mounting instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Registering extension '{ext_name}' from path: {source_path}"),
                OutputLevel::Normal,
            );
        }

        // Base for a relative source path: src_dir if set, else the config's
        // directory. Same rule as `SdkContainer::derive_ext_path_mounts`.
        let base_dir = self.src_dir.clone().unwrap_or_else(|| {
            Path::new(&self.config_path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });
        let resolved_source = if Path::new(source_path).is_absolute() {
            PathBuf::from(source_path)
        } else {
            base_dir.join(source_path)
        };

        // Canonicalize the path to get the absolute path
        let resolved_source = resolved_source.canonicalize().unwrap_or(resolved_source);

        if !resolved_source.exists() {
            return Err(anyhow::anyhow!(missing_path_source_message(
                source_path,
                &resolved_source,
                &base_dir,
                Path::new(&self.config_path),
            )));
        }

        // Check that the path contains an avocado.yaml or avocado.yml file
        let has_config = resolved_source.join("avocado.yaml").exists()
            || resolved_source.join("avocado.yml").exists();
        if !has_config {
            return Err(anyhow::anyhow!(
                "Extension source path does not contain an avocado.yaml or avocado.yml file: {}",
                resolved_source.display()
            ));
        }

        // No state file is written: `type: path` mounts are derived directly
        // from avocado.yaml at container launch (see
        // `SdkContainer::derive_ext_path_mounts`), so the config stays the
        // single source of truth and can't drift. `ext fetch` for a path
        // extension is now purely validation (the checks above).
        print_info(
            &format!(
                "Extension '{ext_name}' will be mounted via bindfs at runtime from: {}",
                resolved_source.display()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Check if an extension is already fetched/installed
    pub fn is_extension_installed(install_dir: &Path, ext_name: &str) -> bool {
        let ext_path = install_dir.join(ext_name);
        // Check if the directory exists and has an avocado config file
        ext_path.exists()
            && (ext_path.join("avocado.yaml").exists() || ext_path.join("avocado.yml").exists())
    }
}

/// Absolute form of `p` without requiring it to exist (`canonicalize` fails on
/// a missing path, which is exactly the case we report on).
fn absolutize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    // Drop the `.` components a config-dir fallback leaves behind, so the
    // reported path is one a user can copy into a shell.
    abs.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// A directory is usable as a `type: path` extension source only if it holds an
/// avocado config.
fn is_ext_source_dir(p: &Path) -> bool {
    p.join("avocado.yaml").is_file() || p.join("avocado.yml").is_file()
}

/// Message for a `type: path` extension whose source directory is missing.
///
/// What a relative `path:` is resolved against (`src_dir`, else the config
/// file's own directory) is invisible from the config, so name the base and
/// show the absolute result rather than echoing the path back. When the
/// directory does exist somewhere obvious under that base, point at it: a
/// wrong prefix (`extensions/foo` for a top-level `foo`) is the usual miss.
pub fn missing_path_source_message(
    source_path: &str,
    resolved: &Path,
    base_dir: &Path,
    config_path: &Path,
) -> String {
    let mut msg = format!(
        "source path '{source_path}' does not exist\n  \
         resolved to: {}\n  relative to: {}\n  declared in: {}",
        absolutize(resolved).display(),
        absolutize(base_dir).display(),
        absolutize(config_path).display(),
    );
    if let Some(hint) = Path::new(source_path)
        .file_name()
        .map(|name| ["", "extensions"].map(|prefix| base_dir.join(prefix).join(name)))
        .and_then(|candidates| {
            candidates
                .into_iter()
                .find(|c| c != resolved && is_ext_source_dir(c))
        })
    {
        msg.push_str(&format!(
            "\n  did you mean: {}",
            absolutize(&hint).display()
        ));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mistake this message exists for: the source lives at `<base>/foo`
    /// and the config says `extensions/foo`. The old text echoed the joined
    /// path and called the base "config directory" without saying where that
    /// was, which is what made this hard to get right.
    #[test]
    fn missing_path_source_names_the_base_and_suggests_the_real_dir() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("foo")).unwrap();
        std::fs::write(base.path().join("foo/avocado.yaml"), "").unwrap();

        let resolved = base.path().join("extensions/foo");
        let msg = missing_path_source_message(
            "extensions/foo",
            &resolved,
            base.path(),
            &base.path().join("avocado.yaml"),
        );

        assert!(msg.contains("'extensions/foo'"), "{msg}");
        assert!(msg.contains(&resolved.display().to_string()), "{msg}");
        assert!(
            msg.contains(&format!("relative to: {}", base.path().display())),
            "{msg}"
        );
        assert!(
            msg.contains(&format!(
                "did you mean: {}",
                base.path().join("foo").display()
            )),
            "{msg}"
        );
    }

    /// No suggestion beats a wrong one: a bare name with nothing matching it
    /// under the base must not invent a candidate.
    #[test]
    fn missing_path_source_suggests_nothing_when_no_candidate_exists() {
        let base = tempfile::tempdir().unwrap();
        let msg = missing_path_source_message(
            "nowhere",
            &base.path().join("nowhere"),
            base.path(),
            &base.path().join("avocado.yaml"),
        );
        assert!(!msg.contains("did you mean"), "{msg}");
    }

    // ---- post-install resolved-version report ---------------------------

    /// The declaration-order trap: app (repo A) depends on base (repo B).
    /// With A's restricted group first, dnf under --repo=A cannot see B's
    /// provider. Ordering must put the provider group first regardless of
    /// declaration order.
    #[test]
    fn repo_groups_order_providers_before_dependents() {
        let app = PackageFetchEntry::from_package_source("app", None, "1.0.0", Some("repo-a"))
            .with_depends_on(vec!["base".to_string()]);
        let base = PackageFetchEntry::from_package_source("base", None, "2.0.0", Some("repo-b"));
        let entries = vec![app, base];
        let repos = vec![Some("repo-a".to_string()), Some("repo-b".to_string())];
        assert_eq!(
            order_repo_groups(&entries, repos),
            vec![Some("repo-b".to_string()), Some("repo-a".to_string())],
            "the provider's group must run first"
        );
    }

    /// A dependency cycle across restricted repos cannot be satisfied in any
    /// order; the ordering must terminate and keep the original order rather
    /// than spin or panic.
    #[test]
    fn repo_group_cycles_keep_declaration_order() {
        let a = PackageFetchEntry::from_package_source("a", None, "1.0.0", Some("ra"))
            .with_depends_on(vec!["b".to_string()]);
        let b = PackageFetchEntry::from_package_source("b", None, "1.0.0", Some("rb"))
            .with_depends_on(vec!["a".to_string()]);
        let entries = vec![a, b];
        let repos = vec![Some("ra".to_string()), Some("rb".to_string())];
        assert_eq!(
            order_repo_groups(&entries, repos.clone()),
            repos,
            "a cycle falls back to declaration order"
        );
    }

    #[test]
    fn parses_the_delimited_report_and_ignores_dnf_chatter() {
        let stdout = format!(
            "Last metadata expiration check: 0:00:01 ago.\n\
             Installing:\n\
             \x20 avocado-ext-deptest-app-a  noarch  0.1.0-r0\n\
             Complete!\n\
             {INSTALLED_REPORT_BEGIN}\n\
             avocado-ext-deptest-app-a 0.1.0-r0 p:avocado-ext(deptest-app-a) p:avocado-ext-layout(nested) r:avocado-ext(deptest-base)\n\
             avocado-ext-deptest-base 1.2.0-r0 p:avocado-ext(deptest-base)\n\
             avocado-ext-deptest-mid 0.3.0-r0\n\
             {INSTALLED_REPORT_END}\n"
        );
        let got = parse_installed_report(&stdout);
        assert_eq!(got.len(), 3);
        // The avocado-ext(...) provide is the extension identity — it is what
        // survives a `source.package` rename, so the parser must surface it.
        assert_eq!(got["avocado-ext-deptest-base"].version, "1.2.0-r0");
        assert_eq!(
            got["avocado-ext-deptest-base"].ext_name.as_deref(),
            Some("deptest-base")
        );
        // The requires side surfaces the solver-visible edges, so the lock
        // can tell a live implied dependency from a removed-edge leftover.
        assert_eq!(
            got["avocado-ext-deptest-app-a"].requires_exts,
            vec!["deptest-base".to_string()]
        );
        assert!(got["avocado-ext-deptest-base"].requires_exts.is_empty());
        // A package with no avocado-ext provide (legacy) reports none.
        assert_eq!(got["avocado-ext-deptest-mid"].version, "0.3.0-r0");
        assert_eq!(got["avocado-ext-deptest-mid"].ext_name, None);
        // The "Installing:" table above must not be mistaken for the report.
        assert!(!got.contains_key("Installing:"));
    }

    #[test]
    fn report_absent_or_empty_yields_nothing_rather_than_garbage() {
        assert!(parse_installed_report("").is_empty());
        assert!(parse_installed_report("Complete!\nno markers here\n").is_empty());
        assert!(parse_installed_report(&format!(
            "{INSTALLED_REPORT_BEGIN}\n{INSTALLED_REPORT_END}\n"
        ))
        .is_empty());
    }

    #[test]
    fn report_tolerates_blank_and_malformed_lines() {
        let stdout = format!(
            "{INSTALLED_REPORT_BEGIN}\n\
             \n\
             no-version-here\n\
             good-pkg 1.0.0-r0\n\
             {INSTALLED_REPORT_END}\n"
        );
        let got = parse_installed_report(&stdout);
        assert_eq!(got.len(), 1);
        assert_eq!(got["good-pkg"].version, "1.0.0-r0");
    }

    #[test]
    fn a_locked_version_produces_a_pinned_spec() {
        // The round trip that makes a rebuild reproducible: whatever the rpmdb
        // reported becomes the spec handed to the next depsolve.
        let e = PackageFetchEntry::from_package_source(
            "avocado-ext-deptest-base",
            None,
            "1.2.0-r0",
            None,
        );
        assert_eq!(e.package_spec, "avocado-ext-deptest-base-1.2.0-r0");
        assert_ne!(
            e.package_spec, "avocado-ext-deptest-base",
            "a pinned entry must not degrade to a bare name"
        );
    }

    // ---- shell-safety validation ----------------------------------------

    #[test]
    fn ordinary_names_validate() {
        let e = PackageFetchEntry::from_package_source(
            "avocado-ext-deptest-base",
            None,
            "1.2.0-r0",
            Some("my-repo"),
        );
        assert!(e.validate().is_ok());
    }

    #[test]
    fn shell_metacharacters_are_rejected() {
        for bad in [
            "ext;rm -rf /",
            "ext$(whoami)",
            "ext`id`",
            "ext name",
            "ext|other",
            "ext'quote",
            "ext\"quote",
            "ext\nnewline",
        ] {
            let e = PackageFetchEntry::from_package_source(bad, None, "1.0.0", None);
            assert!(
                e.validate().is_err(),
                "should have rejected {bad:?} before it reached the shell"
            );
        }
    }

    #[test]
    fn the_pipe_delimiter_is_rejected_in_every_field() {
        // The script splits `<ext>|<pkg>|<spec>`, so a pipe anywhere corrupts
        // the split even without being dangerous.
        let e = PackageFetchEntry::from_package_source("app", Some("pkg|x"), "1.0.0", None);
        assert!(e.validate().is_err());
        let e = PackageFetchEntry::from_package_source("app", None, "1.0.0", Some("repo|x"));
        assert!(e.validate().is_err());
    }

    #[test]
    fn empty_values_are_rejected() {
        let e = PackageFetchEntry::from_package_source("", None, "1.0.0", None);
        assert!(e.validate().is_err());
    }

    #[test]
    fn test_package_entry_uses_extension_name_by_default() {
        let e = PackageFetchEntry::from_package_source("weston-base", None, "1.2.0", None);
        assert_eq!(e.ext_name, "weston-base");
        assert_eq!(e.package_spec, "weston-base-1.2.0");
        assert_eq!(e.repo_name, None);
    }

    #[test]
    fn test_package_entry_keeps_name_separate_from_spec() {
        // The layout query matches on %{name}; `app-a-0.1.0` cannot be split
        // back into name and version in shell, so the name is carried.
        let e = PackageFetchEntry::from_package_source("app-a", None, "0.1.0", None);
        assert_eq!(e.package_name, "app-a");
        assert_eq!(e.package_spec, "app-a-0.1.0");
    }

    #[test]
    fn test_package_entry_honors_a_package_rename() {
        // `source.package` publishes the extension under a different RPM name.
        // The install spec must use it, while the extension name (and so the
        // includes/<name>/ directory) stays as declared.
        let e = PackageFetchEntry::from_package_source(
            "weston-base",
            Some("avocado-ext-weston-base"),
            "1.2.0",
            None,
        );
        assert_eq!(e.ext_name, "weston-base");
        assert_eq!(e.package_name, "avocado-ext-weston-base");
        assert_eq!(e.package_spec, "avocado-ext-weston-base-1.2.0");
    }

    #[test]
    fn test_package_entry_wildcard_version_is_an_unpinned_spec() {
        // `*` hands version choice to the depsolver rather than pinning.
        let e = PackageFetchEntry::from_package_source("app-a", None, "*", None);
        assert_eq!(e.package_spec, "app-a");
    }

    #[test]
    fn test_package_entry_carries_repo_restriction() {
        let e = PackageFetchEntry::from_package_source("app-a", None, "1.0.0", Some("my-repo"));
        assert_eq!(e.repo_name.as_deref(), Some("my-repo"));
    }

    #[test]
    fn test_extension_fetcher_creation() {
        let fetcher = ExtensionFetcher::new(
            "avocado.yaml".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
            "docker.io/avocadolinux/sdk:latest".to_string(),
            false,
        );

        assert!(!fetcher.verbose);
        assert_eq!(fetcher.target, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_is_extension_installed() {
        // This would need a temp directory to test properly
        // For now just verify the function exists
        let result =
            ExtensionFetcher::is_extension_installed(Path::new("/nonexistent"), "test-ext");
        assert!(!result);
    }

    #[test]
    fn test_is_extension_installed_with_config() {
        let tmp_dir = std::env::temp_dir().join("avocado_test_ext_installed");
        let ext_dir = tmp_dir.join("my-ext");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&ext_dir).unwrap();

        // Directory exists but no config file -> not installed
        assert!(!ExtensionFetcher::is_extension_installed(
            &tmp_dir, "my-ext"
        ));

        // Add avocado.yaml -> installed
        std::fs::write(ext_dir.join("avocado.yaml"), "version: '1.0.0'").unwrap();
        assert!(ExtensionFetcher::is_extension_installed(&tmp_dir, "my-ext"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
