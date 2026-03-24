//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::commands::rootfs::install::{install_sysroot, SysrootInstallParams};
use crate::utils::{
    config::{find_active_compile_sections, find_active_extensions, ComposedConfig, Config},
    container::{normalize_sdk_arch, RunConfig, SdkContainer, TuiContext},
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    stamps::{
        compute_compile_deps_input_hash, compute_sdk_input_hash,
        generate_write_sdk_stamp_script_dynamic_arch, generate_write_stamp_script, get_local_arch,
        Stamp, StampOutputs,
    },
    target::validate_and_log_target,
    tui::{TaskId, TaskStatus},
};

/// Data produced by the bootstrap phase, consumed by the parallel install phase.
struct BootstrapResult {
    /// Reloaded composed config (after fetching remote extensions)
    composed: ComposedConfig,
    /// Extension SDK dependencies filtered to active extensions
    extension_sdk_dependencies: HashMap<String, HashMap<String, serde_yaml::Value>>,
    /// SDK dependencies from the config
    sdk_dependencies: Option<HashMap<String, serde_yaml::Value>>,
    /// All SDK package names installed during bootstrap (to be extended by SDK packages phase)
    all_sdk_package_names: Vec<String>,
    /// SDK sysroot type with host architecture
    sdk_sysroot: SysrootType,
    /// Lock file (to be cloned for each parallel task)
    lock_file: LockFile,
    /// Active extensions after config reload
    active_extensions: HashSet<String>,
    /// Resolved src_dir for lock file save path
    src_dir: PathBuf,
}

/// Implementation of the 'sdk install' command.
pub struct SdkInstallCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Disable stamp validation and writing
    pub no_stamps: bool,
    /// Remote host to run on (format: user@host)
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
    pub tui_context: Option<TuiContext>,
}

impl SdkInstallCommand {
    /// Create a new SdkInstallCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
            tui_context: None,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
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

    /// Execute the sdk install command
    pub async fn execute(&mut self) -> Result<()> {
        let _standalone_tui = if self.tui_context.is_none() && self.force {
            crate::utils::tui::create_standalone_tui(
                TaskId::SdkInstall,
                "sdk install",
                self.verbose,
            )
        } else {
            None
        };
        // Use either the provided tui_context or the standalone one
        if self.tui_context.is_none() {
            self.tui_context = _standalone_tui.as_ref().map(|(ctx, _)| ctx.clone());
        }

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
        let target = validate_and_log_target(self.target.as_deref(), config)?;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Determine which extensions are active for this target based on runtime configuration
        let active_extensions = find_active_extensions(
            config,
            &composed.merged_value,
            &target,
            &composed.config_path,
            None,
        )?;

        print_info("Installing SDK dependencies.", OutputLevel::Normal);

        // Get SDK dependencies from the composed config (already has external deps merged)
        let sdk_dependencies = config
            .get_sdk_dependencies_for_target(&self.config_path, &target)
            .with_context(|| "Failed to get SDK dependencies with target interpolation")?;

        // Note: extension_sdk_dependencies is computed inside execute_bootstrap after
        // fetching remote extensions, since we need SDK repos to be available first

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Use the container helper to run the installation
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Create shared RunsOnContext if running on remote host
        // This allows reusing the NFS server and volumes for all container runs
        let mut runs_on_context: Option<RunsOnContext> = if let Some(ref runs_on) = self.runs_on {
            Some(
                container_helper
                    .create_runs_on_context(runs_on, self.nfs_port, container_image, self.verbose)
                    .await?,
            )
        } else {
            None
        };

        // Phase 1: Bootstrap — sets up SDK env, installs bootstrap package, fetches extensions
        let bootstrap_result = self
            .execute_bootstrap(
                config,
                &target,
                container_image,
                &sdk_dependencies,
                repo_url.as_deref(),
                repo_release.as_deref(),
                &container_helper,
                merged_container_args.as_ref(),
                runs_on_context.as_ref(),
                &active_extensions,
            )
            .await;

        // On bootstrap failure, teardown and return early
        let bootstrap = match bootstrap_result {
            Ok(b) => b,
            Err(e) => {
                if let Some(ref mut context) = runs_on_context {
                    if let Err(te) = context.teardown().await {
                        print_error(
                            &format!("Warning: Failed to cleanup remote resources: {te}"),
                            OutputLevel::Normal,
                        );
                    }
                }
                return Err(e);
            }
        };

        // Phase 2: Run SDK packages, rootfs, initramfs, and target-dev in parallel.
        // Each task gets its own lock_file clone since they write to different sections.
        let result = self
            .execute_parallel_phase(
                &bootstrap,
                container_image,
                repo_url.as_deref(),
                repo_release.as_deref(),
                &container_helper,
                merged_container_args.as_ref(),
                runs_on_context.as_ref(),
                &target,
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
            if let Some((ref ctx, ref renderer)) = _standalone_tui {
                renderer.set_status(&ctx.task_id, TaskStatus::Success);
                renderer.shutdown();
            }
        }

        result
    }

    /// Phase 2: Run SDK packages, rootfs, initramfs, and target-dev in parallel
    #[allow(clippy::too_many_arguments)]
    async fn execute_parallel_phase(
        &self,
        bootstrap: &BootstrapResult,
        container_image: &str,
        repo_url: Option<&str>,
        repo_release: Option<&str>,
        container_helper: &SdkContainer,
        merged_container_args: Option<&Vec<String>>,
        runs_on_context: Option<&RunsOnContext>,
        target: &str,
    ) -> Result<()> {
        let composed = &bootstrap.composed;
        let config = &composed.config;
        let active_extensions = &bootstrap.active_extensions;
        let lock_file = &bootstrap.lock_file;
        let src_dir = &bootstrap.src_dir;

        // Discover whether target-dev sysroot is needed (compile sections from
        // fetched external extensions). Prepare the command BEFORE launching
        // the parallel sysroot installs so all four can run concurrently.
        let active_compile_sections =
            find_active_compile_sections(&composed.merged_value, active_extensions);
        let need_target_dev = config.has_compile_sections() && !active_compile_sections.is_empty();

        // Prepare target-dev install command if needed (CPU-only prep, no container calls)
        let target_dev_command = if need_target_dev {
            let compile_dependencies = config.get_compile_dependencies();
            let mut all_compile_packages: Vec<String> = Vec::new();
            let mut all_compile_package_names: Vec<String> = Vec::new();
            for section_name in &active_compile_sections {
                if let Some(dependencies) = compile_dependencies.get(section_name) {
                    let packages = self.build_package_list_with_lock(
                        dependencies,
                        lock_file,
                        target,
                        &SysrootType::TargetSysroot,
                    );
                    all_compile_packages.extend(packages);
                    all_compile_package_names.extend(self.extract_package_names(dependencies));
                }
            }
            all_compile_packages.sort();
            all_compile_packages.dedup();
            all_compile_package_names.sort();
            all_compile_package_names.dedup();

            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };
            let target_sysroot_base_pkg = "avocado-sdk-target-sysroot";
            let target_sysroot_config_version = "*";
            let target_sysroot_pkg = build_package_spec_with_lock(
                lock_file,
                target,
                &SysrootType::TargetSysroot,
                target_sysroot_base_pkg,
                target_sysroot_config_version,
            );

            let command = format!(
                r#"
unset RPM_CONFIGDIR
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_TARGET_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} {} --installroot ${{AVOCADO_PREFIX}}/sdk/target-sysroot \
    install {} {}
"#,
                dnf_args_str,
                yes,
                target_sysroot_pkg,
                all_compile_packages.join(" ")
            );

