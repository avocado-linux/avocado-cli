use crate::utils::feeds::FeedStage;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer, TuiContext};
use crate::utils::kernel_resolver::{
    off_kernel_dnf_excludes, resolve_and_pin_kernel_version, ResolveParams,
};
use crate::utils::kernel_version::substitute_kernel_version;
use crate::utils::lockfile::{build_package_spec_with_lock, LockFile, SysrootType};
use crate::utils::output::{
    print_debug, print_error, print_info, print_success, print_warning, OutputLevel,
};
use crate::utils::runs_on::RunsOnContext;
use crate::utils::stamps::{
    compute_ext_install_input_hash_with_deps, ext_dep_fingerprint,
    generate_batch_read_stamps_script, generate_write_stamp_script, parse_batch_stamps_output,
    remove_own_stamp_line, validate_stamp, Stamp, StampOutputs, StampRequirement, StampStatus,
};
use crate::utils::target::resolve_target_required;
use crate::utils::tui::{TaskId, TuiGuard};

/// Whether an extension name is safe to interpolate raw into the fast-path
/// batch shell script (stamp path + sysroot probe). Names come from composed
/// YAML keys, which are not constrained to shell-safe characters, so a name
/// with quotes or `$(...)` could break the batch protocol or run commands in
/// the SDK container. The fast path only skips names that pass this allowlist;
/// anything else falls through to a normal install.
fn ext_name_stamp_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether every install option this invocation carries is covered by the ext
/// install input hash.
///
/// `--dnf-arg` and `sdk.disable_weak_dependencies` change what dnf resolves and
/// neither reaches the hash, so a sysroot installed under them is not described
/// by the stamp's inputs alone. Shared by the fast path and the stamp writer so
/// the two cannot disagree about what "standard" means.
fn transaction_is_standard(dnf_args: Option<&[String]>, disable_weak_dependencies: bool) -> bool {
    dnf_args.is_none_or(|a| a.is_empty()) && !disable_weak_dependencies
}

/// Shell that drops an extension's build and image stamps, for prefixing to the
/// install-stamp write.
///
/// Nothing chains the installed sysroot into `ext build`'s input hash, which is
/// config-only: `ext install` writes through the plain stamp writer and records
/// no digest for `ext build` to fold. So a transaction that changed the sysroot
/// without changing the config leaves both downstream stamps reading current,
/// `ext build` skips, and `ext image` chains off the stale digest and ships the
/// previous build. Drop the two stamps that vouch for the sysroot's contents,
/// the same way `clean_ext_sysroot_command` does when it clears one. `ext build`
/// writes them back, so this costs a rebuild, not a loop.
///
/// Unconditional, because the condition has no safe direction. Gating it on a
/// non-standard transaction covered entering one and not leaving it: a
/// `--dnf-arg` install builds a lean sysroot and drops the stamps, the build
/// records new ones, and the next plain install correctly declines the mark and
/// re-resolves the sysroot fatter -- then runs no cleanup, because that
/// transaction is standard. The build after it skips over the sysroot that just
/// grew.
///
/// It costs nothing to run every time. An extension that is already up to date
/// never reaches this block, and an install that runs because the config moved
/// implies a rebuild the config moved anyway.
fn drop_content_stamps_script(ext_name: &str) -> String {
    remove_own_stamp_line(&StampRequirement::ext_build(ext_name))
        + &remove_own_stamp_line(&StampRequirement::ext_image(ext_name))
}

/// Whether the fast path may skip an install on this stamp read.
///
/// Current, and not written by a transaction whose options the input hash does
/// not cover (`--dnf-args`, `sdk.disable_weak_dependencies`). The fast path
/// itself only runs for a standard transaction, so a `nonstandard_options`
/// stamp is one this invocation would resolve differently -- reinstall.
fn stamp_allows_skip(status: &StampStatus) -> bool {
    matches!(status, StampStatus::Current(stamp) if !stamp.outputs.nonstandard_options)
}

/// Shell that clears an extension's sysroot and drops the stamps that vouch for
/// what was in it.
///
/// `ext build`'s output — the extension-release files, unit wiring, the applied
/// overlay — lives in this sysroot and is *not* restored by the dnf transaction
/// that follows a clean; only `ext build` puts it back. Its stamp's inputs are
/// unchanged by a clean, so a surviving stamp would let `ext build` report "up
/// to date" over a sysroot that no longer holds its work, and `ext image` would
/// then image an empty extension. Same rule as `--no-stamps`: a step that
/// destroys an output invalidates the stamp claiming it exists.
fn clean_ext_sysroot_command(extension: &str) -> String {
    format!(
        r#"rm -rf "$AVOCADO_EXT_SYSROOTS/{extension}"
{build_stamp}{image_stamp}"#,
        build_stamp = crate::utils::stamps::remove_own_stamp_line(
            &crate::utils::stamps::StampRequirement::ext_build(extension)
        ),
        image_stamp = crate::utils::stamps::remove_own_stamp_line(
            &crate::utils::stamps::StampRequirement::ext_image(extension)
        ),
    )
}

pub struct ExtInstallCommand {
    extension: Option<String>,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    /// See [`Self::with_deps_scheduled`].
    deps_scheduled: bool,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
    pub tui_context: Option<TuiContext>,
    /// Runtime to scope extension state to. When set, extension package
    /// versions are mirrored into `lock.targets.<t>.runtimes.<r>.extensions.<ext>`
    /// alongside the global `lock.targets.<t>.extensions.<ext>` entry that
    /// today's flow populates. The on-disk extension sysroot path stays at
    /// `$AVOCADO_EXT_SYSROOTS/<ext>` for now — Phase 2d follow-ups migrate
    /// the on-disk layout once all ext command consumers (build, image,
    /// clean, runtime build) accept a runtime parameter.
    runtime: Option<String>,
}

