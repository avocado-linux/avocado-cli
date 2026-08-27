//! Extension fetch command implementation.
//!
//! This command fetches remote extensions from various sources (repo, git, path)
//! and installs them to `$AVOCADO_PREFIX/includes/<ext_name>/`.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config, ExtensionSource};
use crate::utils::ext_fetch::{ExtensionFetcher, PackageFetchEntry};
use crate::utils::lockfile::{ExtensionSourceLock, LockFile};
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};
use crate::utils::target::resolve_target_required;

/// Command to fetch remote extensions
pub struct ExtFetchCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Specific extension to fetch (if None, fetches all remote extensions)
    pub extension: Option<String>,
    /// Enable verbose output
    pub verbose: bool,
    /// Force re-fetch even if already installed
    pub force: bool,
    /// Target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Run command on remote host
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
    /// Refuse to update the lock; fail on any lock drift instead.
    pub locked: bool,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl ExtFetchCommand {
    /// Create a new ExtFetchCommand instance
    pub fn new(
        config_path: String,
        extension: Option<String>,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            extension,
            verbose,
            force,
            target,
            container_args,
            sdk_arch: None,
            runs_on: None,
            nfs_port: None,
            locked: false,
            composed_config: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Fail rather than update the lock. A declared extension with no lock
    /// entry, a pinned version that moved, and a pin that cannot satisfy the
    /// current requirements are all errors, and `avocado.lock` is never
    /// written.
    pub fn with_locked(mut self, locked: bool) -> Self {
        self.locked = locked;
        self
    }

    /// Set remote execution host and NFS port
    pub fn with_runs_on(mut self, runs_on: String, nfs_port: Option<u16>) -> Self {
        self.runs_on = Some(runs_on);
        self.nfs_port = nfs_port;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Execute the fetch command
    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;

        // Resolve target
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Get container image
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration"))?;

        // Discover remote extensions from the **composed** value, not the raw
        // consumer yaml. A dependency declared inside an already-fetched
        // extension's own avocado.yaml only exists in the composed config;
        // reading the raw file would make it permanently undiscoverable.
        let remote_extensions =
            Config::discover_remote_extensions_from_value(&composed.merged_value)?;

        // Everything visible before we materialize anything. Later rounds fetch
        // only what was *revealed* by a fetch, which is also what keeps a
        // `--extension` filter meaningful: extensions the filter excluded were
        // visible from the start and are never picked up by a later round.
        let visible_at_start: HashSet<String> =
            remote_extensions.iter().map(|(n, _)| n.clone()).collect();

        if remote_extensions.is_empty() {
            print_info(
                "No remote extensions found in configuration.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Filter to specific extension if requested
        let extensions_to_fetch: Vec<(String, ExtensionSource)> =
            if let Some(ref ext_name) = self.extension {
                remote_extensions
                    .into_iter()
                    .filter(|(name, _)| name == ext_name)
                    .collect()
            } else {
                remote_extensions
            };

        if extensions_to_fetch.is_empty() {
            if let Some(ref ext_name) = self.extension {
                return Err(anyhow::anyhow!(
                    "Extension '{ext_name}' not found in configuration or is not a remote extension"
                ));
            }
            return Ok(());
        }

        // Get the extensions install directory (container path)
        // The directory will be created inside the container, not on the host
        let extensions_dir = config.get_extensions_dir(&self.config_path, &target);

        if self.verbose {
            print_info(
                &format!(
                    "Fetching {} remote extension(s) to {}",
                    extensions_to_fetch.len(),
                    extensions_dir.display()
                ),
                OutputLevel::Normal,
            );
        }

        // Create the fetcher
        // If container_args were already passed (e.g., from sdk install), use them directly
        // Otherwise, merge from config
        let effective_container_args = if self.container_args.is_some() {
            self.container_args.clone()
        } else {
            config.merge_sdk_container_args(None)
        };

        // Get the resolved src_dir for resolving relative extension paths
        let src_dir = config.get_resolved_src_dir(&self.config_path);

        // Load lock file for version pinning
        let lock_src_dir = src_dir.clone().unwrap_or_else(|| {
            Path::new(&self.config_path)
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });
        let mut lock_file =
            LockFile::load(&lock_src_dir).with_context(|| "Failed to load lock file")?;
        let mut lock_file_dirty = false;
        let lock_exists = LockFile::get_path(&lock_src_dir).exists()
            || LockFile::legacy_path(&lock_src_dir).exists();

        let fetcher = ExtensionFetcher::new(
            self.config_path.clone(),
            target.clone(),
            container_image.to_string(),
            self.verbose,
        )
        .with_repo_url(config.get_sdk_repo_url())
        .with_repo_release(config.get_sdk_repo_release())
        .with_container_args(effective_container_args)
        .with_sdk_arch(self.sdk_arch.clone())
        .with_src_dir(src_dir);

        // Fetch each extension
        let mut fetched_count = 0;
        let mut skipped_count = 0;

        // Materialization proceeds in rounds. Round 1 fetches what the config
        // already shows; each later round picks up extensions that only became
        // visible *because* of the previous round — a `git`/`path` dependency
        // the depsolver cannot pull, or a sibling declaration merged in only
        // once its parent's avocado.yaml became readable.
        //
        // In practice this settles in one round: package dependencies are
        // resolved inside a single dnf transaction (see below), so there is
        // usually nothing left to reveal.
        let mut attempted: HashSet<String> = HashSet::new();
        let mut round_targets = extensions_to_fetch;
        let mut round = 0usize;

        // Defensive bound. Termination is already guaranteed by `attempted`
        // growing monotonically over a finite extension set; this only stops a
        // pathological config from spinning.
        const MAX_ROUNDS: usize = 10;

        while !round_targets.is_empty() && round < MAX_ROUNDS {
            round += 1;

            // `--force` re-fetches what the user asked for, not extensions
            // discovered on the way — those were just installed.
            let force_this_round = self.force && round == 1;

            // A declared extension with no pinned version falls through to
            // its config spec below and resolves to whatever the feed serves
            // today — the drift --locked exists to catch. Refuse before the
            // transaction, so the lock is untouched by construction.
            // Per round, because a revealed extension takes the same path.
            if self.locked {
                let unpinned = unlocked_declared(&round_targets, &lock_file, &target);
                if !unpinned.is_empty() {
                    let heading = if lock_exists {
                        "avocado.lock does not pin declared extension(s):"
                    } else {
                        "avocado.lock does not exist, so no declared extension is pinned:"
                    };
                    return Err(anyhow::anyhow!(
                        "{heading}\n{}\n\
                         --locked forbids resolving them. Re-run without --locked to update the lock.",
                        unpinned.join("\n")
                    ));
                }
            }

            // Package-source extensions are collected and installed in ONE dnf
            // transaction rather than one container run each. That is what lets the
            // depsolver pull in each package's `Requires: avocado-ext(<dep>)`
            // closure — inter-extension dependencies are materialized by dnf, not
            // by repeated fetch/recompose/discover rounds.
            let mut package_batch: Vec<PackageFetchEntry> = Vec::new();

            for (ext_name, source) in &round_targets {
                if !attempted.insert(ext_name.clone()) {
                    continue;
                }
                // Check if already installed. Not under --locked: a skipped
                // extension never reaches the lock compare below, so a retry
                // after a failed --locked run (installroot already mutated)
                // would see everything installed and pass with the lock still
                // stale. Re-requesting an installed exact NEVRA is a dnf no-op.
                if !force_this_round
                    && !self.locked
                    && ExtensionFetcher::is_extension_installed(&extensions_dir, ext_name)
                {
                    if self.verbose {
                        print_info(
                        &format!("Extension '{ext_name}' is already installed, skipping (use --force to re-fetch)"),
                        OutputLevel::Normal,
                    );
                    }
                    skipped_count += 1;
                    continue;
                }

                // For package-type sources, use locked version if available
                let effective_source = if let ExtensionSource::Package {
                    version,
                    package,
                    repo_name,
                    include,
                } = source
                {
                    // Lock versions are already RPM-form NEVRAs and pass
                    // through untouched. A CONFIG-declared version is semver:
                    // a pre-release like `1.0.0-rc.1` is packaged as
                    // `1.0.0~rc.1`, so without conversion the generated spec
                    // matches nothing on the very first fetch — replay only
                    // worked because the rpmdb already reports the `~` form.
                    let effective_version = match lock_file
                        .get_extension_source(&target, ext_name)
                        .and_then(|s| s.version.as_deref())
                    {
                        Some(locked) => locked.to_string(),
                        None if version == "*" => version.clone(),
                        None => crate::utils::version::to_rpm_version(version)
                            .unwrap_or_else(|_| version.clone()),
                    };
                    ExtensionSource::Package {
                        version: effective_version,
                        package: package.clone(),
                        repo_name: repo_name.clone(),
                        include: include.clone(),
                    }
                } else {
                    source.clone()
                };

                // Defer package sources to the batched transaction below; record
                // their lock metadata now so the bookkeeping is identical either way.
                if let ExtensionSource::Package {
                    version,
                    package,
                    repo_name,
                    ..
                } = &effective_source
                {
                    // Declared depends_on names ride along so fetch_packages
                    // can order repo-restricted groups provider-first.
                    let declared_deps: Vec<String> = composed
                        .merged_value
                        .get("extensions")
                        .and_then(|e| e.as_mapping())
                        .and_then(|m| m.get(serde_yaml::Value::String(ext_name.clone())))
                        .map(crate::utils::ext_deps::dependency_names)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|d| crate::utils::interpolation::interpolate_name(&d, &target))
                        .collect();
                    package_batch.push(
                        PackageFetchEntry::from_package_source(
                            ext_name,
                            package.as_deref(),
                            version,
                            repo_name.as_deref(),
                        )
                        .with_depends_on(declared_deps),
                    );
                    // Lock entries are written *after* the transaction, from
                    // the installroot's rpmdb — see below. Recording the
                    // requested version here would pin `"*"` as `"*"`.
                    fetched_count += 1;
                    continue;
                }

                print_info(
                    &format!("Fetching extension '{ext_name}'..."),
                    OutputLevel::Normal,
                );

                match fetcher
                    .fetch(ext_name, &effective_source, &extensions_dir, self.force)
                    .await
                {
                    Ok(install_path) => {
                        print_success(
                            &format!(
                                "Successfully fetched extension '{ext_name}' to {}",
                                install_path.display()
                            ),
                            OutputLevel::Normal,
                        );

                        fetched_count += 1;
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "Failed to fetch extension '{ext_name}': {e}"
                        ));
                    }
                }
            }

            // Replay dependencies the lock already recorded, pinned to the
            // versions they resolved to last time.
            //
            // This is what makes a clean checkout reproducible. An implied
            // dependency is named nowhere in `avocado.yaml` — only the lock
            // knows it exists — so without seeding it here, the first fetch on
            // a fresh tree lets the depsolver choose freely and quietly
            // produces a different closure than the lock describes.
            //
            // Skipped only for this round's extensions, which already carry
            // their lock version from the loop above. A declared extension
            // OUTSIDE the round (excluded by the `--extension` filter; `sdk
            // install` fetches one extension at a time) whose entry an older
            // fetch flagged implied must still be replayed, or the depsolver
            // resolves it freely when a batch member pulls it in.
            let round_package_exts = package_source_names(&round_targets);
            // Everything up to here is author-declared; pins are appended
            // after, so the two groups can be separated again if the pinned
            // solve has to be retried without them.
            let declared_count = package_batch.len();
            for (name, pin) in lock_file.implied_extension_sources(&target) {
                if round_package_exts.contains(&name) {
                    continue;
                }
                let Some(version) = pin.version.as_deref() else {
                    continue;
                };
                if self.verbose {
                    print_info(
                        &format!("Pinning locked dependency '{name}' to {version}"),
                        OutputLevel::Normal,
                    );
                }
                package_batch.push(PackageFetchEntry::from_package_source(
                    &name,
                    pin.package.as_deref(),
                    version,
                    None,
                ));
            }

            let pinned_count = package_batch.len() - declared_count;

            // The post-transaction recompose doubles as the round-end
            // discovery below; composing can read extension configs through
            // a container, so it is not done twice.
            let mut round_composed: Option<ComposedConfig> = None;

            // One transaction for every package-source extension. dnf resolves and
            // installs any dependency extensions beyond those named here.
            if !package_batch.is_empty() {
                print_info(
                    &format!(
                        "Fetching {} package extension(s) and their dependencies...",
                        package_batch.len()
                    ),
                    OutputLevel::Normal,
                );
                // Locked pins are handed to dnf as exact NEVRAs, so a
                // dependent requiring a newer base does not silently win — the
                // depsolve fails on contradictory requests. That failure is
                // the drift signal.
                //
                // Default is to re-solve without the pins and report what
                // moved: the conflict only arises because something the author
                // changed invalidated the old solution, and recomputing it is
                // the lock doing its job. `--locked` turns the same situation
                // into an error, which is what CI wants.
                let resolved = match fetcher
                    .fetch_packages(&package_batch, force_this_round)
                    .await
                {
                    Ok(r) => r,
                    Err(e) if pinned_count > 0 && !self.locked => {
                        print_warning(
                            "Locked dependency versions could not satisfy current \
                             requirements; re-resolving.",
                            OutputLevel::Normal,
                        );
                        if self.verbose {
                            print_info(&format!("Depsolve error was: {e}"), OutputLevel::Normal);
                        }
                        let unpinned: Vec<PackageFetchEntry> =
                            package_batch.iter().take(declared_count).cloned().collect();
                        fetcher
                            .fetch_packages(&unpinned, force_this_round)
                            .await
                            .with_context(|| {
                                "Re-resolving without locked dependency versions also failed"
                            })?
                    }
                    Err(e) if pinned_count > 0 && self.locked => {
                        return Err(e).with_context(|| {
                            "avocado.lock pins dependency versions that cannot satisfy the \
                             current requirements.\n\
                             Re-run without --locked to update the lock, or align the \
                             declared versions."
                        });
                    }
                    Err(e) => return Err(e),
                };

                // Report any locked dependency whose version moved. Under
                // --locked this is fatal; otherwise it is a loud warning
                // backed by a reviewable diff in avocado.lock.
                let mut drift: Vec<String> = Vec::new();
                let mut downgrades: Vec<String> = Vec::new();
                for (name, pin) in lock_file.implied_extension_sources(&target) {
                    let pkg = pin.package.clone().unwrap_or_else(|| name.clone());
                    let (Some(was), Some(now)) = (
                        pin.version.as_deref(),
                        resolved.get(&pkg).map(|p| p.version.as_str()),
                    ) else {
                        continue;
                    };
                    if was == now {
                        continue;
                    }
                    let line = format!("  {name}: {was} -> {now}");
                    if crate::utils::ext_deps::is_rpm_downgrade(was, now) {
                        downgrades.push(line);
                    } else {
                        drift.push(line);
                    }
                }

                // A constraint dragging a shared base *backwards* has no benign
                // reading — it means something old was pinned against a newer
                // platform. Refuse regardless of --locked.
                if !downgrades.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Refusing to downgrade locked dependency extension(s):\n{}\n\
                         A dependent is constraining a shared base to an older version. \
                         Align the declared versions rather than moving the base back.",
                        downgrades.join("\n")
                    ));
                }
                if !drift.is_empty() {
                    if self.locked {
                        return Err(anyhow::anyhow!(
                            "avocado.lock is out of date; --locked forbids updating it:\n{}\n\
                             Re-run without --locked to update the lock.",
                            drift.join("\n")
                        ));
                    }
                    print_warning(
                        &format!(
                            "Updating locked dependency extension(s) in avocado.lock:\n{}\n\
                             Review the avocado.lock diff before committing.",
                            drift.join("\n")
                        ),
                        OutputLevel::Normal,
                    );
                }

                // Authorship is a property of the config, not of a round's
                // batch: a filtered or later-round fetch sees only a slice of
                // the config, and classifying from the slice marked a declared
                // dependency implied. Taken from a recompose AFTER the
                // transaction: a just-materialized extension's own avocado.yaml
                // may declare a package source the depsolver also pulled in,
                // and the pre-fetch config would call it implied on a clean
                // tree and declared on every later fetch.
                let recomposed = Config::load_composed(&self.config_path, Some(&target))
                    .with_context(|| {
                        format!(
                            "Failed to reload config from {} after fetch",
                            self.config_path
                        )
                    })?;
                let declared = package_source_names(
                    &Config::discover_remote_extensions_from_value(&recomposed.merged_value)?,
                );
                round_composed = Some(recomposed);

                // Record what the installroot actually holds — the only
                // reproducible answer. It covers extensions the depsolver
                // pulled in unasked, and resolves `version: "*"` to a real
                // NEVRA instead of round-tripping the wildcard.
                let requested: HashMap<&str, &PackageFetchEntry> = package_batch
                    .iter()
                    .map(|e| (e.package_name.as_str(), e))
                    .collect();
                // Every avocado-ext capability some installed package still
                // requires — the solver-visible LIVE edges. An implied entry
                // outside this set is a leftover from a removed edge and must
                // age out of the lock rather than replay forever.
                let required_caps: HashSet<&str> = resolved
                    .values()
                    .flat_map(|p| p.requires_exts.iter().map(String::as_str))
                    .collect();
                let mut locked_changes: Vec<String> = Vec::new();
                for (pkg_name, installed) in &resolved {
                    let entry = requested.get(pkg_name.as_str());
                    // Extension identity, in precedence order: the declared
                    // entry's name; else the package's own avocado-ext(<name>)
                    // provide — the identity that survives a `source.package`
                    // rename, so a depsolver-pulled renamed dependency locks
                    // under the name the graph and stamp code address it by.
                    //
                    // An UNREQUESTED package with no avocado-ext provide is an
                    // ordinary RPM dependency, not an extension — locking it
                    // would fabricate an implied extension that later fetches
                    // replay into includes/<pkg>. Only explicitly requested
                    // legacy packages may lack the provide.
                    let ext_name = match (entry, installed.ext_name.as_ref()) {
                        (Some(e), _) => e.ext_name.clone(),
                        (None, Some(provide)) => provide.clone(),
                        (None, None) => continue,
                    };
                    // Implied means "not author-declared", NOT "not in this
                    // transaction": a replayed pin sits in the batch (and so
                    // in `requested`), and recording it implied:false would
                    // make the lock forget the extension belongs to the
                    // solver-derived closure — the next clean checkout would
                    // stop replaying its pin.
                    let implied = !declared.contains(&ext_name);
                    // A leftover implied extension nothing requires any more
                    // (its depends_on edge was removed) is pruned from the
                    // lock instead of re-recorded. The installed payload still
                    // sits in the installroot until a --force re-fetch clears
                    // it, but the lock stops asserting it, so it no longer
                    // replays into future solves.
                    let planned = if implied && !required_caps.contains(ext_name.as_str()) {
                        None
                    } else {
                        Some(ExtensionSourceLock {
                            source_type: "package".to_string(),
                            package: Some(pkg_name.clone()),
                            version: Some(installed.version.clone()),
                            implied,
                        })
                    };
                    // --locked never mutates the lock: whatever the plain
                    // path would have written differently is reported.
                    if self.locked {
                        let existing = lock_file.get_extension_source(&target, &ext_name);
                        if let Some(line) =
                            locked_source_change(&ext_name, existing, planned.as_ref())
                        {
                            locked_changes.push(line);
                        }
                        continue;
                    }
                    match planned {
                        None => lock_file.clear_extension_source(&target, &ext_name),
                        Some(source) => lock_file.set_extension_source(&target, &ext_name, source),
                    }
                    lock_file_dirty = true;
                }

                if !locked_changes.is_empty() {
                    locked_changes.sort();
                    return Err(anyhow::anyhow!(
                        "avocado.lock is out of date; --locked forbids updating it:\n{}\n\
                         Re-run without --locked to update the lock.",
                        locked_changes.join("\n")
                    ));
                }

                print_success(
                    &format!(
                        "Successfully fetched {} package extension(s).",
                        package_batch.len()
                    ),
                    OutputLevel::Normal,
                );
            }

            // See whether materializing the round revealed any remote
            // extension that was not visible before we started. Nothing
            // touches the tree after the transaction, so its recompose is
            // current; only a round with no transaction composes here.
            let recomposed = match round_composed {
                Some(composed) => composed,
                None => {
                    Config::load_composed(&self.config_path, Some(&target)).with_context(|| {
                        format!(
                            "Failed to reload config from {} after fetch",
                            self.config_path
                        )
                    })?
                }
            };
            let revealed = Config::discover_remote_extensions_from_value(&recomposed.merged_value)?;
            round_targets = revealed
                .into_iter()
                .filter(|(name, _)| !visible_at_start.contains(name) && !attempted.contains(name))
                .collect();

            if !round_targets.is_empty() && self.verbose {
                print_info(
                    &format!(
                        "Discovered {} additional remote extension(s) after fetching: {}",
                        round_targets.len(),
                        round_targets
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        if round >= MAX_ROUNDS && !round_targets.is_empty() {
            return Err(anyhow::anyhow!(
                "Extension discovery did not settle after {MAX_ROUNDS} fetch rounds; \
                 still pending: {}",
                round_targets
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // Nothing above dirties the lock under --locked; the guard keeps that
        // an invariant rather than a property of each path remembering to.
        if lock_file_dirty && !self.locked {
            lock_file
                .save(&lock_src_dir)
                .with_context(|| "Failed to save lock file")?;
        }

        // Summary
        if fetched_count > 0 || skipped_count > 0 {
            let mut summary_parts = Vec::new();
            if fetched_count > 0 {
                summary_parts.push(format!("{fetched_count} fetched"));
            }
            if skipped_count > 0 {
                summary_parts.push(format!("{skipped_count} skipped"));
            }
            print_info(
                &format!("Extension fetch complete: {}", summary_parts.join(", ")),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Get the list of remote extensions that would be fetched
    #[allow(dead_code)]
    pub fn get_remote_extensions(&self) -> Result<Vec<(String, ExtensionSource)>> {
        Config::discover_remote_extensions(&self.config_path, self.target.as_deref())
    }
}

/// Names of the package-source extensions in a discovered extension list.
fn package_source_names(extensions: &[(String, ExtensionSource)]) -> HashSet<String> {
    extensions
        .iter()
        .filter(|(_, source)| matches!(source, ExtensionSource::Package { .. }))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Declared package-source extensions the lock does not pin — the ones a
/// fetch would resolve from the feed instead of the lock. A `"*"` entry
/// counts: an older CLI wrote the request back instead of the resolved NEVRA,
/// and it pins nothing. One diagnostic line each, sorted.
fn unlocked_declared(
    declared: &[(String, ExtensionSource)],
    lock_file: &LockFile,
    target: &str,
) -> Vec<String> {
    let mut out: Vec<String> = declared
        .iter()
        .filter(|(_, source)| matches!(source, ExtensionSource::Package { .. }))
        .filter_map(|(name, _)| {
            match lock_file
                .get_extension_source(target, name)
                .and_then(|s| s.version.as_deref())
            {
                None => Some(format!("  {name}")),
                Some("*") => Some(format!("  {name} (locked as \"*\", which pins nothing)")),
                Some(_) => None,
            }
        })
        .collect();
    out.sort();
    out
}

/// The `name: was -> now` line a `--locked` run reports when the entry the
/// plain path would write differs from the one on disk. `None` when they
/// agree. Either side may be absent: a new implied dependency has no
/// `existing`, a pruned one has no `planned`.
fn locked_source_change(
    ext_name: &str,
    existing: Option<&ExtensionSourceLock>,
    planned: Option<&ExtensionSourceLock>,
) -> Option<String> {
    if existing == planned {
        return None;
    }
    // A `source.package` rename at the same version would otherwise read
    // as `1.0-r0 -> 1.0-r0`.
    let renamed = matches!(
        (
            existing.and_then(|s| s.package.as_deref()),
            planned.and_then(|s| s.package.as_deref()),
        ),
        (Some(was), Some(now)) if was != now
    );
    let describe = |source: Option<&ExtensionSourceLock>| match source {
        None => "(none)".to_string(),
        Some(s) => format!(
            "{}{}{}",
            match (&s.package, renamed) {
                (Some(package), true) => format!("{package} "),
                _ => String::new(),
            },
            s.version.as_deref().unwrap_or("(unpinned)"),
            if s.implied { " (implied)" } else { "" }
        ),
    };
    Some(format!(
        "  {ext_name}: {} -> {}",
        describe(existing),
        describe(planned)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ext_fetch_command_creation() {
        let cmd = ExtFetchCommand::new(
            "avocado.yaml".to_string(),
            Some("test-ext".to_string()),
            true,
            false,
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert_eq!(cmd.extension, Some("test-ext".to_string()));
        assert!(cmd.verbose);
        assert!(!cmd.force);
    }

    fn package(name: &str) -> (String, ExtensionSource) {
        (
            name.to_string(),
            ExtensionSource::Package {
                version: "*".to_string(),
                package: None,
                repo_name: None,
                include: None,
            },
        )
    }

    fn entry(version: Option<&str>, implied: bool) -> ExtensionSourceLock {
        ExtensionSourceLock {
            source_type: "package".to_string(),
            package: None,
            version: version.map(str::to_string),
            implied,
        }
    }

    #[test]
    fn unlocked_declared_names_package_sources_without_a_pinned_version() {
        let mut lock = LockFile::new();
        lock.set_extension_source("t", "pinned", entry(Some("1.0-r0"), false));
        lock.set_extension_source("t", "versionless", entry(None, false));
        lock.set_extension_source("t", "wildcard", entry(Some("*"), false));
        lock.set_extension_source("other", "other-target", entry(Some("1.0-r0"), false));
        let declared = vec![
            package("other-target"),
            package("versionless"),
            package("wildcard"),
            package("pinned"),
            (
                "from-git".to_string(),
                ExtensionSource::Git {
                    url: "https://example.invalid/ext.git".to_string(),
                    git_ref: None,
                    sparse_checkout: None,
                    include: None,
                },
            ),
            package("absent"),
        ];

        assert_eq!(
            unlocked_declared(&declared, &lock, "t"),
            vec![
                "  absent",
                "  other-target",
                "  versionless",
                "  wildcard (locked as \"*\", which pins nothing)",
            ]
        );
    }

    /// An absent lockfile loads as an empty lock, so every declared
    /// package extension is unpinned — the missing-file case needs no
    /// separate path.
    #[test]
    fn unlocked_declared_reports_everything_against_an_empty_lock() {
        let declared = vec![package("b"), package("a")];
        assert_eq!(
            unlocked_declared(&declared, &LockFile::new(), "t"),
            vec!["  a", "  b"]
        );
    }

    #[test]
    fn locked_source_change_is_silent_when_disk_and_plan_agree() {
        let same = entry(Some("1.0-r0"), false);
        assert_eq!(locked_source_change("a", Some(&same), Some(&same)), None);
        assert_eq!(locked_source_change("a", None, None), None);
    }

    #[test]
    fn locked_source_change_names_missing_moved_pruned_and_reclassified_entries() {
        let v1 = entry(Some("1.0-r0"), false);
        let v2 = entry(Some("1.1-r0"), false);
        let v1_implied = entry(Some("1.0-r0"), true);
        let wildcard = entry(Some("*"), false);

        assert_eq!(
            locked_source_change("a", None, Some(&v1)).as_deref(),
            Some("  a: (none) -> 1.0-r0")
        );
        assert_eq!(
            locked_source_change("a", Some(&v1), Some(&v2)).as_deref(),
            Some("  a: 1.0-r0 -> 1.1-r0")
        );
        assert_eq!(
            locked_source_change("a", Some(&v1_implied), None).as_deref(),
            Some("  a: 1.0-r0 (implied) -> (none)")
        );
        assert_eq!(
            locked_source_change("a", Some(&v1), Some(&v1_implied)).as_deref(),
            Some("  a: 1.0-r0 -> 1.0-r0 (implied)")
        );
        // A lock written by an older CLI recorded the request, not the
        // resolved NEVRA; it pins nothing and must read as drift.
        assert_eq!(
            locked_source_change("a", Some(&wildcard), Some(&v1)).as_deref(),
            Some("  a: * -> 1.0-r0")
        );
    }

    /// Same version, different `source.package`: the package must appear
    /// or the line reads as no change at all.
    #[test]
    fn locked_source_change_names_the_package_on_a_rename() {
        let old = ExtensionSourceLock {
            package: Some("old-app".to_string()),
            ..entry(Some("1.0-r0"), false)
        };
        let new = ExtensionSourceLock {
            package: Some("new-app".to_string()),
            ..entry(Some("1.0-r0"), false)
        };
        assert_eq!(
            locked_source_change("app", Some(&old), Some(&new)).as_deref(),
            Some("  app: old-app 1.0-r0 -> new-app 1.0-r0")
        );
        // Only a rename names the package; a plain version move stays terse.
        let moved = ExtensionSourceLock {
            version: Some("1.1-r0".to_string()),
            ..old.clone()
        };
        assert_eq!(
            locked_source_change("app", Some(&old), Some(&moved)).as_deref(),
            Some("  app: 1.0-r0 -> 1.1-r0")
        );
    }
}