            Some((
                command,
                all_compile_package_names,
                target_sysroot_base_pkg.to_string(),
            ))
        } else {
            None
        };

        // Register parallel tasks on TUI and signal status transitions.
        if let Some(r) = crate::utils::tui::get_active_renderer() {
            r.set_status(&TaskId::SdkInstall, TaskStatus::Success);
            r.register_task(TaskId::SdkPackages, "sdk packages".to_string());
            r.set_status(&TaskId::SdkPackages, TaskStatus::Running);
            r.set_status(&TaskId::RootfsInstall, TaskStatus::Running);
            r.set_status(&TaskId::InitramfsInstall, TaskStatus::Running);
            if need_target_dev {
                r.register_task(TaskId::TargetDevInstall, "target-dev install".to_string());
                r.set_status(&TaskId::TargetDevInstall, TaskStatus::Running);
            }
        }

        // Clone lock files for each parallel task (they write to different sections)
        let mut sdk_pkg_lock = lock_file.clone();
        let mut rootfs_lock = lock_file.clone();
        let mut initramfs_lock = lock_file.clone();
        #[allow(unused_variables)]
        let target_dev_lock = lock_file.clone();

        // Build SDK packages future
        let sdk_pkg_fut = self.install_sdk_packages(
            config,
            &bootstrap.sdk_dependencies,
            &bootstrap.extension_sdk_dependencies,
            &mut sdk_pkg_lock,
            &bootstrap.all_sdk_package_names,
            &bootstrap.sdk_sysroot,
            container_image,
            target,
            repo_url,
            repo_release,
            container_helper,
            merged_container_args,
            runs_on_context,
        );

        // Build sysroot install params
        // Build TUI contexts for each sysroot task
        let rootfs_tui = self.tui_context.as_ref().map(|ctx| TuiContext {
            task_id: TaskId::RootfsInstall,
            renderer: ctx.renderer.clone(),
        });
        let initramfs_tui = self.tui_context.as_ref().map(|ctx| TuiContext {
            task_id: TaskId::InitramfsInstall,
            renderer: ctx.renderer.clone(),
        });

        let mut rootfs_params = SysrootInstallParams {
            sysroot_type: SysrootType::Rootfs,
            config,
            lock_file: &mut rootfs_lock,
            src_dir,
            container_helper,
            container_image,
            target,
            repo_url,
            repo_release,
            merged_container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            verbose: self.verbose,
            force: self.force,
            runs_on_context,
            sdk_arch: self.sdk_arch.as_ref(),
            no_stamps: self.no_stamps,
            parsed: Some(&composed.merged_value),
            tui_context: rootfs_tui,
        };
        let mut initramfs_params = SysrootInstallParams {
            sysroot_type: SysrootType::Initramfs,
            config,
            lock_file: &mut initramfs_lock,
            src_dir,
            container_helper,
            container_image,
            target,
            repo_url,
            repo_release,
            merged_container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            verbose: self.verbose,
            force: self.force,
            runs_on_context,
            sdk_arch: self.sdk_arch.as_ref(),
            no_stamps: self.no_stamps,
            parsed: Some(&composed.merged_value),
            tui_context: initramfs_tui,
        };

        // Build the target-dev future (or a no-op if not needed)
        let target_dev_tui_ctx = if need_target_dev {
            self.tui_context.as_ref().map(|ctx| TuiContext {
                task_id: TaskId::TargetDevInstall,
                renderer: ctx.renderer.clone(),
            })
        } else {
            None
        };

        let target_dev_fut = async {
            if let Some((ref cmd, _, _)) = target_dev_command {
                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target.to_string(),
                    command: cmd.clone(),
                    verbose: self.verbose,
                    source_environment: false,
                    interactive: !self.force,
                    repo_url: repo_url.map(|s| s.to_string()),
                    repo_release: repo_release.map(|s| s.to_string()),
                    container_args: merged_container_args.cloned(),
                    dnf_args: self.dnf_args.clone(),
                    disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                    tui_context: target_dev_tui_ctx.clone(),
                    ..Default::default()
                };
                let success = run_container_command(
                    container_helper,
                    run_config,
                    runs_on_context,
                    self.sdk_arch.as_ref(),
                )
                .await?;
                if success {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "Failed to install target-sysroot with compile dependencies."
                    ))
                }
            } else {
                Ok(())
            }
        };

        // Helper: wrap a future so it sets TUI status immediately on completion.
        macro_rules! with_tui_status {
            ($fut:expr, $task_id:expr) => {
                async {
                    let result = $fut.await;
                    if let Some(r) = crate::utils::tui::get_active_renderer() {
                        if result.is_ok() {
                            r.set_status(&$task_id, TaskStatus::Success);
                        } else {
                            r.set_status(&$task_id, TaskStatus::Failed);
                        }
                    }
                    result
                }
            };
        }

        // Run all four tasks in parallel — each updates TUI immediately on completion
        let (sdk_pkg_result, rootfs_result, initramfs_result, target_dev_result) = tokio::join!(
            with_tui_status!(sdk_pkg_fut, TaskId::SdkPackages),
            with_tui_status!(install_sysroot(&mut rootfs_params), TaskId::RootfsInstall),
            with_tui_status!(
                install_sysroot(&mut initramfs_params),
                TaskId::InitramfsInstall
            ),
            with_tui_status!(target_dev_fut, TaskId::TargetDevInstall),
        );

        // Merge lock file changes back into a single lock file for saving
        let mut final_lock = lock_file.clone();

        // Merge SDK packages lock
        if sdk_pkg_result.is_ok() {
            if let Some(target_locks) = sdk_pkg_lock.targets.get(target) {
                let entry = final_lock.targets.entry(target.to_string()).or_default();
                entry.sdk = target_locks.sdk.clone();
            }
        }
        // Merge rootfs lock
        if rootfs_result.is_ok() {
            if let Some(target_locks) = rootfs_lock.targets.get(target) {
                final_lock
                    .targets
                    .entry(target.to_string())
                    .or_default()
                    .rootfs = target_locks.rootfs.clone();
            }
        }
        // Merge initramfs lock
        if initramfs_result.is_ok() {
            if let Some(target_locks) = initramfs_lock.targets.get(target) {
                final_lock
                    .targets
                    .entry(target.to_string())
                    .or_default()
                    .initramfs = target_locks.initramfs.clone();
            }
        }
        // Merge target-dev lock
        if need_target_dev && target_dev_result.is_ok() {
            if let Some(target_locks) = target_dev_lock.targets.get(target) {
                final_lock
                    .targets
                    .entry(target.to_string())
                    .or_default()
                    .target_sysroot = target_locks.target_sysroot.clone();
            }
            // Post-install: query versions and update lock file
            if let Some((_, ref all_compile_package_names, ref base_pkg)) = target_dev_command {
                print_success(
                    "Installed target-sysroot with compile dependencies.",
                    OutputLevel::Normal,
                );
                let mut packages_to_query = all_compile_package_names.clone();
                packages_to_query.push(base_pkg.clone());

                let installed_versions = container_helper
                    .query_installed_packages(
                        &SysrootType::TargetSysroot,
                        &packages_to_query,
                        container_image,
                        target,
                        repo_url.map(|s| s.to_string()),
                        repo_release.map(|s| s.to_string()),
                        merged_container_args.cloned(),
                        runs_on_context,
                        self.sdk_arch.as_ref(),
                    )
                    .await?;

                if !installed_versions.is_empty() {
                    final_lock.update_sysroot_versions(
                        target,
                        &SysrootType::TargetSysroot,
                        installed_versions,
                    );
                }
            }
        }
        final_lock.save(src_dir)?;

        // Propagate errors
        sdk_pkg_result?;
        rootfs_result?;
        initramfs_result?;
        target_dev_result?;

        // Write compile-deps stamp (unless --no-stamps)
        // This tracks which compile dependencies are installed in the target-sysroot.
        // When runtimes change, the active compile sections change, making this stamp stale.
        if !self.no_stamps {
            let compile_inputs =
                compute_compile_deps_input_hash(&composed.merged_value, &active_compile_sections)?;

            let stamp_script = if self.runs_on.is_some() {
                // For remote execution, use dynamic arch detection
                // Build the stamp JSON with a placeholder that gets replaced at runtime
                let outputs = StampOutputs::default();
                let stamp = Stamp::compile_deps_install("DYNAMIC_ARCH", compile_inputs, outputs);
                let stamp_json = stamp.to_json()?;
                // Replace the placeholder with dynamic arch detection
                let stamp_json_escaped = stamp_json.replace('"', "\\\"");
                format!(
                    r#"
HOST_ARCH=$(uname -m)
STAMP_DIR="${{AVOCADO_PREFIX}}/.stamps/sdk/${{HOST_ARCH}}"
mkdir -p "$STAMP_DIR"
STAMP_JSON="{stamp_json_escaped}"
STAMP_JSON=$(echo "$STAMP_JSON" | sed "s/DYNAMIC_ARCH/$HOST_ARCH/g")
echo "$STAMP_JSON" > "$STAMP_DIR/compile-deps.stamp"
echo "[INFO] Wrote compile-deps stamp for arch $HOST_ARCH"
"#
                )
            } else {
                let outputs = StampOutputs::default();
                let host_arch = get_local_arch();
                let stamp = Stamp::compile_deps_install(host_arch, compile_inputs, outputs);
                generate_write_stamp_script(&stamp)?
            };

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                tui_context: self.tui_context.clone(),
                ..Default::default()
            };

            run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if self.verbose {
                print_info("Wrote compile-deps stamp.", OutputLevel::Normal);
            }
        }

        // Write SDK install stamp (unless --no-stamps)
        // The stamp uses the host architecture (CPU arch where SDK runs) rather than
        // the target architecture (what you're building for). This allows --runs-on
        // to detect if the SDK is installed for the remote's architecture.
        if !self.no_stamps {
            let inputs = compute_sdk_input_hash(&composed.merged_value)?;

            // When using --runs-on, we need to detect the remote architecture dynamically
            // since the remote host may have a different CPU arch than the local machine.
            // Otherwise, use the local architecture.
            let stamp_script = if self.runs_on.is_some() {
                // Use dynamic arch detection for remote execution
                generate_write_sdk_stamp_script_dynamic_arch(inputs)
            } else {
                // Use local architecture for local execution
                let outputs = StampOutputs::default();
                let host_arch = get_local_arch();
                let stamp = Stamp::sdk_install(host_arch, inputs, outputs);
                generate_write_stamp_script(&stamp)?
            };

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                // runs_on handled by shared context
                tui_context: self.tui_context.clone(),
                ..Default::default()
            };

            run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if self.verbose {
                print_info("Wrote SDK install stamp.", OutputLevel::Normal);
            }
        }

        Ok(())
    }

    /// Fetch remote extensions after SDK bootstrap
    ///
    /// This discovers extensions with a `source` field and fetches them
    /// using the SDK environment where repos are already configured.
    /// Only fetches extensions that are in the active_extensions set.
    async fn fetch_remote_extensions_in_sdk(
        &self,
        target: &str,
        merged_container_args: Option<&Vec<String>>,
        active_extensions: &std::collections::HashSet<String>,
    ) -> Result<()> {
        use crate::commands::ext::ExtFetchCommand;

        // Discover remote extensions (with target interpolation for extension names)
        let all_remote_extensions =
            Config::discover_remote_extensions(&self.config_path, Some(target))?;

        // Filter to only extensions referenced by active runtimes
        let remote_extensions: Vec<_> = all_remote_extensions
            .into_iter()
            .filter(|(name, _)| active_extensions.contains(name))
            .collect();

        if remote_extensions.is_empty() {
            return Ok(());
        }

        print_info(
            &format!(
                "Fetching {} remote extension(s)...",
                remote_extensions.len()
            ),
            OutputLevel::Normal,
        );

        // Fetch each active remote extension individually
        for (ext_name, _source) in &remote_extensions {
            let mut fetch_cmd = ExtFetchCommand::new(
                self.config_path.clone(),
                Some(ext_name.clone()),
                self.verbose,
                false, // Don't force re-fetch
                Some(target.to_string()),
                merged_container_args.cloned(),
            )
            .with_sdk_arch(self.sdk_arch.clone());

            // Pass through the runs_on context for remote execution
            if let Some(runs_on) = &self.runs_on {
                fetch_cmd = fetch_cmd.with_runs_on(runs_on.clone(), self.nfs_port);
            }

            fetch_cmd.execute().await?;
        }

        Ok(())
    }

    /// Bootstrap phase: sets up SDK environment, installs bootstrap package,
    /// fetches remote extensions, reloads config. Returns data needed for the
    /// parallel install phase.
    #[allow(clippy::too_many_arguments)]
    async fn execute_bootstrap(
        &self,
        config: &Config,
        target: &str,
        container_image: &str,
        sdk_dependencies: &Option<HashMap<String, serde_yaml::Value>>,
        repo_url: Option<&str>,
        repo_release: Option<&str>,
        container_helper: &SdkContainer,
        merged_container_args: Option<&Vec<String>>,
        runs_on_context: Option<&RunsOnContext>,
        initial_active_extensions: &HashSet<String>,
    ) -> Result<BootstrapResult> {
        // Determine host architecture for SDK package tracking
        // Priority: sdk_arch (for cross-arch emulation) > runs_on remote arch > local arch
        let host_arch = if let Some(ref arch) = self.sdk_arch {
            // Convert sdk_arch to normalized architecture name (e.g., "aarch64", "x86_64")
            normalize_sdk_arch(arch)?
        } else if let Some(context) = runs_on_context {
            context
                .get_host_arch()
                .await
                .with_context(|| "Failed to get remote host architecture")?
        } else {
            get_local_arch().to_string()
        };

        // Create SDK sysroot type with the host architecture
        let sdk_sysroot = SysrootType::Sdk(host_arch.clone());

        if self.verbose {
            print_info(
                &format!("Using host architecture '{host_arch}' for SDK package tracking."),
                OutputLevel::Normal,
            );
        }

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

        // Initialize SDK environment first (creates directories, copies configs, sets up wrappers)
        print_info("Initializing SDK environment.", OutputLevel::Normal);

        let sdk_init_command = r#"
echo "[INFO] Initializing Avocado SDK."
mkdir -p $AVOCADO_SDK_PREFIX/etc
mkdir -p $AVOCADO_EXT_SYSROOTS
cp /etc/rpmrc $AVOCADO_SDK_PREFIX/etc
cp -r /etc/rpm $AVOCADO_SDK_PREFIX/etc
cp -r /etc/dnf $AVOCADO_SDK_PREFIX/etc
cp -r /etc/yum.repos.d $AVOCADO_SDK_PREFIX/etc

# Compute the machine-scoped SDK arch (SDKIMGARCH) for this machine+host combination.
# This arch is used by nativesdk packages so that each machine gets independent PR
# revision tracking while sharing the same SDK host arch repo path.
MACHINE_US=$(echo "$AVOCADO_TARGET" | tr '-' '_')
SDK_ARCH_US=$(uname -m | tr '-' '_')
SDKIMGARCH_US="${MACHINE_US}_${SDK_ARCH_US}_avocadosdk"
GENERIC_SDK_ARCH_US="${SDK_ARCH_US}_avocadosdk"

# Append arch compat entries to the rpmrc so RPM will accept SDKIMGARCH packages
# during bootstrap install. RPM_ETCCONFIGDIR points here for the bootstrap dnf call.
echo "arch_compat: ${SDKIMGARCH_US}: all any noarch ${SDK_ARCH_US} ${GENERIC_SDK_ARCH_US} all_avocadosdk ${SDKIMGARCH_US}" >> $AVOCADO_SDK_PREFIX/etc/rpmrc
echo "buildarch_compat: ${SDKIMGARCH_US}: noarch" >> $AVOCADO_SDK_PREFIX/etc/rpmrc

# Prepend the SDKIMGARCH to the dnf arch vars so DNF searches the machine-scoped
# SDK repo for bootstrap packages (varsdir points here for the bootstrap dnf call).
ARCH_FILE=$AVOCADO_SDK_PREFIX/etc/dnf/vars/arch
EXISTING_ARCH=$(cat "$ARCH_FILE" 2>/dev/null || echo "")
if [ -n "$EXISTING_ARCH" ]; then
    echo "${SDKIMGARCH_US}:${EXISTING_ARCH}" > "$ARCH_FILE"
else
    echo "${SDKIMGARCH_US}" > "$ARCH_FILE"
fi

# Update the rpm platform file to SDKIMGARCH so RPM's transaction check accepts
# machine-scoped packages. The platform file determines the host arch for RPM;
# without this, RPM sees x86_64_avocadosdk and rejects qemux86_64_x86_64_avocadosdk
# packages as "intended for a different architecture".
PLATFORM_FILE=$AVOCADO_SDK_PREFIX/etc/rpm/platform
rm -f "$PLATFORM_FILE"
echo "${SDKIMGARCH_US}-avocado-linux" > "$PLATFORM_FILE"

# Restore custom repo URL after copying container defaults (which may overwrite it)
if [ -n "$AVOCADO_SDK_REPO_URL" ]; then
    mkdir -p $AVOCADO_SDK_PREFIX/etc/dnf/vars
    echo "$AVOCADO_SDK_REPO_URL" > $AVOCADO_SDK_PREFIX/etc/dnf/vars/repo_url
fi

mkdir -p $AVOCADO_SDK_PREFIX/usr/lib/rpm
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/usr/lib/rpm/

# Before calling DNF, $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros needs to be updated to point:
#   - /usr -> $AVOCADO_SDK_PREFIX/usr
#   - /var -> $AVOCADO_SDK_PREFIX/var
sed -i "s|^%_usr[[:space:]]*/usr$|%_usr                   $AVOCADO_SDK_PREFIX/usr|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros
sed -i "s|^%_var[[:space:]]*/var$|%_var                   $AVOCADO_SDK_PREFIX/var|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros

# Create separate rpm config for versioned extensions with custom %_dbpath
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/ext-rpm-config/
# Update macros for versioned extensions to use extension.d/rpm database location
sed -i "s|^%_dbpath[[:space:]]*%{_var}/lib/rpm$|%_dbpath                %{_var}/lib/extension.d/rpm|" $AVOCADO_SDK_PREFIX/ext-rpm-config/macros

# Create separate rpm config for extension scriptlets with selective execution
# This allows only update-alternatives and opkg to run, blocking other scriptlet commands
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/

# Create a bin directory for command wrappers
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin

# Create update-alternatives wrapper that uses OPKG_OFFLINE_ROOT
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/update-alternatives << 'UAWRAPPER_EOF'
#!/bin/bash
# update-alternatives wrapper for extension scriptlets
# Sets OPKG_OFFLINE_ROOT to manage alternatives within the extension sysroot

if [ -n "$AVOCADO_EXT_INSTALLROOT" ]; then
    case "$1" in
        --install|--remove|--config|--auto|--display|--list|--query|--set)
            # Debug: Show what we're doing
            echo "update-alternatives: OPKG_OFFLINE_ROOT=$AVOCADO_EXT_INSTALLROOT"
            echo "update-alternatives: executing: update-alternatives $*"

            # Set OPKG_OFFLINE_ROOT to the extension's installroot
            # This tells opkg-update-alternatives to operate within that root
            # Also ensure alternatives directory is created
            /usr/bin/mkdir -p "${AVOCADO_EXT_INSTALLROOT}/var/lib/opkg/alternatives" 2>/dev/null || true

            # Set clean PATH and call update-alternatives with OPKG_OFFLINE_ROOT
            export OPKG_OFFLINE_ROOT="$AVOCADO_EXT_INSTALLROOT"
            PATH="${AVOCADO_SDK_PREFIX}/usr/bin:/usr/bin:/bin" \
                exec ${AVOCADO_SDK_PREFIX}/usr/bin/update-alternatives "$@"
            ;;
    esac