impl ExtInstallCommand {
    pub fn new(
        extension: Option<String>,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            deps_scheduled: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
            tui_context: None,
            runtime: None,
        }
    }

    /// Scope this install to a runtime. When set, extension package versions
    /// are also recorded under `runtimes.<r>.extensions.<ext>` in the lockfile
    /// alongside the existing global `extensions.<ext>` entry, AND the
    /// container entrypoint flips `\$AVOCADO_EXT_SYSROOTS` to the runtime-
    /// scoped path. A compat symlink at the legacy location keeps callers
    /// that haven't been opted in yet (build, image, clean, runtime build,
    /// fetch) reading the same content transparently.
    pub fn with_runtime(mut self, runtime: Option<String>) -> Self {
        self.runtime = runtime;
        self
    }

    /// Build the container `env_vars` map carrying `AVOCADO_RUNTIME` when a
    /// runtime is in scope. The container entrypoint reads it to compute
    /// `\$AVOCADO_EXT_SYSROOTS`. Returns `None` when no runtime is set so
    /// `RunConfig` defaults preserve today's behavior.
    fn runtime_env_vars(&self) -> Option<HashMap<String, String>> {
        self.runtime.as_ref().map(|rt| {
            let mut m = HashMap::new();
            m.insert("AVOCADO_RUNTIME".to_string(), rt.clone());
            m
        })
    }

    /// Compute the [`SysrootType`] that this install should track in the
    /// lockfile for a given extension. Prefers the runtime-scoped variant
    /// (`runtimes.<r>.extensions.<ext>`) whenever a runtime is in scope —
    /// avocado-cli fully owns the state volume, so there's no value in
    /// dual-writing the legacy global namespace. Standalone callers
    /// without a resolved runtime fall back to the legacy variant for
    /// the deprecation window until orchestrators always pass one.
    fn extension_sysroot(&self, extension: &str) -> SysrootType {
        match self.runtime.as_deref() {
            Some(rt) => SysrootType::RuntimeExtension {
                runtime: rt.to_string(),
                extension: extension.to_string(),
            },
            None => SysrootType::Extension(extension.to_string()),
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Declare that the caller has already scheduled every `depends_on`
    /// target as its own install, ordered before this one.
    ///
    /// `avocado install` fans out one command per extension and runs them
    /// concurrently. Each of them expanding its own closure would have two
    /// dependents rebuild the same shared base at the same time — a `rm -rf`
    /// of a sysroot another task is installing into. The scheduler owns the
    /// closure there; this command only installs what it was handed.
    pub fn with_deps_scheduled(mut self, scheduled: bool) -> Self {
        self.deps_scheduled = scheduled;
        self
    }

    /// Set remote execution options
    pub fn with_runs_on(mut self, runs_on: Option<String>, nfs_port: Option<u16>) -> Self {
        self.runs_on = runs_on;
        self.nfs_port = nfs_port;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub fn with_tui_context(mut self, ctx: TuiContext) -> Self {
        self.tui_context = Some(ctx);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let ext_label = self.extension.as_deref().unwrap_or("all");
        let tui_guard = if self.tui_context.is_none() {
            Some(TuiGuard::new(
                TaskId::ExtInstall(ext_label.to_string()),
                &format!("ext install {}", ext_label),
                self.verbose,
            ))
        } else {
            None
        };
        let effective_tui_context = self
            .tui_context
            .clone()
            .or_else(|| tui_guard.as_ref().and_then(|g| g.tui_context()));

        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };

        let config = &composed.config;
        let parsed = &composed.merged_value;

        // Merge container args from config and CLI (similar to SDK commands)
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Resolve target and apply the reproducible snapshot pin before reading
        // repo_release, so it reflects the pinned channel snapshot.
        let target = resolve_target_required(self.target.as_deref(), config)?;
        crate::utils::snapshot::resolve_and_apply_for(config, &self.config_path, &target).await?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let feeds = config
            .materialize_feeds(&target, FeedStage::Ext, &self.config_path)
            .await?;

        // Determine which extensions to install (with their locations)
        let extensions_to_install: Vec<(String, ExtensionLocation)> =
            if let Some(extension_name) = &self.extension {
                // Single extension specified - use comprehensive lookup
                match config.find_extension_in_dependency_tree(
                    &self.config_path,
                    extension_name,
                    &target,
                )? {
                    Some(location) => {
                        if self.verbose {
                            match &location {
                                ExtensionLocation::Local { name, config_path } => {
                                    print_info(
                                        &format!(
                                        "Found local extension '{name}' in config '{config_path}'"
                                    ),
                                        OutputLevel::Normal,
                                    );
                                }
                                ExtensionLocation::Remote { name, source } => {
                                    print_info(
                                        &format!(
                                        "Found remote extension '{name}' with source: {source:?}"
                                    ),
                                        OutputLevel::Normal,
                                    );
                                }
                            }
                        }
                        vec![(extension_name.clone(), location)]
                    }
                    None => {
                        print_error(
                            &format!("Extension '{extension_name}' not found in configuration."),
                            OutputLevel::Normal,
                        );
                        return Ok(());
                    }
                }
            } else {
                // No extension specified - install all local extensions
                match parsed.get("extensions") {
                    Some(ext_section) => match ext_section.as_mapping() {
                        Some(table) => table
                            .keys()
                            .filter_map(|k| {
                                k.as_str().map(|s| {
                                    (
                                        s.to_string(),
                                        ExtensionLocation::Local {
                                            name: s.to_string(),
                                            config_path: self.config_path.clone(),
                                        },
                                    )
                                })
                            })
                            .collect(),
                        None => vec![],
                    },
                    None => {
                        print_info("No extensions found in configuration.", OutputLevel::Normal);
                        return Ok(());
                    }
                }
            };

        if extensions_to_install.is_empty() {
            print_info("No extensions found in configuration.", OutputLevel::Normal);
            return Ok(());
        }

        let graph = crate::utils::ext_deps::DependencyGraph::from_composed(&composed, &target)?;

        // Expand to the dependency closure, then order dependencies before
        // dependents.
        //
        // This is the *install* order, deliberately the opposite of the
        // runtime manifest's parent-first merge order: a dependency's sysroot
        // has to exist and be fully populated before a dependent can seed its
        // rpmdb from it. Installing alphabetically would seed from an empty or
        // half-built dependency and silently defeat de-duplication.
        let extensions_to_install = {
            let requested: Vec<String> = extensions_to_install
                .iter()
                .map(|(n, _)| n.clone())
                .collect();
            match graph.resolve(&requested) {
                Ok(closure) => {
                    let position = |name: &str| {
                        closure
                            .order
                            .iter()
                            .position(|n| n == name)
                            // Anything outside the graph keeps its relative
                            // place after the ordered members.
                            .unwrap_or(usize::MAX)
                    };
                    let mut ordered = extensions_to_install;
                    // `ext install kiosk-a` has to install weston-base too.
                    // kiosk-a's rpmdb is seeded from its dependency's sysroot,
                    // so a dependency the author never named still has to be
                    // there — ordering alone would leave the seed source
                    // absent and the install would fail creating the sysroot.
                    //
                    // Every closure member is a key in the composed
                    // `extensions:` mapping by construction (that mapping is
                    // what the graph was built from), which is exactly what
                    // `Local` means here: read the block out of `parsed`. The
                    // install path treats `Local` and `Remote` identically —
                    // both read the merged config — so a remote dependency
                    // needs no separate lookup.
                    //
                    // `closure.order` holds each name once, so the set of
                    // names already present needs no updating as entries are
                    // appended.
                    let missing: Vec<String> = if self.deps_scheduled {
                        vec![]
                    } else {
                        let present: HashSet<&String> = ordered.iter().map(|(n, _)| n).collect();
                        closure
                            .order
                            .iter()
                            .filter(|n| !present.contains(n))
                            .cloned()
                            .collect()
                    };
                    for name in missing {
                        let location = ExtensionLocation::Local {
                            name: name.clone(),
                            config_path: self.config_path.clone(),
                        };
                        ordered.push((name, location));
                    }
                    ordered.sort_by_key(|(name, _)| position(name));
                    if self.verbose {
                        print_info(
                            &format!(
                                "Install order (dependencies first): {}",
                                ordered
                                    .iter()
                                    .map(|(n, _)| n.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" -> ")
                            ),
                            OutputLevel::Normal,
                        );
                    }
                    ordered
                }
                // A closure that will not resolve is an error HERE, not a
                // degradation: falling back to a flat install let
                // `ext install app` "succeed" without installing or seeding
                // from its declared base — the opposite of refusing unknown
                // dependency subtrees. `resolve` only walks the requested
                // roots, so unrelated broken or unfetched extensions cannot
                // block an install that never touches them.
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Cannot install with an unresolved dependency closure: {e}\n\
                         Fix the depends_on declaration (or fetch the missing \
                         extension) and re-run."
                    ));
                }
            }
        };

        // Direct `depends_on` edges per extension, used to pick each one's
        // rpmdb seed source. Derived *after* the expansion above so a
        // dependency that was pulled in rather than named gets its own seed
        // source too — a chain seeds base <- mid <- app, not just the leaf.
        //
        // The closure resolved above (an unresolved one is an error now), so
        // every dependency's sysroot is guaranteed to exist for seeding.
        let direct_deps: HashMap<String, Vec<String>> = extensions_to_install
            .iter()
            .filter_map(|(name, _)| {
                let node = graph.get(name)?;
                if node.depends_on.is_empty() {
                    return None;
                }
                Some((
                    name.clone(),
                    node.depends_on.iter().map(|d| d.name.clone()).collect(),
                ))
            })
            .collect();

        let ext_names: Vec<&str> = extensions_to_install
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        print_info(
            &format!(
                "Installing {} extension(s): {}.",
                extensions_to_install.len(),
                ext_names.join(", ")
            ),
            OutputLevel::Normal,
        );

        // Get the SDK image from interpolated config
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
        })?;

        // Use resolved target (from CLI/env) if available, otherwise fall back to config
        let _config_target = parsed
            .get("runtimes")
            .and_then(|runtime| runtime.as_mapping())
            .and_then(|runtime_table| {
                if runtime_table.len() == 1 {
                    runtime_table.values().next()
                } else {
                    None
                }
            })
            .and_then(|runtime_config| runtime_config.get("target"))
            .and_then(|target| target.as_str())
            .map(|s| s.to_string());
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Use the container helper to run the setup commands
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Create shared RunsOnContext if running on remote host
        let mut runs_on_context: Option<RunsOnContext> = if let Some(ref runs_on) = self.runs_on {
            Some(
                container_helper
                    .create_runs_on_context(runs_on, self.nfs_port, container_image, self.verbose)
                    .await?,
            )
        } else {
            None
        };

        // Execute the installation and ensure cleanup
        let result = self
            .execute_install_internal(
                config,
                parsed,
                &extensions_to_install,
                &direct_deps,
                &graph,
                &container_helper,
                container_image,
                &target,
                repo_url.as_ref(),
                repo_release.as_ref(),
                feeds.as_ref(),
                &merged_container_args,
                runs_on_context.as_ref(),
                &effective_tui_context,
            )
            .await;

        // Always teardown the context if it was created
        if let Some(ref mut context) = runs_on_context {
            if let Err(e) = context.teardown().await {
                print_error(
                    &format!("Warning: Failed to cleanup remote resources: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        if result.is_ok() {
            if let Some(ref guard) = tui_guard {
                guard.mark_success();
            }
        }

        result
    }

    /// Internal implementation of the install logic
    #[allow(clippy::too_many_arguments)]
    async fn execute_install_internal(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        extensions_to_install: &[(String, ExtensionLocation)],
        direct_deps: &std::collections::HashMap<String, Vec<String>>,
        graph: &crate::utils::ext_deps::DependencyGraph,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        feeds: Option<&crate::utils::feeds::FeedMaterialization>,
        merged_container_args: &Option<Vec<String>>,
        runs_on_context: Option<&RunsOnContext>,
        effective_tui_context: &Option<TuiContext>,
    ) -> Result<()> {
        let total = extensions_to_install.len();

        // Load lock file for reproducible builds
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                PathBuf::from(&self.config_path)
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            });
        let mut lock_file = LockFile::load(&src_dir).with_context(|| "Failed to load lock file")?;
        lock_file.check_distro_release_compat(config.get_distro_release().as_deref());
        lock_file.distro_release = config.get_distro_release();

        if self.verbose && !lock_file.is_empty() {
            print_info(
                "Using existing lock file for version pinning.",
                OutputLevel::Normal,
            );
        }

        // Batch-read existing extension install stamps in one container call so we
        // can skip extensions whose inputs are unchanged, instead of re-running
        // the dnf transaction (and, for depends_on extensions, the sysroot
        // rebuild) on every install. Skipped under --force, --no-stamps, or
        // --runs-on; any read failure leaves the map empty and we install
        // everything.
        // The stamp only fingerprints extension config, not per-invocation install
        // options: `--dnf-args` and the weak-dependency setting change the
        // transaction but not the stamp, so disable the fast path when either is
        // in play rather than skip a differently-configured install. Also off for
        // --force, --no-stamps, and --runs-on.
        let fast_path = !self.force
            && !self.no_stamps
            && runs_on_context.is_none()
            && transaction_is_standard(
                self.dnf_args.as_deref(),
                config.get_sdk_disable_weak_dependencies(),
            );
        // Only fast-path shell-safe names (see `ext_name_stamp_safe`); the rest
        // fall through to a normal install rather than being skipped.
        let stamp_safe = ext_name_stamp_safe;
        let stamp_reads: std::collections::HashMap<String, Option<String>> = if !fast_path {
            std::collections::HashMap::new()
        } else {
            let reqs: Vec<StampRequirement> = extensions_to_install
                .iter()
                .filter(|(name, _)| stamp_safe(name))
                .map(|(name, _)| StampRequirement::ext_install(name))
                .collect();
            // Read the stamps AND probe each sysroot's existence in the same
            // call, so a surviving stamp over a manually-removed sysroot does
            // not skip a needed reinstall.
            let mut command = generate_batch_read_stamps_script(&reqs);
            for (name, _) in extensions_to_install.iter().filter(|(n, _)| stamp_safe(n)) {
                command.push_str(&format!(
                        "\nprintf 'sysroot:{name}:::%s\\n' \"$([ -d \"$AVOCADO_EXT_SYSROOTS/{name}\" ] && echo yes || echo no)\""
                    ));
            }
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                env_vars: self.runtime_env_vars(),
                ..Default::default()
            };
            match container_helper
                .run_in_container_with_output(run_config)
                .await
            {
                Ok(output) => parse_batch_stamps_output(output.as_deref().unwrap_or("")),
                Err(_) => std::collections::HashMap::new(),
            }
        };

        // Install each extension
        for (index, (ext_name, ext_location)) in extensions_to_install.iter().enumerate() {
            if self.verbose {
                print_debug(
                    &format!("Installing ({}/{}) {}.", index + 1, total, ext_name),
                    OutputLevel::Normal,
                );
            }

            // Is this extension already up to date? Compute its current input
            // hash the same way the stamp writer does -- folding in its direct
            // dependencies' current fingerprints (deps are installed earlier in
            // this topologically-ordered loop, so the lockfile is current here) --
            // and compare against the stamp read above.
            let ext_up_to_date = if !fast_path || !stamp_safe(ext_name) {
                false
            } else {
                let sysroot_present = stamp_reads
                    .get(&format!("sysroot:{ext_name}"))
                    .and_then(|v| v.as_deref())
                    == Some("yes");
                // `avocado update` clears the lock's package pins to force
                // re-resolution against the new snapshot; a dependency-free
                // extension would otherwise keep a matching config hash,
                // sysroot, and stamp and skip without re-locking. Only accept
                // the stamp when the extension still has resolved package pins.
                let has_pins = lock_file
                    .get_extension_packages_any_scope(target, ext_name)
                    .map(|p| !p.is_empty())
                    .unwrap_or(false);
                let dep_state: Vec<(String, String)> = direct_deps
                    .get(ext_name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|dep| {
                        (
                            dep.clone(),
                            ext_dep_fingerprint(&lock_file, target, graph, dep),
                        )
                    })
                    .collect();
                // The pin is read across scopes, the same way the `ext build`
                // and `ext image` readers do: a scope-specific key would make
                // this fast path and those readers disagree with the writer.
                let resolved_kernel = lock_file
                    .get_kernel_version_any_scope(target, ext_name)
                    .cloned();
                match compute_ext_install_input_hash_with_deps(
                    parsed,
                    ext_name,
                    &dep_state,
                    resolved_kernel.as_deref(),
                ) {
                    Ok(inputs) => {
                        let req = StampRequirement::ext_install(ext_name);
                        let json = stamp_reads
                            .get(&req.relative_path())
                            .and_then(|v| v.as_deref());
                        // A stamp carrying `nonstandard_options` describes a
                        // sysroot resolved with `--dnf-args` or weak
                        // dependencies disabled -- neither of which the input
                        // hash covers. The fast path only ever runs for a
                        // standard transaction, so skipping on one would leave
                        // a sysroot this invocation would have resolved
                        // differently. Reinstall instead.
                        sysroot_present
                            && has_pins
                            && stamp_allows_skip(&validate_stamp(&req, json, Some(&inputs)))
                    }
                    Err(_) => false,
                }
            };

            if !self
                .install_single_extension(
                    config,
                    parsed,
                    ext_name,
                    ext_location,
                    container_helper,
                    container_image,
                    target,
                    repo_url,
                    repo_release,
                    feeds,
                    merged_container_args,
                    config.get_sdk_disable_weak_dependencies(),
                    &mut lock_file,
                    &src_dir,
                    runs_on_context,
                    effective_tui_context,
                    direct_deps.get(ext_name).map(Vec::as_slice).unwrap_or(&[]),
                    ext_up_to_date,
                )
                .await?
            {
                return Err(anyhow::anyhow!("Failed to install extension '{ext_name}'"));
            }

            // The config-only stamp fingerprints extension config, not the
            // per-invocation transaction: `--dnf-arg` and a disabled
            // weak-dependency setting install a different package set than the
            // stamp records. The stamp is still written -- it is what says the
            // sysroot exists, and `ext build` refuses to run without it -- but
            // it is marked so this command's own fast path will not skip over
            // it on a later plain install. Deleting it instead made every
            // `install --dnf-arg=...` project unbuildable: `ext build` demands
            // the install stamp, and the only advertised fix (`avocado ext
            // install <name>`) deleted it again.
            let standard = transaction_is_standard(
                self.dnf_args.as_deref(),
                config.get_sdk_disable_weak_dependencies(),
            );
            // Write extension install stamp (unless --no-stamps, or it was
            // already up to date -- the stamp is already current).
            if !self.no_stamps && !ext_up_to_date {
                // Update peek line so it doesn't stay on "Complete!" during stamp write
                if let Some(ref ctx) = effective_tui_context {
                    ctx.renderer
                        .append_output(&ctx.task_id, "Writing install stamp...".to_string());
                }
                // Fold in the state of whatever this extension was seeded
                // from, so changing a dependency invalidates its dependents.
                // Read after the dependency installed — topological order
                // guarantees its lock entry is already current.
                // Fingerprinted through the same helper build/image validation
                // uses — the writer and readers drifting apart is the hash
                // mismatch that broke every depends_on extension at build.
                let dep_state: Vec<(String, String)> = direct_deps
                    .get(ext_name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|dep| {
                        (
                            dep.clone(),
                            crate::utils::stamps::ext_dep_fingerprint(
                                &lock_file, target, graph, dep,
                            ),
                        )
                    })
                    .collect();
                // Resolved kernel pin for this extension — set by
                // `resolve_and_pin_kernel_version` during the install above,
                // read across scopes like every other reader of it. Folding
                // it in means `avocado clean --unlock` clearing the pin (or a
                // range spec resolving to a new version) invalidates the stamp.
                let resolved_kernel = lock_file
                    .get_kernel_version_any_scope(target, ext_name)
                    .cloned();
                let inputs = compute_ext_install_input_hash_with_deps(
                    parsed,
                    ext_name,
                    &dep_state,
                    resolved_kernel.as_deref(),
                )?;
                let outputs = StampOutputs {
                    nonstandard_options: !standard,
                    ..Default::default()
                };
                let stamp = Stamp::ext_install(ext_name, target, inputs, outputs);
                let stamp_script =
                    drop_content_stamps_script(ext_name) + &generate_write_stamp_script(&stamp)?;

                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target.to_string(),
                    command: stamp_script,
                    verbose: self.verbose,
                    source_environment: true,
                    interactive: false,
                    repo_url: repo_url.cloned(),
                    feeds: feeds.cloned(),
                    repo_release: repo_release.cloned(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    // runs_on handled by shared context
                    sdk_arch: self.sdk_arch.clone(),
                    tui_context: effective_tui_context.clone(),
                    env_vars: self.runtime_env_vars(),
                    ..Default::default()
                };

                run_container_command(container_helper, run_config, runs_on_context).await?;

                if self.verbose {
                    print_info(
                        &format!("Wrote install stamp for extension '{ext_name}'."),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        if !extensions_to_install.is_empty() {
            print_success(
                &format!("Installed {} extension(s).", extensions_to_install.len()),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// A pin change is a move between two pins, or a first pin over a sysroot
    /// that already exists: `avocado update` and `avocado clean --unlock`
    /// erase every pin and keep the sysroots, so `prev` is `None` exactly
    /// when the sysroot most likely holds another kernel's modules. A first
    /// pin over no sysroot, and an extension that resolves no kernel at all
    /// (`resolved` = None), leave things as they are.
    fn kernel_pin_change(
        prev: Option<&str>,
        resolved: Option<&str>,
        sysroot_exists: bool,
    ) -> Option<(String, String)> {
        match (prev, resolved) {
            (Some(p), Some(n)) if p != n => Some((p.to_string(), n.to_string())),
            (None, Some(n)) if sysroot_exists => Some(("none".to_string(), n.to_string())),
            _ => None,
        }
    }

    /// Compare the config's current package list with the lock file's previously installed
    /// packages to detect removals. Returns true if the sysroot needs to be cleaned and
    /// reinstalled from scratch.
    ///
    /// When packages are removed from the config, DNF install alone cannot remove them from
    /// the sysroot. We must clean the sysroot and reinstall to bring it in sync with the config.
    /// Only the removed packages' lock entries are cleared, preserving version pinning for
    /// packages that remain in the config.
    fn detect_package_removals(
        &self,
        parsed: &serde_yaml::Value,
        extension: &str,
        ext_location: &ExtensionLocation,
        _config: &Config,
        target: &str,
        lock_file: &mut LockFile,
    ) -> bool {
        let sysroot = self.extension_sysroot(extension);
        let locked_names = lock_file.get_locked_package_names(target, &sysroot);

        if locked_names.is_empty() {
            return false;
        }

        // Gather current config package names for this extension
        let ext_config = match ext_location {
            ExtensionLocation::Remote { .. } | ExtensionLocation::Local { .. } => parsed
                .get("extensions")
                .and_then(|ext| ext.get(extension))
                .cloned(),
        };

        let config_names: HashSet<String> = ext_config
            .as_ref()
            .and_then(|ec| ec.get("packages"))
            .and_then(|deps| deps.as_mapping())
            .map(|deps_map| {
                deps_map
                    .keys()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let removed: Vec<String> = locked_names.difference(&config_names).cloned().collect();

        if removed.is_empty() {
            return false;
        }

        print_info(
            &format!(
                "Packages removed from extension '{}': {}. Cleaning sysroot for fresh install.",
                extension,
                removed.join(", ")
            ),
            OutputLevel::Normal,
        );

        // Remove only the stale entries, preserving version pins for remaining packages
        lock_file.remove_packages_from_sysroot(target, &sysroot, &removed);

        true
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_single_extension(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        extension: &str,
        ext_location: &ExtensionLocation,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        feeds: Option<&crate::utils::feeds::FeedMaterialization>,
        merged_container_args: &Option<Vec<String>>,
        disable_weak_dependencies: bool,
        lock_file: &mut LockFile,
        src_dir: &Path,
        runs_on_context: Option<&RunsOnContext>,
        effective_tui_context: &Option<TuiContext>,
        direct_deps: &[String],
        up_to_date: bool,
    ) -> Result<bool> {
        let sysroot = self.extension_sysroot(extension);

        // Record runtime → extension membership upfront when a runtime is in
        // scope. Extensions without `packages:` (file-only / compile-only)
        // would otherwise leave no trace in the lockfile because the
        // package-version write only fires when packages get pinned. This
        // ensures `runtimes.<r>.extensions.<ext>` exists as a membership
        // marker. Save immediately so the membership persists even when
        // the rest of install_single_extension takes the
        // no-packages-skip-save path.
        if let Some(rt) = self.runtime.as_deref() {
            lock_file.record_runtime_extension_membership(target, rt, extension);
            lock_file.save(src_dir).with_context(|| {
                format!("Failed to record membership for extension '{extension}' in runtime '{rt}'")
            })?;
        }

        // Already up to date (install stamp matches current inputs): the sysroot
        // is built and its packages are present, so skip the dnf transaction and
        // sysroot work. Membership above is still recorded for lockfile
        // consistency.
        if up_to_date {
            print_success(
                &format!("Extension '{extension}' is up to date."),
                OutputLevel::Normal,
            );
            return Ok(true);
        }

        // Snapshot the previously-pinned kernel BEFORE the resolver runs: it
        // overwrites the lock's pin in place, so reading afterwards would only
        // return what it just wrote. The kernel is resolved up here, ahead of
        // the clean decision, because a pin change is one of its inputs.
        let prev_pinned_kver = lock_file.get_kernel_version(target, &sysroot).cloned();

        // Get extension configuration from the composed/merged config
        // For remote extensions, this comes from the merged remote extension config
        // For local extensions, this comes from the main config's ext section
        let raw_ext_config = match ext_location {
            ExtensionLocation::Remote { .. } | ExtensionLocation::Local { .. } => {
                // Use the already-merged config from `parsed` which contains remote extension configs
                parsed
                    .get("extensions")
                    .and_then(|ext| ext.get(extension))
                    .cloned()
            }
        };

        // Resolve the kernel version up-front when the extension declares
        // overrides that depend on it OR has packages that could include
        // kernel-family names. Skip the container roundtrip for trivial
        // extensions (no packages, no overrides).
        let has_overrides_or_packages = raw_ext_config.as_ref().is_some_and(|ec| {
            ec.get("packages").is_some()
                || ec.as_mapping().is_some_and(|m| {
                    m.keys().any(|k| {
                        k.as_str()
                            .is_some_and(|s| s.starts_with("target-") || s.starts_with("kernel-"))
                    })
                })
        });

        // Resolve the kernel version AND, while the resolver still has its
        // ResolveParams in scope, compute dnf --exclude flags for every other
        // kernel in the feed. Excludes block transitive RDEPENDS/RRECOMMENDS
        // from resolving unqualified `kernel-module-X` virtuals against an
        // off-kernel package — the same pattern that leaks 5.15 modules into
        // a 6.6-pinned extension via k3s-server's iptables/conntrack chain.
        let (resolved_kver, off_kernel_excludes) = if has_overrides_or_packages {
            // Extensions inherit the top-level kernel.version (they're not
            // runtime-scoped), so pass None for runtime_name.
            let mut resolve_params = ResolveParams {
                container_helper,
                container_image,
                target,
                sysroot: sysroot.clone(),
                runtime_name: None,
                config,
                lock_file,
                repo_url: repo_url.map(|s| s.as_str()),
                repo_release: repo_release.map(|s| s.as_str()),
                feeds,
                merged_container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                runs_on_context,
                sdk_arch: self.sdk_arch.as_ref(),
                verbose: self.verbose,
                tui_context: effective_tui_context.clone(),
            };
            let kver = resolve_and_pin_kernel_version(&mut resolve_params).await?;
            let excludes = match kver.as_deref() {
                Some(k) => off_kernel_dnf_excludes(&resolve_params, k).await?,
                None => Vec::new(),
            };
            (kver, excludes)
        } else {
            (None, Vec::new())
        };

        // The resolver pinned in memory only. The lock is saved once, at the
        // end of a successful install: a save before the clean and the dnf
        // transaction below would let an interrupted run claim the new kernel
        // over the old sysroot, and the next run would then see no change to
        // clean. On any failure the in-memory lock is dropped and the one on
        // disk still says what the sysroot actually holds.

        // Does the sysroot exist? Decides whether a first pin counts as a
        // change (a cleared pin over a populated sysroot) and, further down,
        // whether a fresh rpmdb has to be seeded.
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: format!("[ -d $AVOCADO_EXT_SYSROOTS/{extension} ]"),
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: repo_url.cloned(),
            feeds: feeds.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            tui_context: effective_tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };
        let sysroot_existed =
            run_container_command(container_helper, run_config, runs_on_context).await?;

        // dnf is additive: a re-install after a kernel pin change would land
        // the new kernel's module packages *alongside* the old pin's, leaving
        // /lib/modules/<old-kver>/ and stale module packages in the sysroot.
        // Same rule as rootfs/initramfs: a changed pin means a clean sysroot.
        let kernel_pin_change = Self::kernel_pin_change(
            prev_pinned_kver.as_deref(),
            resolved_kver.as_deref(),
            sysroot_existed,
        );
        if let Some((prev, new_kver)) = &kernel_pin_change {
            print_info(
                &format!(
                    "Extension '{extension}': kernel pin changed ({prev} -> {new_kver}); cleaning sysroot for fresh install"
                ),
                OutputLevel::Normal,
            );
            // The package pins name exact builds from the old kernel's
            // install; asked for again in the fresh sysroot, a rolling feed
            // may no longer carry them. Same as `clear_rootfs` on the rootfs
            // path: a wiped sysroot gets a wiped package map.
            lock_file.clear_sysroot_packages(target, &sysroot);
        }

        // Detect package removals: compare current config packages with lock file.
        // If packages were removed, we must clean the sysroot and reinstall from scratch
        // because DNF install is additive-only and cannot remove packages.
        let needs_clean_reinstall = self.detect_package_removals(
            parsed,
            extension,
            ext_location,
            config,
            target,
            lock_file,
        );

        // The rpmdb seed is applied only when the sysroot is created, so a
        // surviving sysroot keeps whatever it was seeded from originally.
        //
        // That silently defeats de-duplication: an extension first built
        // before it had a dependency — or before its dependency changed —
        // keeps a rootfs-seeded rpmdb, dnf sees the shared packages as absent,
        // and installs a private copy again. The seed source is part of the
        // sysroot's identity, so any reason to re-seed is a reason to recreate.
        //
        // Reaching here at all means the stamp was already judged stale (or
        // stamps are off), so rebuilding a dependent is not extra work in the
        // steady state.
        let reseed_required = !direct_deps.is_empty();
        let clean_first =
            needs_clean_reinstall || self.force || reseed_required || kernel_pin_change.is_some();
        if clean_first {
            // Clean the sysroot so it will be recreated fresh below, and drop
            // the stamps that vouch for what was in it. `ext build`'s output —
            // the extension-release files, unit wiring, the overlay — lives in
            // this sysroot and is not reinstalled by the dnf transaction that
            // follows; only `ext build` puts it back. Its stamp's inputs are
            // unchanged by a clean, so leaving the stamp behind lets `ext build`
            // report "up to date" over a sysroot that no longer holds its work,
            // and `ext image` then images an empty extension. Same rule as
            // `--no-stamps`: a step that destroys an output invalidates the
            // stamp that claims it exists.
            let clean_command = clean_ext_sysroot_command(extension);

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: clean_command,
                verbose: self.verbose,
                source_environment: false,
                interactive: false,
                repo_url: repo_url.cloned(),
                feeds: feeds.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                tui_context: effective_tui_context.clone(),
                env_vars: self.runtime_env_vars(),
                ..Default::default()
            };
            // `rm -rf` on a missing sysroot is a no-op; a failure here is a
            // real one (container did not start, read-only or busy mount).
            // Installing over it anyway would add the new kernel's packages
            // next to the old contents and then stamp the result current, so
            // stop instead; the lock on disk still names the old pin and the
            // next run detects the change again.
            let clean_ok =
                run_container_command(container_helper, run_config, runs_on_context).await?;
            if !clean_ok {
                return Err(anyhow::anyhow!(
                    "Failed to clean sysroot for extension '{extension}'; refusing to install over it"
                ));
            }
        }
        // A cleaned sysroot is gone by construction; otherwise it is as probed.
        let sysroot_exists = sysroot_existed && !clean_first;

        // Seed this extension's rpmdb — the mechanism that de-duplicates
        // shared packages.
        //
        // An extension's image is the *net-new files* over whatever its rpmdb
        // already claims is installed. Seeding from the rootfs alone means a
        // dependency's packages look absent, so dnf installs a second private
        // copy and both images ship it. Seeding from the dependency's sysroot
        // instead makes those packages look present, and dnf omits them.
        //
        // A chain composes naturally: mid seeds from base (rootfs ∪ base),
        // app-b then seeds from mid (rootfs ∪ base ∪ mid). Topological install
        // order guarantees the dependency's sysroot is already populated.
        let seed_source = match direct_deps {
            [] => "$AVOCADO_PREFIX/rootfs".to_string(),
            [only] => format!("$AVOCADO_EXT_SYSROOTS/{only}"),
            [first, rest @ ..] => {
                // True diamond. Seeding from one dependency still de-duplicates
                // that branch; packages unique to the others are not yet
                // subtracted, so they ship twice. Correct, just not optimal —
                // say so rather than let it look fully deduplicated.
                print_warning(
                    &format!(
                        "Extension '{extension}' depends on {} extensions; \
                         de-duplicating against '{first}' only. Packages unique to {} \
                         may be duplicated in this image.",
                        direct_deps.len(),
                        rest.join(", ")
                    ),
                    OutputLevel::Normal,
                );
                format!("$AVOCADO_EXT_SYSROOTS/{first}")
            }
        };
        if self.verbose && !direct_deps.is_empty() {
            print_info(
                &format!("Seeding '{extension}' rpmdb from {seed_source}"),
                OutputLevel::Normal,
            );
        }
        let setup_command = format!(
            "mkdir -p $AVOCADO_EXT_SYSROOTS/{extension}/var/lib && cp -rf {seed_source}/var/lib/rpm $AVOCADO_EXT_SYSROOTS/{extension}/var/lib"
        );

        if !sysroot_exists {
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: setup_command,
                verbose: self.verbose,
                source_environment: false,
                interactive: false,
                repo_url: repo_url.cloned(),
                feeds: feeds.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                tui_context: effective_tui_context.clone(),
                env_vars: self.runtime_env_vars(),
                ..Default::default()
            };
            let success =
                run_container_command(container_helper, run_config, runs_on_context).await?;

            if success {
                print_success(
                    &format!("Created sysroot for extension '{extension}'."),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!("Failed to create sysroot for extension '{extension}'."),
                    OutputLevel::Normal,
                );
                return Ok(false);
            }
        }

        // Apply target/kernel sub-section overrides now that kver is known.
        // Strips override sub-keys and merges matching ones into the parent
        // so consumers below see a flat `packages: { ... }` map with
        // kernel-conditional entries already folded in.
        let ext_config = raw_ext_config.as_ref().map(|ec| {
            config.resolve_overrides_in_value(
                ec.clone(),
                target,
                resolved_kver.as_deref(),
                &format!("extensions.{extension}"),
            )
        });

        // Install dependencies if they exist
        let dependencies = ext_config.as_ref().and_then(|ec| ec.get("packages"));

        if let Some(serde_yaml::Value::Mapping(deps_map)) = dependencies {
            // Build list of packages to install and handle extension dependencies
            let mut packages = Vec::new();
            let mut package_names = Vec::new();
            let mut extension_dependencies = Vec::new();

            for (package_name_val, version_spec) in deps_map {
                // Convert package name from Value to String
                let package_name = match package_name_val.as_str() {
                    Some(name) => name,
                    None => continue, // Skip if package name is not a string
                };

                let resolved_name = match resolved_kver.as_deref() {
                    Some(kver) => substitute_kernel_version(package_name, kver),
                    None => package_name.to_string(),
                };

                // Handle different dependency types based on value format
                match version_spec {
                    // Simple string version: "package: version" or "package: '*'"
                    // These are always package repository dependencies
                    serde_yaml::Value::String(version) => {
                        let package_spec = build_package_spec_with_lock(
                            lock_file,
                            target,
                            &sysroot,
                            &resolved_name,
                            version,
                        );
                        packages.push(package_spec);
                        package_names.push(package_name.to_string());
                    }
                    // Object/mapping value: need to check what type of dependency
                    serde_yaml::Value::Mapping(spec_map) => {
                        // Skip compile dependencies - these are SDK-compiled, not from repo
                        // Format: { compile: "section-name", install: "script.sh" }
                        if spec_map.get("compile").is_some() {
                            if self.verbose {
                                print_debug(
                                    &format!("Skipping compile dependency '{package_name}' (SDK-compiled, not from repo)"),
                                    OutputLevel::Normal,
                                );
                            }
                            continue;
                        }

                        // Check for extension dependency
                        // Format: { ext: "extension-name" } or { ext: "name", config: "path" } or { ext: "name", vsn: "version" }
                        if let Some(ext_name) = spec_map.get("extensions").and_then(|v| v.as_str())
                        {
                            // Check if this is a versioned extension (has vsn field)
                            if let Some(version) = spec_map.get("vsn").and_then(|v| v.as_str()) {
                                extension_dependencies
                                    .push((ext_name.to_string(), Some(version.to_string())));
                                if self.verbose {
                                    print_info(
                                        &format!("Found versioned extension dependency: {ext_name} version {version}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            }
                            // Check if this is an external extension (has config field)
                            else if let Some(config_path) =
                                spec_map.get("config").and_then(|v| v.as_str())
                            {
                                extension_dependencies.push((ext_name.to_string(), None));
                                if self.verbose {
                                    print_info(
                                        &format!("Found external extension dependency: {ext_name} from config {config_path}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            } else {
                                // Local extension
                                extension_dependencies.push((ext_name.to_string(), None));
                                if self.verbose {
                                    print_info(
                                        &format!("Found local extension dependency: {ext_name}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            }
                            continue; // Skip adding to packages list
                        }

                        // Check for explicit version in object format
                        // Format: { version: "1.0.0" }
                        if let Some(serde_yaml::Value::String(version)) = spec_map.get("version") {
                            let package_spec = build_package_spec_with_lock(
                                lock_file,
                                target,
                                &sysroot,
                                &resolved_name,
                                version,
                            );
                            packages.push(package_spec);
                            package_names.push(package_name.to_string());
                        }
                        // If it's a mapping without compile, ext, or version keys, skip it
                        // (unknown format)
                    }
                    _ => {}
                }
            }

            // Handle extension dependencies first
            if !extension_dependencies.is_empty() {
                if self.verbose {
                    print_info(
                        &format!("Extension '{extension}' has {} extension dependencies that need to be installed first", extension_dependencies.len()),
                        OutputLevel::Normal,
                    );
                }

                // Note: Extension dependencies should be handled by the main install command
                // or by recursive calls to ExtInstallCommand for each dependency.
                // For now, we'll log them but not install them directly to avoid circular dependencies.
                for (ext_name, version) in &extension_dependencies {
                    if let Some(ver) = version {
                        print_info(
                            &format!("Extension dependency: {ext_name} (version {ver}) - should be installed via main install command"),
                            OutputLevel::Normal,
                        );
                    } else {
                        print_info(
                            &format!("Extension dependency: {ext_name} - should be installed via main install command"),
                            OutputLevel::Normal,
                        );
                    }
                }
            }

            if !packages.is_empty() {
                // Build DNF install command
                // dnf never prompts here: this applies the package set avocado.yaml and
                // avocado.lock already declare, so there is no decision left to make.
                // `sdk dnf` / `ext dnf` / `runtime dnf` are the interactive path.
                let yes = "-y";
                let installroot = format!("$AVOCADO_EXT_SYSROOTS/{extension}");
                let dnf_args_str = if let Some(args) = &self.dnf_args {
                    format!(" {} ", args.join(" "))
                } else {
                    String::new()
                };
                let exclude_str = if off_kernel_excludes.is_empty() {
                    String::new()
                } else {
                    format!(" {} ", off_kernel_excludes.join(" "))
                };
                let command = format!(
                    r#"
RPM_NO_CHROOT_FOR_SCRIPTS=1 \
AVOCADO_EXT_INSTALLROOT={} \
PATH=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin:$PATH \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot={} \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    {} \
    install \
    {} \
    {}
"#,
                    installroot,
                    installroot,
                    dnf_args_str,
                    exclude_str,
                    yes,
                    packages.join(" ")
                );

                if self.verbose {
                    print_info(&format!("Running command: {command}"), OutputLevel::Normal);
                }

                // Run the DNF install command
                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target.to_string(),
                    command,
                    verbose: self.verbose,
                    source_environment: false, // don't source environment
                    // dnf runs with -y, so nothing here can prompt: no PTY, ever.
                    interactive: false,
                    repo_url: repo_url.cloned(),
                    feeds: feeds.cloned(),
                    repo_release: repo_release.cloned(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    disable_weak_dependencies,
                    // runs_on handled by shared context
                    sdk_arch: self.sdk_arch.clone(),
                    tui_context: effective_tui_context.clone(),
                    env_vars: self.runtime_env_vars(),
                    ..Default::default()
                };
                let install_success =
                    run_container_command(container_helper, run_config, runs_on_context).await?;

                if !install_success {
                    print_error(
                        &format!("Failed to install dependencies for extension '{extension}'."),
                        OutputLevel::Normal,
                    );
                    return Ok(false);
                }

                // Query installed versions and update lock file
                if !package_names.is_empty() {
                    let installed_versions = container_helper
                        .query_installed_packages(
                            &sysroot,
                            &package_names,
                            container_image,
                            target,
                            repo_url.cloned(),
                            repo_release.cloned(),
                            merged_container_args.clone(),
                            runs_on_context,
                            self.sdk_arch.as_ref(),
                            self.runtime_env_vars(),
                        )
                        .await?;

                    if !installed_versions.is_empty() {
                        // Single-write to the sysroot computed by
                        // `extension_sysroot()` — runtime-scoped when a
                        // runtime is in scope, legacy global namespace
                        // otherwise.
                        lock_file.update_sysroot_versions(target, &sysroot, installed_versions);
                        // And where they came from, which matters most here:
                        // extensions are the stage most likely to draw from a
                        // second feed. Best effort — dnf answers from the
                        // installroot's own history, and a lock records a bare
                        // version when it cannot.
                        let origins = container_helper
                            .query_installed_origins(
                                &sysroot,
                                container_image,
                                target,
                                repo_url.cloned(),
                                repo_release.cloned(),
                                merged_container_args.clone(),
                                runs_on_context,
                                self.sdk_arch.as_ref(),
                                self.runtime_env_vars(),
                            )
                            .await;
                        lock_file.set_sysroot_origins(target, &sysroot, &origins);
                        if self.verbose {
                            print_info(
                                &format!("Updated lock file with extension '{extension}' package versions."),
                                OutputLevel::Normal,
                            );
                        }
                    }
                }
            } else if self.verbose {
                print_debug(
                    &format!("No valid dependencies found for extension '{extension}'."),
                    OutputLevel::Normal,
                );
            }
        } else if let Some(deps_value) = dependencies {
            // packages field exists but is not a YAML mapping — detect common syntax mistakes
            if !deps_value.is_null() {
                let value_str =
                    serde_yaml::to_string(deps_value).unwrap_or_else(|_| format!("{deps_value:?}"));
                let hint = if value_str.contains('=') {
                    "\n\nIt looks like '=' was used instead of ':'. YAML uses ':' for key-value pairs.\n\
                     Example:\n  packages:\n    curl: \"*\"\n    iperf3: \"*\""
                } else {
                    "\n\nExpected a YAML mapping (key: value pairs).\n\
                     Example:\n  packages:\n    curl: \"*\"\n    iperf3: \"*\""
                };
                return Err(anyhow::anyhow!(
                    "Invalid 'packages' format in extension '{extension}': \
                     expected a mapping but got: {}{hint}",
                    value_str.trim()
                ));
            }
        } else if self.verbose {
            print_debug(
                &format!("No dependencies defined for extension '{extension}'."),
                OutputLevel::Normal,
            );
        }

        // The one save: kernel pin, package versions and origins land together,
        // and only once the sysroot holds what they describe.
        lock_file.save(src_dir)?;
        Ok(true)
    }
}

/// Helper function to run a container command, using shared context if available
async fn run_container_command(
    container_helper: &SdkContainer,
    config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
) -> Result<bool> {
    if let Some(context) = runs_on_context {
        container_helper
            .run_in_container_with_context(&config, context)
            .await
    } else {
        container_helper.run_in_container(config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_only_accepts_shell_safe_extension_names() {
        // Ordinary names the fast path may interpolate directly.
        for ok in ["foo", "my-ext", "my_ext", "ext.v2", "avocado-dev", "a1"] {
            assert!(ext_name_stamp_safe(ok), "{ok} should be accepted");
        }
        // Injection / protocol-breaking names must fall through to a normal
        // install instead of being skipped via the batch script.
        for bad in [
            "",
            "ext name",          // splits the printf/path
            "ext\"; rm -rf /\"", // quote break + command
            "$(reboot)",         // command substitution
            "`id`",              // backtick substitution
            "a/b",               // path traversal into the stamp path
            "ext\nrm",           // newline breaks the line protocol
            "ext:::x",           // collides with the ':::' delimiter
        ] {
            assert!(!ext_name_stamp_safe(bad), "{bad:?} should be rejected");
        }
    }

    /// Clearing the sysroot must take the build and image stamps with it.
    /// Without this, `install --force` leaves a sysroot holding only package
    /// state while `ext build`'s stamp still reads current — the skip then
    /// fires and `ext image` ships an extension with no content in it.
    #[test]
    fn clean_ext_sysroot_drops_the_stamps_that_vouch_for_its_contents() {
        let cmd = clean_ext_sysroot_command("app");
        assert!(cmd.contains(r#"rm -rf "$AVOCADO_EXT_SYSROOTS/app""#));
        assert!(cmd.contains(r#"rm -f "$AVOCADO_PREFIX/.stamps/"'ext/app/build.stamp'"#));
        assert!(cmd.contains(r#"rm -f "$AVOCADO_PREFIX/.stamps/"'ext/app/image.stamp'"#));
        // The install stamp is rewritten by the install that follows; removing
        // it here would be harmless but is not this function's job.
        assert!(!cmd.contains("install.stamp"));
    }

    /// `--dnf-arg` and `sdk.disable_weak_dependencies` change what dnf resolves
    /// and neither reaches the install hash, so the stamp written after one
    /// must say so. Both the fast path and the writer read this, so a drift
    /// between them would either skip a differently-resolved sysroot or mark
    /// every ordinary install.
    #[test]
    fn only_options_the_hash_cannot_see_make_a_transaction_nonstandard() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(transaction_is_standard(None, false));
        assert!(transaction_is_standard(Some(&[]), false));
        assert!(!transaction_is_standard(Some(&args(&["--refresh"])), false));
        assert!(!transaction_is_standard(None, true));
        assert!(!transaction_is_standard(Some(&args(&["--refresh"])), true));
    }

    /// An install run with `--dnf-arg` (or weak dependencies disabled) still
    /// leaves an install stamp behind. It used to delete one instead, which
    /// made such a project permanently unbuildable: `ext build` hard-requires
    /// `ext/<name>/install.stamp`, and the fix it advertised
    /// (`avocado ext install <name>`) deleted the stamp again on every run.
    /// The stamp is marked instead, and only this command's fast path reads
    /// the mark.
    #[test]
    fn a_nonstandard_transaction_marks_the_install_stamp_instead_of_deleting_it() {
        use crate::utils::stamps::{generate_write_stamp_script, StampInputs};
        let inputs = StampInputs::new("h".to_string());
        let marked = Stamp::ext_install(
            "app",
            "t",
            inputs.clone(),
            StampOutputs {
                nonstandard_options: true,
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&marked).unwrap();

        // The stamp is written, and says how it was written.
        let script = generate_write_stamp_script(&marked).unwrap();
        assert!(script.contains(r#""$AVOCADO_PREFIX/.stamps/"'ext/app/install.stamp'"#));
        assert!(script.contains("\"nonstandard_options\": true"));

        // `ext build`'s validator accepts it -- the sysroot is installed.
        let req = StampRequirement::ext_install("app");
        assert!(matches!(
            validate_stamp(&req, Some(&json), Some(&inputs)),
            StampStatus::Current(_)
        ));
        // ...but a later plain install must not skip over it.
        assert!(!stamp_allows_skip(&validate_stamp(
            &req,
            Some(&json),
            Some(&inputs)
        )));

        // A standard transaction's stamp still takes the fast path, and a
        // stamp written before the field existed reads as standard.
        let plain = Stamp::ext_install("app", "t", inputs.clone(), StampOutputs::default());
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(!plain_json.contains("nonstandard_options"));
        assert!(stamp_allows_skip(&validate_stamp(
            &req,
            Some(&plain_json),
            Some(&inputs)
        )));
    }

    /// `ext build`'s input hash is config-only, so nothing in it moves when a
    /// transaction changes the sysroot without changing the config. Its skip
    /// would then fire over content that changed, and `ext image` would chain
    /// off the stale digest and ship the previous build. Dropping both stamps
    /// is what `clean_ext_sysroot` already does for the same reason.
    ///
    /// Not gated on the transaction being non-standard. That gate covered
    /// entering a non-standard install and not leaving one: a `--dnf-arg`
    /// install builds a lean sysroot, the build records stamps for it, and the
    /// next plain install re-resolves the sysroot fatter while running no
    /// cleanup, because that transaction is standard.
    #[test]
    fn an_install_that_runs_drops_the_stamps_that_vouch_for_the_sysroot() {
        let script = drop_content_stamps_script("app");
        assert!(script.contains(r#"rm -f "$AVOCADO_PREFIX/.stamps/"'ext/app/build.stamp'"#));
        assert!(script.contains(r#"rm -f "$AVOCADO_PREFIX/.stamps/"'ext/app/image.stamp'"#));
        // The install stamp is written right after this, and is what `ext
        // build` needs to run at all -- removing it is the bug being fixed.
        assert!(!script.contains("install.stamp"));
    }

    /// A move between two pins cleans the sysroot, and so does a first pin
    /// over a sysroot that already exists -- `avocado update` and `clean
    /// --unlock` erase the pins and keep the sysroots. A first pin over no
    /// sysroot has nothing stale to remove, and an extension that resolves no
    /// kernel never had kernel-family packages to begin with.
    #[test]
    fn a_kernel_pin_change_is_a_move_or_a_first_pin_over_a_populated_sysroot() {
        let change = |p, n, exists| ExtInstallCommand::kernel_pin_change(p, n, exists);
        assert_eq!(
            change(Some("6.6.5"), Some("6.6.6"), true),
            Some(("6.6.5".to_string(), "6.6.6".to_string()))
        );
        assert_eq!(change(Some("6.6.5"), Some("6.6.5"), true), None);
        // Cleared pin, populated sysroot: the `avocado update` path.
        assert_eq!(
            change(None, Some("6.6.5"), true),
            Some(("none".to_string(), "6.6.5".to_string()))
        );
        // Cleared pin, no sysroot: a plain first install.
        assert_eq!(change(None, Some("6.6.5"), false), None);
        assert_eq!(change(Some("6.6.5"), None, true), None);
    }
}
