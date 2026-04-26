//! Install command implementation that runs SDK, extension, and runtime installs.

use anyhow::{Context, Result};
use std::pin::Pin;
use std::sync::Arc;

use crate::commands::{
    ext::ExtInstallCommand, runtime::RuntimeInstallCommand, sdk::SdkInstallCommand,
};
use crate::utils::{
    config::{ComposedConfig, Config},
    container::TuiContext,
    output::{print_error, print_info, print_success, should_use_tui, OutputLevel},
    scheduler::{TaskGraph, TaskScheduler},
    target::validate_and_log_target,
    tui::{TaskId, TaskRenderer, TaskStatus},
};

/// Represents an extension dependency
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionDependency {
    /// Extension defined in the config (local or fetched remote)
    Local(String),
}

/// Implementation of the 'install' command that runs all install subcommands.
pub struct InstallCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Runtime name to install dependencies for (if not provided, installs for all runtimes)
    pub runtime: Option<String>,
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
}

impl InstallCommand {
    /// Create a new InstallCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        runtime: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            runtime,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
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
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Execute the install command
    pub async fn execute(&self) -> Result<()> {
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
        // parsed from initial load is not used after sdk install reloads config
        let _parsed = &composed.merged_value;
        let _target = validate_and_log_target(self.target.as_deref(), config)?;

        // Compute target runtimes early so we can show a useful start message.
        let initial_runtimes = self.find_target_relevant_runtimes(config, _parsed, &_target)?;
        if initial_runtimes.len() == 1 {
            print_info(
                &format!(
                    "Installing packages and dependencies for '{}' runtime",
                    initial_runtimes[0]
                ),
                OutputLevel::Normal,
            );
        } else if initial_runtimes.is_empty() {
            print_info("Installing SDK packages", OutputLevel::Normal);
        } else {
            let names: Vec<&str> = initial_runtimes.iter().map(|s| s.as_str()).collect();
            print_info(
                &format!(
                    "Installing packages and dependencies for runtimes [{}]",
                    names.join(", ")
                ),
                OutputLevel::Normal,
            );
        }

        // Create a single TUI renderer for the entire install flow.
        // Register SDK + sysroot tasks upfront (we know these from config).
        // Ext/runtime tasks are added after config reload.
        // Only use TUI when --force is set (dnf gets --assumeyes, so no
        // prompts).  Without --force, dnf may ask for confirmation and the
        // user needs to see and respond to the output directly.
        let renderer = if should_use_tui() && !self.verbose && self.force {
            let r = Arc::new(TaskRenderer::new(false));
            r.register_task(TaskId::SdkInstall, "sdk bootstrap".to_string());
            r.register_task(TaskId::SdkPackages, "sdk packages".to_string());
            r.register_task(TaskId::RootfsInstall, "rootfs install".to_string());
            r.register_task(TaskId::InitramfsInstall, "initramfs install".to_string());
            // target-dev install is registered dynamically by sdk/install.rs
            // after fetching extensions and discovering compile sections
            crate::utils::tui::set_active_renderer(&r);
            r.start();
            Some(r)
        } else {
            None
        };

        // 1. Install SDK dependencies
        if let Some(ref r) = renderer {
            r.set_status(&TaskId::SdkInstall, TaskStatus::Running);
        }
        print_info("Step 1/3: Installing SDK dependencies", OutputLevel::Normal);

        let sdk_tui_ctx = renderer.as_ref().map(|r| TuiContext {
            task_id: TaskId::SdkInstall,
            renderer: Arc::clone(r),
        });

        let mut sdk_install_cmd = SdkInstallCommand::new(
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_no_stamps(self.no_stamps)
        .with_runs_on(self.runs_on.clone(), self.nfs_port)
        .with_sdk_arch(self.sdk_arch.clone())
        .with_composed_config(Arc::clone(&composed));

        if let Some(ctx) = sdk_tui_ctx {
            sdk_install_cmd = sdk_install_cmd.with_tui_context(ctx);
        }

        let sdk_result = sdk_install_cmd.execute().await;

        if let Some(ref r) = renderer {
            if sdk_result.is_ok() {
                r.set_status(&TaskId::SdkInstall, TaskStatus::Success);
            } else {
                r.set_status(&TaskId::SdkInstall, TaskStatus::Failed);
                r.shutdown();
                if let Err(e) = sdk_result {
                    print_error(
                        &format!("{:#}", e.context("Failed to install SDK dependencies")),
                        OutputLevel::Normal,
                    );
                }
                std::process::exit(1);
            }
        }

        sdk_result.with_context(|| "Failed to install SDK dependencies")?;

        // Reload composed config after SDK install to pick up newly fetched remote extensions
        let composed = Arc::new(
            Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                || {
                    format!(
                        "Failed to reload composed config from {} after SDK install",
                        self.config_path
                    )
                },
            )?,
        );
        let config = &composed.config;
        let parsed = &composed.merged_value;

        // Determine which extensions and runtimes to install
        let extensions_to_install = self.find_required_extensions(&composed, &_target)?;
        let target_runtimes = self.find_target_relevant_runtimes(config, parsed, &_target)?;

        // Register ext/runtime tasks now that we know what was fetched.
        // (Sysroot tasks were already registered upfront.)
        if let Some(ref r) = renderer {
            for ext_dep in &extensions_to_install {
                let ExtensionDependency::Local(name) = ext_dep;
                r.register_task(
                    TaskId::ExtInstall(name.clone()),
                    format!("ext install {name}"),
                );
            }
            for rt in &target_runtimes {
                r.register_task(
                    TaskId::RuntimeInstall(rt.clone()),
                    format!("runtime install {rt}"),
                );
            }
        }

        // Determine parallelism
        let max_parallel: usize = std::env::var("AVOCADO_PARALLEL_TASKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| num_cpus::get().min(4));
        let max_parallel = if self.runs_on.is_some() {
            1
        } else {
            max_parallel
        };

        // Build DAG: ext installs have no inter-dependencies (parallel),
        // runtime installs depend on all ext installs completing first.
        if !extensions_to_install.is_empty() || !target_runtimes.is_empty() {
            let mut graph = TaskGraph::new();

            let ext_task_ids: Vec<TaskId> = extensions_to_install
                .iter()
                .map(|e| {
                    let ExtensionDependency::Local(name) = e;
                    TaskId::ExtInstall(name.clone())
                })
                .collect();

            for id in &ext_task_ids {
                graph.add_task(id.clone(), vec![]);
            }

            for rt in &target_runtimes {
                // Runtime installs depend on all ext installs
                graph.add_task(TaskId::RuntimeInstall(rt.clone()), ext_task_ids.clone());
            }

            let config_path = self.config_path.clone();
            let verbose = self.verbose;
            let force = self.force;
            let cli_target = self.target.clone();
            let container_args = self.container_args.clone();
            let dnf_args = self.dnf_args.clone();
            let no_stamps = self.no_stamps;
            let runs_on = self.runs_on.clone();
            let nfs_port = self.nfs_port;
            let sdk_arch = self.sdk_arch.clone();
            let composed2 = Arc::clone(&composed);
            let renderer2 = renderer.clone();
            // Phase 2d.3 dual-write: when the project has exactly one target-relevant
            // runtime, ext installs can mirror their lockfile state under
            // `runtimes.<r>.extensions.<ext>`. Multi-runtime projects skip the mirror
            // for now (per-extension membership lookup lands in a follow-up commit).
            let dual_write_runtime = if target_runtimes.len() == 1 {
                target_runtimes.first().cloned()
            } else {
                None
            };

            let sched_renderer = renderer
                .clone()
                .unwrap_or_else(|| Arc::new(TaskRenderer::new(true)));
            // Without TUI (no --force), run tasks sequentially so each
            // interactive prompt gets exclusive stdin access.
            let effective_parallel = if renderer.is_some() { max_parallel } else { 1 };
            let mut scheduler = TaskScheduler::new(graph, sched_renderer, effective_parallel);

            let sched_result = scheduler
                .run(move |task_id: TaskId| {
                    let config_path = config_path.clone();
                    let cli_target = cli_target.clone();
                    let container_args = container_args.clone();
                    let dnf_args = dnf_args.clone();
                    let runs_on = runs_on.clone();
                    let sdk_arch = sdk_arch.clone();
                    let composed = Arc::clone(&composed2);
                    let renderer = renderer2.clone();
                    let dual_write_runtime = dual_write_runtime.clone();

                    Box::pin(async move {
                        let tui_ctx = renderer.as_ref().map(|r| TuiContext {
                            task_id: task_id.clone(),
                            renderer: Arc::clone(r),
                        });

                        match task_id {
                            TaskId::ExtInstall(ref name) => {
                                let mut cmd = ExtInstallCommand::new(
                                    Some(name.clone()),
                                    config_path,
                                    verbose,
                                    force,
                                    cli_target,
                                    container_args,
                                    dnf_args,
                                )
                                .with_no_stamps(no_stamps)
                                .with_runs_on(runs_on, nfs_port)
                                .with_sdk_arch(sdk_arch)
                                .with_composed_config(composed)
                                .with_runtime(dual_write_runtime);
                                if let Some(ctx) = tui_ctx {
                                    cmd = cmd.with_tui_context(ctx);
                                }
                                cmd.execute().await
                            }
                            TaskId::RuntimeInstall(ref name) => {
                                let mut cmd = RuntimeInstallCommand::new(
                                    Some(name.clone()),
                                    config_path,
                                    verbose,
                                    force,
                                    cli_target,
                                    container_args,
                                    dnf_args,
                                )
                                .with_no_stamps(no_stamps)
                                .with_runs_on(runs_on, nfs_port)
                                .with_sdk_arch(sdk_arch)
                                .with_composed_config(composed);
                                if let Some(ctx) = tui_ctx {
                                    cmd = cmd.with_tui_context(ctx);
                                }
                                cmd.execute().await
                            }
                            _ => Ok(()),
                        }
                    })
                        as Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
                })
                .await;

            // Always shut down BEFORE propagating the error so the render
            // loop stops and can't corrupt the terminal.
            if let Some(ref r) = renderer {
                r.shutdown();
            }
            if let Err(e) = sched_result {
                print_error(&format!("{e:#}"), OutputLevel::Normal);
                std::process::exit(1);
            }
        } else {
            // No scheduler tasks — still need to shut down the renderer
            if let Some(ref r) = renderer {
                r.shutdown();
            }
        }

        print_success(
            "All components installed successfully!",
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Find all extensions required by the runtime/target, or all extensions if no runtime/target specified
    fn find_required_extensions(
        &self,
        composed: &ComposedConfig,
        target: &str,
    ) -> Result<Vec<ExtensionDependency>> {
        use std::collections::HashSet;

        let mut required_extensions = HashSet::new();

        let config = &composed.config;
        let parsed = &composed.merged_value;
        let config_path = &composed.config_path;

        // First, find which runtimes are relevant for this target
        let target_runtimes = self.find_target_relevant_runtimes(config, parsed, target)?;

        if target_runtimes.is_empty() {
            if self.verbose {
                print_info(
                    &format!("No runtimes found for target '{target}'. No extensions to install."),
                    OutputLevel::Normal,
                );
            }
            // No runtimes match this target - return empty list
            // Users can explicitly install extensions via `avocado ext install -e <name>`
        } else {
            // Only install extensions needed by the target-relevant runtimes
            if let Some(runtime_section) = parsed.get("runtimes").and_then(|r| r.as_mapping()) {
                for runtime_name in &target_runtimes {
                    if let Some(_runtime_config) = runtime_section.get(runtime_name) {
                        // Check both base dependencies and target-specific dependencies
                        let merged_runtime =
                            config.get_merged_runtime_config(runtime_name, target, config_path)?;
                        if let Some(merged_value) = merged_runtime {
                            // NEW FORMAT: Extensions are listed directly under runtimes.<name>.extensions
                            if let Some(extensions_list) =
                                merged_value.get("extensions").and_then(|e| e.as_sequence())
                            {
                                for ext_val in extensions_list {
                                    if let Some(ext_name) = ext_val.as_str() {
                                        required_extensions.insert(ExtensionDependency::Local(
                                            ext_name.to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut extensions: Vec<ExtensionDependency> = required_extensions.into_iter().collect();
        extensions.sort_by(|a, b| {
            let ExtensionDependency::Local(name_a) = a;
            let ExtensionDependency::Local(name_b) = b;
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Find runtimes that are relevant for the specified target
    fn find_target_relevant_runtimes(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        target: &str,
    ) -> Result<Vec<String>> {
        let mut relevant_runtimes = Vec::new();

        if let Some(runtime_section) = parsed.get("runtimes").and_then(|r| r.as_mapping()) {
            for runtime_name_val in runtime_section.keys() {
                if let Some(runtime_name) = runtime_name_val.as_str() {
                    // If a specific runtime is requested, only check that one
                    if let Some(ref requested_runtime) = self.runtime {
                        if runtime_name != requested_runtime {
                            continue;
                        }
                    }

                    // Check if this runtime is relevant for the target
                    let merged_runtime = config.get_merged_runtime_config(
                        runtime_name,
                        target,
                        &self.config_path,
                    )?;
                    if let Some(merged_value) = merged_runtime {
                        if let Some(runtime_target) =
                            merged_value.get("target").and_then(|t| t.as_str())
                        {
                            // Runtime has explicit target - only include if it matches
                            if runtime_target == target {
                                relevant_runtimes.push(runtime_name.to_string());
                            }
                        } else {
                            // Runtime has no target specified - include for all targets
                            relevant_runtimes.push(runtime_name.to_string());
                        }
                    } else {
                        // If there's no merged config, check the base runtime config
                        if let Some(runtime_config) = runtime_section.get(runtime_name_val) {
                            if let Some(runtime_target) =
                                runtime_config.get("target").and_then(|t| t.as_str())
                            {
                                // Runtime has explicit target - only include if it matches
                                if runtime_target == target {
                                    relevant_runtimes.push(runtime_name.to_string());
                                }
                            } else {
                                // Runtime has no target specified - include for all targets
                                relevant_runtimes.push(runtime_name.to_string());
                            }
                        }
                    }
                }
            }
        }

        Ok(relevant_runtimes)
    }
}

// ---------------------------------------------------------------------------
// Imperative add / remove commands
// ---------------------------------------------------------------------------

use crate::utils::config_edit::PackageScope;
use crate::utils::output::OutputLevel as OL;

/// `avocado install <packages> -e <ext>` -- add packages to config + install
pub struct PackageAddCommand {
    pub packages: Vec<String>,
    pub extension: Option<String>,
    pub runtime: Option<String>,
    #[allow(dead_code)] // validated in main.rs scope routing
    pub sdk: bool,
    pub config_path: String,
    pub verbose: bool,
    pub force: bool,
    pub no_save: bool,
    pub target: Option<String>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub no_stamps: bool,
    pub runs_on: Option<String>,
    pub nfs_port: Option<u16>,
    pub sdk_arch: Option<String>,
}

impl PackageAddCommand {
    pub async fn execute(&self) -> Result<()> {
        let scope = self.resolve_scope();
        let config_path = std::path::Path::new(&self.config_path);

        // 1. Write packages to avocado.yaml (unless --no-save)
        if !self.no_save {
            let added =
                crate::utils::config_edit::add_packages(config_path, &scope, &self.packages)?;
            if added.is_empty() {
                print_info(
                    &format!(
                        "All specified packages already present in {}.",
                        scope_label(&scope)
                    ),
                    OL::Normal,
                );
            } else {
                print_success(
                    &format!(
                        "Added {} package(s) to {}: {}",
                        added.len(),
                        scope_label(&scope),
                        added.join(", ")
                    ),
                    OL::Normal,
                );
            }
        }

        // 2. Run the scoped install to actually install the packages
        self.run_scoped_install(&scope).await
    }

    fn resolve_scope(&self) -> PackageScope {
        if let Some(ref ext) = self.extension {
            PackageScope::Extension(ext.clone())
        } else if let Some(ref rt) = self.runtime {
            PackageScope::Runtime(rt.clone())
        } else {
            PackageScope::Sdk
        }
    }

    async fn run_scoped_install(&self, scope: &PackageScope) -> Result<()> {
        match scope {
            PackageScope::Extension(name) => {
                let cmd = ExtInstallCommand::new(
                    Some(name.clone()),
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
            PackageScope::Runtime(name) => {
                let mut cmd = RuntimeInstallCommand::new(
                    Some(name.clone()),
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
            PackageScope::Sdk => {
                let mut cmd = SdkInstallCommand::new(
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
        }
    }
}

/// `avocado uninstall <packages> -e <ext>` -- remove from config + sync sysroot
pub struct PackageRemoveCommand {
    pub packages: Vec<String>,
    pub extension: Option<String>,
    pub runtime: Option<String>,
    #[allow(dead_code)] // validated in main.rs scope routing
    pub sdk: bool,
    pub config_path: String,
    pub verbose: bool,
    pub force: bool,
    pub target: Option<String>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub no_stamps: bool,
    pub runs_on: Option<String>,
    pub nfs_port: Option<u16>,
    pub sdk_arch: Option<String>,
}

impl PackageRemoveCommand {
    pub async fn execute(&self) -> Result<()> {
        let scope = self.resolve_scope();
        let config_path = std::path::Path::new(&self.config_path);

        // 1. Remove packages from avocado.yaml
        let removed =
            crate::utils::config_edit::remove_packages(config_path, &scope, &self.packages)?;

        if removed.is_empty() {
            print_error(
                &format!(
                    "None of the specified packages found in {}.",
                    scope_label(&scope)
                ),
                OL::Normal,
            );
            return Ok(());
        }

        print_success(
            &format!(
                "Removed {} package(s) from {}: {}",
                removed.len(),
                scope_label(&scope),
                removed.join(", ")
            ),
            OL::Normal,
        );

        // 2. Re-run install to sync the sysroot (the sync-aware install will detect
        //    the removals via lock file comparison and clean+reinstall automatically)
        print_info(
            &format!("Syncing {} sysroot...", scope_label(&scope)),
            OL::Normal,
        );

        self.run_scoped_install(&scope).await
    }

    fn resolve_scope(&self) -> PackageScope {
        if let Some(ref ext) = self.extension {
            PackageScope::Extension(ext.clone())
        } else if let Some(ref rt) = self.runtime {
            PackageScope::Runtime(rt.clone())
        } else {
            PackageScope::Sdk
        }
    }

    async fn run_scoped_install(&self, scope: &PackageScope) -> Result<()> {
        match scope {
            PackageScope::Extension(name) => {
                let cmd = ExtInstallCommand::new(
                    Some(name.clone()),
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
            PackageScope::Runtime(name) => {
                let mut cmd = RuntimeInstallCommand::new(
                    Some(name.clone()),
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
            PackageScope::Sdk => {
                let mut cmd = SdkInstallCommand::new(
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone());
                cmd.execute().await
            }
        }
    }
}

fn scope_label(scope: &PackageScope) -> String {
    match scope {
        PackageScope::Extension(name) => format!("extension '{name}'"),
        PackageScope::Runtime(name) => format!("runtime '{name}'"),
        PackageScope::Sdk => "SDK".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = InstallCommand::new(
            "avocado.yaml".to_string(),
            true,
            false,
            Some("my-runtime".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.runtime, Some("my-runtime".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = InstallCommand::new(
            "config.toml".to_string(),
            false,
            false,
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_new_with_runtime() {
        let cmd = InstallCommand::new(
            "avocado.yaml".to_string(),
            false,
            true,
            Some("test-runtime".to_string()),
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert!(cmd.force);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_extension_dependency_variants() {
        // Test that ExtensionDependency::Local can be created, compared, cloned, and hashed
        let local_a = ExtensionDependency::Local("test-ext".to_string());
        let local_b = ExtensionDependency::Local("other-ext".to_string());

        // Test equality
        assert_eq!(local_a, ExtensionDependency::Local("test-ext".to_string()));
        assert_ne!(local_a, local_b);

        // Test that they can be cloned and hashed (for HashSet usage)
        let mut set = std::collections::HashSet::new();
        set.insert(local_a.clone());
        set.insert(local_b.clone());
        assert_eq!(set.len(), 2);

        // Inserting a duplicate should not increase the set size
        set.insert(local_a.clone());
        assert_eq!(set.len(), 2);
    }
}