fi

# If called without AVOCADO_EXT_INSTALLROOT, fail safely
exit 0
UAWRAPPER_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/update-alternatives

# Create opkg wrapper
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/opkg << 'OPKGWRAPPER_EOF'
#!/bin/bash
# opkg wrapper for extension scriptlets
exec ${AVOCADO_SDK_PREFIX}/usr/bin/opkg "$@"
OPKGWRAPPER_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/opkg

# Create generic noop wrapper for commands we don't want to execute
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/noop-command << 'NOOP_EOF'
#!/bin/bash
# Generic noop wrapper - always succeeds
exit 0
NOOP_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/noop-command

# Create a smart grep wrapper that pretends users/groups exist
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/grep << 'GREP_EOF'
#!/bin/bash
# Smart grep wrapper for scriptlet user/group validation
# When checking /etc/passwd or /etc/group, pretend the user/group exists
# For everything else, use the real grep

# Check if this looks like a user/group existence check
if [[ "$*" =~ /etc/passwd ]] || [[ "$*" =~ /etc/group ]]; then
    # Pretend we found a match - output a fake line and exit 0
    echo "placeholder:x:1000:1000::/:/bin/false"
    exit 0
fi

# For everything else, use real grep (find it in original PATH, not our wrapper dir)
# Remove our wrapper directory from PATH to find the real grep
ORIGINAL_PATH="${PATH#${AVOCADO_SDK_PREFIX}/ext-rpm-config-scripts/bin:}"
exec env PATH="$ORIGINAL_PATH" grep "$@"
GREP_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/grep

# Create symlinks for common scriptlet commands that should noop
# Allowlist approach: we create wrappers for what we DON'T want, not for what we DO want
for cmd in useradd groupadd usermod groupmod userdel groupdel chown chmod chgrp \
           flock systemd-tmpfiles udevadm \
           dbus-send killall service update-rc.d invoke-rc.d \
           gtk-update-icon-cache glib-compile-schemas update-desktop-database \
           fc-cache mkfontdir mkfontscale install-info update-mime-database \
           passwd chpasswd gpasswd newusers \
           systemd-sysusers systemd-hwdb kmod insmod modprobe \
           setcap getcap chcon restorecon selinuxenabled getenforce \
           rpm-helper gtk-query-immodules-3.0 \
           gdk-pixbuf-query-loaders gio-querymodules \
           dconf gsettings glib-compile-resources \
           bbnote bbfatal bbwarn bbdebug; do
    ln -sf noop-command $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/$cmd
done

# Create depmod wrapper that operates against the installroot sysroot
# Only runs when AVOCADO_SYSROOT_SCRIPTS=1 (set for rootfs/initramfs installs,
# NOT for extension installs where depmod is unnecessary)
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/depmod << 'DEPMOD_EOF'
#!/bin/bash
# depmod wrapper for sysroot installs
# Only active for rootfs/initramfs (AVOCADO_SYSROOT_SCRIPTS=1), noop for extensions
if [ "$AVOCADO_SYSROOT_SCRIPTS" = "1" ] && [ -n "$AVOCADO_EXT_INSTALLROOT" ]; then
    # Extract kernel version from arguments (last non-flag argument)
    KVER=""
    ARGS=()
    for arg in "$@"; do
        case "$arg" in
            -*) ARGS+=("$arg") ;;
            *)  KVER="$arg" ;;
        esac
    done
    if [ -z "$KVER" ]; then
        KVER=$(ls "$AVOCADO_EXT_INSTALLROOT/usr/lib/modules/" 2>/dev/null | head -n 1)
    fi
    if [ -n "$KVER" ] && [ -d "$AVOCADO_EXT_INSTALLROOT/usr/lib/modules/$KVER" ]; then
        exec /sbin/depmod "${ARGS[@]}" -b "$AVOCADO_EXT_INSTALLROOT" "$KVER"
    fi
fi
exit 0
DEPMOD_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/depmod

# ldconfig is noop'd during install — ldconfig -r requires chroot which doesn't
# work cross-arch. Instead, ldconfig is run during the rootfs/initramfs build step
# against the work copy so ld.so.cache is baked into the final image.
ln -sf noop-command $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/ldconfig

# Create systemctl wrapper that handles enable/disable/preset for sysroot installs.
# Only active for rootfs/initramfs (AVOCADO_SYSROOT_SCRIPTS=1), noop for extensions.
# This is a minimal offline implementation (like Yocto's systemd-systemctl-native)
# because the SDK's systemctl has hardcoded sysconfdir paths that break --root.
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/systemctl << 'SYSTEMCTL_EOF'
#!/bin/bash
# Minimal offline systemctl for sysroot enable/disable/preset operations.
# Parses [Install] sections and creates symlinks directly — no running systemd needed.

ROOT=""
ACTION=""
UNITS=()
PRESET_MODE=""

# Parse arguments
while [ $# -gt 0 ]; do
    case "$1" in
        --root=*) ROOT="${1#--root=}" ;;
        --root) shift; ROOT="$1" ;;
        --preset-mode=*) PRESET_MODE="${1#--preset-mode=}" ;;
        --no-block|--no-reload|--force|-f|--now) ;; # ignore
        enable|disable|preset|preset-all|mask|unmask|daemon-reload|restart|start|stop|reload|is-enabled)
            ACTION="$1" ;;
        -*) ;; # ignore unknown flags
        *) UNITS+=("$1") ;;
    esac
    shift
done

# For scriptlet calls, use AVOCADO_EXT_INSTALLROOT as root if --root not given
if [ "$AVOCADO_SYSROOT_SCRIPTS" = "1" ] && [ -z "$ROOT" ] && [ -n "$AVOCADO_EXT_INSTALLROOT" ]; then
    ROOT="$AVOCADO_EXT_INSTALLROOT"
fi

# Noop for actions we don't handle offline, or if no root is set
case "$ACTION" in
    enable|disable|preset|preset-all|mask|unmask) ;;
    *) exit 0 ;;
esac

[ -z "$ROOT" ] && exit 0

UNIT_DIR="$ROOT/usr/lib/systemd/system"
ETC_DIR="$ROOT/etc/systemd/system"

# Parse WantedBy/RequiredBy/Alias from a unit file's [Install] section
parse_install_section() {
    local unit_file="$1"
    local key="$2"
    [ -f "$unit_file" ] || return
    local in_install=false
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%%#*}"  # strip comments
        line="${line#"${line%%[![:space:]]*}"}"  # trim leading whitespace
        [ -z "$line" ] && continue
        case "$line" in
            \[Install\]*) in_install=true; continue ;;
            \[*) in_install=false; continue ;;
        esac
        if $in_install; then
            case "$line" in
                ${key}=*)
                    local val="${line#*=}"
                    val="${val#"${val%%[![:space:]]*}"}"
                    echo "$val"
                    ;;
            esac
        fi
    done < "$unit_file"
}

do_enable() {
    local unit="$1"
    local unit_file="$UNIT_DIR/$unit"
    [ -f "$unit_file" ] || return 0

    # Process WantedBy
    for target in $(parse_install_section "$unit_file" "WantedBy"); do
        local wants_dir="$ETC_DIR/${target}.wants"
        mkdir -p "$wants_dir"
        ln -sf "/usr/lib/systemd/system/$unit" "$wants_dir/$unit"
    done

    # Process RequiredBy
    for target in $(parse_install_section "$unit_file" "RequiredBy"); do
        local requires_dir="$ETC_DIR/${target}.requires"
        mkdir -p "$requires_dir"
        ln -sf "/usr/lib/systemd/system/$unit" "$requires_dir/$unit"
    done

    # Process Alias
    for alias in $(parse_install_section "$unit_file" "Alias"); do
        ln -sf "/usr/lib/systemd/system/$unit" "$ETC_DIR/$alias"
    done

    # Process Also (recursive enable)
    for also in $(parse_install_section "$unit_file" "Also"); do
        do_enable "$also"
    done
}

do_disable() {
    local unit="$1"
    local unit_file="$UNIT_DIR/$unit"
    [ -f "$unit_file" ] || return 0

    for target in $(parse_install_section "$unit_file" "WantedBy"); do
        rm -f "$ETC_DIR/${target}.wants/$unit"
    done
    for target in $(parse_install_section "$unit_file" "RequiredBy"); do
        rm -f "$ETC_DIR/${target}.requires/$unit"
    done
    for alias in $(parse_install_section "$unit_file" "Alias"); do
        rm -f "$ETC_DIR/$alias"
    done
}

do_mask() {
    local unit="$1"
    ln -sf /dev/null "$ETC_DIR/$unit"
}

do_unmask() {
    local unit="$1"
    local link="$ETC_DIR/$unit"
    [ -L "$link" ] && [ "$(readlink "$link")" = "/dev/null" ] && rm -f "$link"
}

# Load preset rules: returns lines like "enable <pattern>" or "disable <pattern>"
load_presets() {
    local preset_dirs=("$ROOT/etc/systemd/system-preset" "$ROOT/usr/lib/systemd/system-preset")
    # In initrd mode, also check initrd-preset
    if [ -f "$ROOT/etc/initrd-release" ]; then
        preset_dirs=("$ROOT/usr/lib/systemd/initrd-preset" "${preset_dirs[@]}")
    fi
    for dir in "${preset_dirs[@]}"; do
        [ -d "$dir" ] || continue
        for f in $(ls "$dir"/*.preset 2>/dev/null | sort); do
            while IFS= read -r line || [[ -n "$line" ]]; do
                line="${line%%#*}"
                line="${line#"${line%%[![:space:]]*}"}"
                [ -z "$line" ] && continue
                echo "$line"
            done < "$f"
        done
    done
}

check_preset() {
    local unit="$1"
    local rules
    rules=$(load_presets)
    while IFS= read -r rule; do
        local action pattern
        action="${rule%% *}"
        pattern="${rule#* }"
        pattern="${pattern#"${pattern%%[![:space:]]*}"}"
        [ -z "$pattern" ] && continue
        # fnmatch-style: check if unit matches pattern
        case "$unit" in
            $pattern) echo "$action"; return ;;
        esac
    done <<< "$rules"
    # Default: enable
    echo "enable"
}

case "$ACTION" in
    enable)
        for unit in "${UNITS[@]}"; do do_enable "$unit"; done ;;
    disable)
        for unit in "${UNITS[@]}"; do do_disable "$unit"; done ;;
    mask)
        for unit in "${UNITS[@]}"; do do_mask "$unit"; done ;;
    unmask)
        for unit in "${UNITS[@]}"; do do_unmask "$unit"; done ;;
    preset)
        for unit in "${UNITS[@]}"; do
            result=$(check_preset "$unit")
            if [ "$result" = "enable" ] && [ "$PRESET_MODE" != "disable-only" ]; then
                do_enable "$unit"
            elif [ "$result" = "disable" ] && [ "$PRESET_MODE" != "enable-only" ]; then
                do_disable "$unit"
            fi
        done ;;
    preset-all)
        if [ -d "$UNIT_DIR" ]; then
            for unit_file in "$UNIT_DIR"/*.service "$UNIT_DIR"/*.socket "$UNIT_DIR"/*.timer "$UNIT_DIR"/*.path "$UNIT_DIR"/*.target; do
                [ -f "$unit_file" ] || continue
                unit="$(basename "$unit_file")"
                result=$(check_preset "$unit")
                if [ "$result" = "enable" ] && [ "$PRESET_MODE" != "disable-only" ]; then
                    do_enable "$unit"
                elif [ "$result" = "disable" ] && [ "$PRESET_MODE" != "enable-only" ]; then
                    do_disable "$unit"
                fi
            done
        fi ;;
esac
exit 0
SYSTEMCTL_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/systemctl

# Create shell wrapper for scriptlet interpreter
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh << 'SHELL_EOF'
#!/bin/bash
# Shell wrapper for RPM scriptlets
# Set OPT=--opt to make Yocto scriptlets skip user/group management
# This is the proper way to tell Yocto scripts we're in a sysroot environment

# Set PATH to find our command wrappers first, but explicitly exclude the installroot
# Only include: wrapper bin, SDK utilities, and container system paths
export PATH="${AVOCADO_SDK_PREFIX}/ext-rpm-config-scripts/bin:${AVOCADO_SDK_PREFIX}/usr/bin:/usr/bin:/bin"

# Tell Yocto scriptlets we're in OPT mode (skip user/group creation)
export OPT="--opt"

exec ${AVOCADO_SDK_PREFIX}/usr/bin/bash "$@"
SHELL_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh

# Update macros for extension scriptlets
sed -i "s|^%_dbpath[[:space:]]*%{_var}/lib/rpm$|%_dbpath                %{_var}/lib/rpm|" $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/macros

# Add macro overrides for shell interpreter only
cat >> $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/macros << 'MACROS_EOF'

# Override shell interpreter for scriptlets to use our custom shell
%__bash                 $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh
%__sh                   $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh
MACROS_EOF
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: sdk_init_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };

        let init_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        if init_success {
            print_success("Initialized SDK environment.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to initialize SDK environment."));
        }

        // Install avocado-sdk-{target} with version from distro.version
        print_info(
            &format!("Installing SDK for target '{target}'."),
            OutputLevel::Normal,
        );

        // Build package name and spec with lock file support
        let sdk_target_pkg_name = format!("avocado-sdk-{target}");
        let sdk_target_config_version = "*";
        let sdk_target_pkg = build_package_spec_with_lock(
            &lock_file,
            target,
            &sdk_sysroot,
            &sdk_target_pkg_name,
            sdk_target_config_version,
        );

        let sdk_target_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_HOST_REPO_CONF \
    -y \
    install \
    {sdk_target_pkg}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: sdk_target_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };

        let sdk_target_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Track all SDK packages installed for lock file update at the end
        let mut all_sdk_package_names: Vec<String> = Vec::new();

        if sdk_target_success {
            print_success(
                &format!("Installed SDK for target '{target}'."),
                OutputLevel::Normal,
            );
            // Add to list for later query (after environment is fully set up)
            all_sdk_package_names.push(sdk_target_pkg_name);
        } else {
            return Err(anyhow::anyhow!(
                "Failed to install SDK for target '{target}'."
            ));
        }

        // Run check-update to refresh metadata using the combined repo config.
        // This uses arch-specific varsdir for correct architecture filtering,
        // with repos from both arch-specific SDK and target-repoconf.
        let check_update_command = r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    check-update || true
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_update_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };

        run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Install avocado-sdk-bootstrap — repo scoping via --releasever, lock file pins exact version
        print_info("Installing SDK bootstrap.", OutputLevel::Normal);

        let bootstrap_pkg_name = "avocado-sdk-bootstrap";
        let bootstrap_config_version = "*";
        let bootstrap_pkg = build_package_spec_with_lock(
            &lock_file,
            target,
            &sdk_sysroot,
            bootstrap_pkg_name,
            bootstrap_config_version,
        );

        // Use combined repo config for bootstrap installation.
        // The bootstrap package is a nativesdk package that needs both the base repos
        // (from arch-specific SDK) and target-specific repos (from target-repoconf).
        let bootstrap_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    -y \
    install \
    {bootstrap_pkg}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: bootstrap_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };

        let bootstrap_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        if bootstrap_success {
            print_success("Installed SDK bootstrap.", OutputLevel::Normal);
            // Add to list for later query (after environment is fully set up)
            all_sdk_package_names.push(bootstrap_pkg_name.to_string());
        } else {
            return Err(anyhow::anyhow!("Failed to install SDK bootstrap."));
        }

        // Fetch remote extensions now that SDK repos are available
        // Only fetch extensions that are referenced by active runtimes
        self.fetch_remote_extensions_in_sdk(
            target,
            merged_container_args,
            initial_active_extensions,
        )
        .await?;

        // Reload composed config to include extension configs
        let composed = Config::load_composed(&self.config_path, Some(target))
            .with_context(|| "Failed to reload composed config after fetching extensions")?;
        let config = &composed.config;

        // Re-compute active extensions after config reload (remote extension configs may be merged)
        let active_extensions = find_active_extensions(
            config,
            &composed.merged_value,
            target,
            &self.config_path,
            None,
        )?;

        // Re-compute extension SDK dependencies, filtered to only active extensions
        let config_content = serde_yaml::to_string(&composed.merged_value)
            .with_context(|| "Failed to serialize composed config")?;
        let all_extension_sdk_dependencies = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                &config_content,
                Some(&self.config_path),
                Some(target),
            )
            .with_context(|| "Failed to parse extension SDK dependencies")?;
        let extension_sdk_dependencies: HashMap<String, HashMap<String, serde_yaml::Value>> =
            all_extension_sdk_dependencies
                .into_iter()
                .filter(|(ext_name, _)| active_extensions.contains(ext_name))
                .collect();

        // After bootstrap, source environment-setup and configure SSL certs for subsequent commands
        if self.verbose {
            print_info(
                "Configuring SDK environment after bootstrap.",
                OutputLevel::Normal,
            );
        }

        let env_setup_command = r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
    echo "[INFO] Sourced SDK environment setup."
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"
    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
    echo "[INFO] SSL certificates configured."
fi
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: env_setup_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };

        run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Return data needed for the parallel phase
        Ok(BootstrapResult {
            composed,
            extension_sdk_dependencies,
            sdk_dependencies: sdk_dependencies.clone(),
            all_sdk_package_names,
            sdk_sysroot,
            lock_file,
            active_extensions,
            src_dir,
        })
    }

    /// Install SDK packages (dependencies from config + extension SDK deps).
    /// Runs as one of the parallel tasks after bootstrap completes.
    #[allow(clippy::too_many_arguments)]
    async fn install_sdk_packages(
        &self,
        config: &Config,
        sdk_dependencies: &Option<HashMap<String, serde_yaml::Value>>,
        extension_sdk_dependencies: &HashMap<String, HashMap<String, serde_yaml::Value>>,
        lock_file: &mut LockFile,
        bootstrap_package_names: &[String],
        sdk_sysroot: &SysrootType,
        container_image: &str,
        target: &str,
        repo_url: Option<&str>,
        repo_release: Option<&str>,
        container_helper: &SdkContainer,
        merged_container_args: Option<&Vec<String>>,
        runs_on_context: Option<&RunsOnContext>,
    ) -> Result<()> {
        let mut sdk_packages = Vec::new();
        let mut sdk_package_names = Vec::new();

        // Add regular SDK dependencies
        if let Some(ref dependencies) = sdk_dependencies {
            sdk_packages.extend(self.build_package_list_with_lock(
                dependencies,
                lock_file,
                target,
                sdk_sysroot,
            ));
            sdk_package_names.extend(self.extract_package_names(dependencies));
        }

        // Add extension SDK dependencies to the package list
        for (ext_name, ext_deps) in extension_sdk_dependencies {
            if self.verbose {
                print_info(
                    &format!("Adding SDK dependencies from extension '{ext_name}'"),
                    OutputLevel::Normal,
                );
            }
            let ext_packages =
                self.build_package_list_with_lock(ext_deps, lock_file, target, sdk_sysroot);
            sdk_packages.extend(ext_packages);
            sdk_package_names.extend(self.extract_package_names(ext_deps));
        }

        // Track all SDK package names for version query (bootstrap pkgs + new deps)
        let mut all_sdk_package_names: Vec<String> = bootstrap_package_names.to_vec();

        if !sdk_packages.is_empty() {
            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            // Use combined repo config for SDK dependencies.
            // SDK dependencies are nativesdk packages that need both the base repos
            // (from arch-specific SDK) and target-specific repos (from target-repoconf).
            // The combined config uses arch-specific varsdir for correct architecture
            // filtering, which is critical for --runs-on with cross-arch targets.
            let sdk_pkg_tui_ctx = self.tui_context.as_ref().map(|ctx| TuiContext {
                task_id: TaskId::SdkPackages,
                renderer: ctx.renderer.clone(),
            });
            let command = format!(
                r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    {} \
    install \
    {}
"#,
                dnf_args_str,
                yes,
                sdk_packages.join(" ")
            );

            // Use the container helper's run_in_container method
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command,
                verbose: self.verbose,
                source_environment: true,
                interactive: !self.force,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                // runs_on handled by shared context
                tui_context: sdk_pkg_tui_ctx,
                ..Default::default()
            };
            let install_success = run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if install_success {
                print_success("Installed SDK dependencies.", OutputLevel::Normal);
                // Add SDK dependency package names to the list
                all_sdk_package_names.extend(sdk_package_names);
            } else {
                return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
            }
        } else {
            print_success("No dependencies configured.", OutputLevel::Normal);
        }

        // Query all SDK packages at once (bootstrap + dependencies)
        // This is done after environment-setup is sourced for reliability
        if !all_sdk_package_names.is_empty() {
            let installed_versions = container_helper
                .query_installed_packages(
                    sdk_sysroot,
                    &all_sdk_package_names,
                    container_image,
                    target,
                    repo_url.map(|s| s.to_string()),
                    repo_release.map(|s| s.to_string()),
                    merged_container_args.cloned(),
                    runs_on_context,
                    self.sdk_arch.as_ref(),
                )
                .await?;

            if !installed_versions.is_empty() {
                lock_file.update_sysroot_versions(target, sdk_sysroot, installed_versions);
                if self.verbose {
                    print_info(
                        &format!(
                            "Updated lock file with {} SDK package versions.",
                            all_sdk_package_names.len()
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        Ok(())
    }

    /// Build a list of packages from dependencies HashMap, using lock file for pinned versions
    fn build_package_list_with_lock(
        &self,
        dependencies: &HashMap<String, serde_yaml::Value>,
        lock_file: &LockFile,
        target: &str,
        sysroot: &SysrootType,
    ) -> Vec<String> {
        let mut packages = Vec::new();

        for (package_name, version) in dependencies {
            let config_version = match version {
                serde_yaml::Value::String(v) => v.clone(),
                serde_yaml::Value::Mapping(_) => "*".to_string(),
                _ => "*".to_string(),
            };

            let package_spec = build_package_spec_with_lock(
                lock_file,
                target,
                sysroot,
                package_name,
                &config_version,
            );
            packages.push(package_spec);
        }

        packages
    }

    /// Extract just the package names from a dependencies HashMap
    fn extract_package_names(
        &self,
        dependencies: &HashMap<String, serde_yaml::Value>,
    ) -> Vec<String> {
        dependencies.keys().cloned().collect()
    }
}

/// Helper function to run a container command, using shared context if available
async fn run_container_command(
    container_helper: &SdkContainer,
    mut config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
    sdk_arch: Option<&String>,
) -> Result<bool> {
    // Inject sdk_arch if provided
    if let Some(arch) = sdk_arch {
        config.sdk_arch = Some(arch.clone());
    }

    if let Some(context) = runs_on_context {
        // Use the shared context - don't set runs_on in config as we're handling it
        container_helper
            .run_in_container_with_context(&config, context)
            .await
    } else {
        // No shared context - use regular execution (may create its own context if runs_on is set)
        container_helper.run_in_container(config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;
    use std::collections::HashMap;

    #[test]
    fn test_build_package_list_with_lock() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);
        let lock_file = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));
        deps.insert(
            "package3".to_string(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );

        let packages = cmd.build_package_list_with_lock(&deps, &lock_file, target, &sdk_x86);

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"package1".to_string()));
        assert!(packages.contains(&"package2-1.0.0".to_string()));
        assert!(packages.contains(&"package3".to_string()));
    }

    #[test]
    fn test_build_package_list_with_lock_uses_locked_version() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);
        let mut lock_file = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        // Add a locked version for package1
        lock_file.update_sysroot_versions(
            target,
            &sdk_x86,
            [("package1".to_string(), "2.0.0-r0.x86_64".to_string())]
                .into_iter()
                .collect(),
        );

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));

        let packages = cmd.build_package_list_with_lock(&deps, &lock_file, target, &sdk_x86);

        assert_eq!(packages.len(), 2);
        // package1 should use locked version instead of "*"
        assert!(packages.contains(&"package1-2.0.0-r0.x86_64".to_string()));
        // package2 has no lock entry, uses config version
        assert!(packages.contains(&"package2-1.0.0".to_string()));
    }

    #[test]
    fn test_new() {
        let cmd = SdkInstallCommand::new(
            "config.toml".to_string(),
            true,
            false,
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }
}
