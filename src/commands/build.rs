//! Build command implementation that runs SDK compile, extension build, and runtime build.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use crate::commands::{
    ext::{ExtBuildCommand, ExtImageCommand},
    runtime::RuntimeBuildCommand,
};
use crate::utils::{
    config::{ComposedConfig, Config, ExtensionSource},
    container::{SdkContainer, TuiContext},
    output::{print_error, print_info, print_success, should_use_tui, OutputLevel},
    scheduler::{TaskGraph, TaskScheduler},
    tui::{TaskId, TaskRenderer, TaskStatus},
};

/// Represents an extension dependency that can be either local or remote
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionDependency {
    /// Extension defined in the main config file
    Local(String),
    /// Remote extension with source field (repo, git, or path)
    Remote {
        name: String,
        source: ExtensionSource,
    },
}

/// Implementation of the 'build' command that runs all build subcommands.
pub struct BuildCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Runtime name to build (if not provided, builds all runtimes)
    pub runtime: Option<String>,
    /// Extension name to build (if not provided, builds all required extensions)
    pub extension: Option<String>,
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

impl BuildCommand {
    /// Create a new BuildCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        runtime: Option<String>,
        extension: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            runtime,
            extension,
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

    /// Execute the build command
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
        let parsed = &composed.merged_value;
        let target = crate::utils::target::validate_and_log_target(self.target.as_deref(), config)?;

        // If a specific extension is requested, build only that extension
        if let Some(ref ext_name) = self.extension {
            return self
                .build_single_extension(&composed, ext_name, &target)
                .await;
        }

        // If a specific runtime is requested, build only that runtime and its dependencies
        if let Some(ref runtime_name) = self.runtime {
            return self
                .build_single_runtime(&composed, runtime_name, &target)
                .await;
        }

        // Determine which runtimes to build based on target
        let runtimes_to_build = self.get_runtimes_to_build(config, parsed, &target)?;

        if runtimes_to_build.is_empty() {
            print_info("No runtimes found to build.", OutputLevel::Normal);
            return Ok(());
        }

        if runtimes_to_build.len() == 1 {
            print_info(
                &format!("Building '{}' runtime", runtimes_to_build[0]),
                OutputLevel::Normal,
            );
        } else {
            let names: Vec<&str> = runtimes_to_build.iter().map(|s| s.as_str()).collect();
            print_info(
                &format!("Building runtimes [{}]", names.join(", ")),
                OutputLevel::Normal,
            );
        }
        let required_extensions =
            self.find_required_extensions(config, parsed, &runtimes_to_build, &target)?;

        // Pre-flight: verify dependencies are installed before drawing the
        // task list.  This gives the user a clear "run avocado install" message
        // instead of failing mid-build inside a TUI.
        if !self.no_stamps {
            use crate::utils::stamps::{
                generate_batch_read_stamps_script, resolve_required_stamps, validate_stamps_batch,
                StampCommand, StampComponent,
            };

            let container_image = config.get_sdk_image().ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'")
            })?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, config)?.verbose(false);

            // Check that SDK install stamp exists (required for all builds).
            // Use Runtime/Install which just requires sdk_install().
            let required =
                resolve_required_stamps(StampCommand::Install, StampComponent::Runtime, None, &[]);

            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = crate::utils::container::RunConfig {
                container_image: container_image.clone(),
                target: target.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: config.get_sdk_repo_url(),
                repo_release: config.get_sdk_repo_release(),
                container_args: self.container_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                runs_on: self.runs_on.clone(),
                nfs_port: self.nfs_port,
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;
            let validation =
                validate_stamps_batch(&required, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error =
                    validation.into_error_with_runs_on("Cannot build", self.runs_on.as_deref());
                // Print the error with fix commands (includes "avocado install")
                error.print_and_exit();
            }
        }

        // Set up TUI renderer if applicable
        let renderer = if should_use_tui() && !self.verbose {
            let r = Arc::new(TaskRenderer::new(false));
            // Register tasks in execution order: all builds, then all images,
            // then all runtimes — so the checklist matches the dependency flow.
            for ext_dep in &required_extensions {
                let name = match ext_dep {
                    ExtensionDependency::Local(n) => n.clone(),
                    ExtensionDependency::Remote { name, .. } => name.clone(),
                };
                r.register_task(TaskId::ExtBuild(name.clone()), format!("ext build {name}"));
            }
            for ext_dep in &required_extensions {
                let name = match ext_dep {
                    ExtensionDependency::Local(n) => n.clone(),
                    ExtensionDependency::Remote { name, .. } => name.clone(),
                };
                r.register_task(TaskId::ExtImage(name.clone()), format!("ext image {name}"));
            }
            for rt in &runtimes_to_build {
                r.register_task(
                    TaskId::RuntimeBuild(rt.clone()),
                    format!("runtime build {rt}"),
                );
            }
            crate::utils::tui::set_active_renderer(&r);
            r.start();
            Some(r)
        } else {
            None
        };

        // Determine parallelism
        let max_parallel: usize = std::env::var("AVOCADO_PARALLEL_TASKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| num_cpus::get().min(4));

        // --runs-on forces sequential (remote execution)
        let max_parallel = if self.runs_on.is_some() {
            1
        } else {
            max_parallel
        };

        // Phase 1: Build extension builds + images in parallel via the scheduler.
        // ExtBuild(name) → no deps, ExtImage(name) → ExtBuild(name)
        if !required_extensions.is_empty() {
            let mut graph = TaskGraph::new();
            for ext_dep in &required_extensions {
                let name = match ext_dep {
                    ExtensionDependency::Local(n) => n.clone(),
                    ExtensionDependency::Remote { name, .. } => name.clone(),
                };
                graph.add_task(TaskId::ExtBuild(name.clone()), vec![]);
                graph.add_task(
                    TaskId::ExtImage(name.clone()),
                    vec![TaskId::ExtBuild(name.clone())],
                );
            }

            let config_path = self.config_path.clone();
            let verbose = self.verbose;
            let cli_target = self.target.clone();
            let container_args = self.container_args.clone();
            let dnf_args = self.dnf_args.clone();
            let no_stamps = self.no_stamps;
            let runs_on = self.runs_on.clone();
            let nfs_port = self.nfs_port;
            let sdk_arch = self.sdk_arch.clone();
            let composed2 = Arc::clone(&composed);
            let renderer2 = renderer.clone();
            // When the project resolves to a single runtime, propagate it so
            // ExtBuild/ExtImage land in the runtime-scoped sysroot tree and
            // gate on membership/precondition checks. Multi-runtime builds
            // run in legacy (per-target) mode for now — the scheduler graph
            // is keyed by extension name, not (runtime, extension).
            let dual_write_runtime = if runtimes_to_build.len() == 1 {
                runtimes_to_build.first().cloned()
            } else {
                None
            };

            let sched_renderer = renderer
                .clone()
                .unwrap_or_else(|| Arc::new(TaskRenderer::new(true)));
            let mut scheduler = TaskScheduler::new(graph, sched_renderer, max_parallel);

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
                            TaskId::ExtBuild(ref name) => {
                                let mut cmd = ExtBuildCommand::new(
                                    name.clone(),
                                    config_path,
                                    verbose,
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
                            TaskId::ExtImage(ref name) => {
                                let mut cmd = ExtImageCommand::new(
                                    name.clone(),
                                    config_path,
                                    verbose,
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
            if let Err(e) = sched_result {
                if let Some(ref r) = renderer {
                    r.shutdown();
                }
                print_error(&format!("{e:#}"), OutputLevel::Normal);
                std::process::exit(1);
            }
        }

        // Phase 2: Build runtimes sequentially (RuntimeBuildCommand contains
        // non-Send types like TufSigner, so it cannot be spawned on tokio).
        let mut rt_error: Option<anyhow::Error> = None;
        for runtime_name in &runtimes_to_build {
            let rt_task_id = TaskId::RuntimeBuild(runtime_name.clone());
            if let Some(ref r) = renderer {
                r.set_status(&rt_task_id, TaskStatus::Running);
            }

            let tui_ctx = renderer.as_ref().map(|r| TuiContext {
                task_id: rt_task_id.clone(),
                renderer: Arc::clone(r),
            });

            let mut cmd = RuntimeBuildCommand::new(
                runtime_name.clone(),
                self.config_path.clone(),
                self.verbose,
                self.target.clone(),
                self.container_args.clone(),
                self.dnf_args.clone(),
            )
            .with_no_stamps(self.no_stamps)
            .with_runs_on(self.runs_on.clone(), self.nfs_port)
            .with_sdk_arch(self.sdk_arch.clone())
            .with_composed_config(Arc::clone(&composed));

            if let Some(ctx) = tui_ctx {
                cmd = cmd.with_tui_context(ctx);
            }

            let result = cmd.execute().await;

            if let Some(ref r) = renderer {
                if result.is_ok() {
                    r.set_status(&rt_task_id, TaskStatus::Success);
                } else {
                    r.set_status(&rt_task_id, TaskStatus::Failed);
                }
            }

            if let Err(e) = result {
                rt_error = Some(e.context(format!("Failed to build runtime '{runtime_name}'")));
                break;
            }
        }

        // Always shut down the TUI renderer before returning
        if let Some(ref r) = renderer {
            r.shutdown();
        }

        if let Some(e) = rt_error {
            print_error(&format!("{e:#}"), OutputLevel::Normal);
            std::process::exit(1);
        }

        print_success("All components built successfully!", OutputLevel::Normal);
        Ok(())
    }

    /// Determine which runtimes to build based on the --runtime parameter and target
    fn get_runtimes_to_build(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        target: &str,
    ) -> Result<Vec<String>> {
        let runtime_section = parsed
            .get("runtimes")
            .and_then(|r| r.as_mapping())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        let mut target_runtimes = Vec::new();

        for runtime_name_val in runtime_section.keys() {
            if let Some(runtime_name) = runtime_name_val.as_str() {
                // If a specific runtime is requested, only check that one
                if let Some(ref requested_runtime) = self.runtime {
                    if runtime_name != requested_runtime {
                        continue;
                    }
                }

                // Check if this runtime is relevant for the target
                let merged_runtime =
                    config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
                if let Some(merged_value) = merged_runtime {
                    if let Some(runtime_target) =
                        merged_value.get("target").and_then(|t| t.as_str())
                    {
                        // Runtime has explicit target - only include if it matches
                        if runtime_target == target {
                            target_runtimes.push(runtime_name.to_string());
                        }
                    } else {
                        // Runtime has no target specified - include for all targets
                        target_runtimes.push(runtime_name.to_string());
                    }
                } else {
                    // If there's no merged config, check the base runtime config
                    if let Some(runtime_config) = runtime_section.get(runtime_name_val) {
                        if let Some(runtime_target) =
                            runtime_config.get("target").and_then(|t| t.as_str())
                        {
                            // Runtime has explicit target - only include if it matches
                            if runtime_target == target {
                                target_runtimes.push(runtime_name.to_string());
                            }
                        } else {
                            // Runtime has no target specified - include for all targets
                            target_runtimes.push(runtime_name.to_string());
                        }
                    }
                }
            }
        }

        // If a specific runtime was requested but doesn't match the target, return an error
        if let Some(ref requested_runtime) = self.runtime {
            if target_runtimes.is_empty() {
                return Err(anyhow::anyhow!(
                    "Runtime '{requested_runtime}' is not configured for target '{target}'"
                ));
            }
        }

        Ok(target_runtimes)
    }

    /// Find all extensions required by the specified runtimes and target
    fn find_required_extensions(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        runtimes: &[String],
        target: &str,
    ) -> Result<Vec<ExtensionDependency>> {
        use crate::utils::interpolation::interpolate_name;

        let mut required_extensions = HashSet::new();
        let _visited = HashSet::<String>::new(); // For cycle detection

        // If no runtimes are found for this target, don't build any extensions
        if runtimes.is_empty() {
            return Ok(vec![]);
        }

        // Build a map of interpolated ext names to their source config
        // This is needed because ext section keys may contain templates like {{ avocado.target }}
        let mut ext_sources: std::collections::HashMap<String, Option<ExtensionSource>> =
            std::collections::HashMap::new();
        if let Some(ext_section) = parsed.get("extensions").and_then(|e| e.as_mapping()) {
            for (ext_key, ext_config) in ext_section {
                if let Some(raw_name) = ext_key.as_str() {
                    // Interpolate the extension name with the target
                    let interpolated_name = interpolate_name(raw_name, target);
                    // Use parse_extension_source which properly deserializes the source field
                    let source = Config::parse_extension_source(&interpolated_name, ext_config)
                        .ok()
                        .flatten();
                    ext_sources.insert(interpolated_name, source);
                }
            }
        }

        for runtime_name in runtimes {
            // Get merged runtime config for this target
            let merged_runtime =
                config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
            if let Some(merged_value) = merged_runtime {
                // Read extensions from the new `extensions` array format
                if let Some(extensions) =
                    merged_value.get("extensions").and_then(|e| e.as_sequence())
                {
                    for ext in extensions {
                        if let Some(ext_name) = ext.as_str() {
                            // Check if this extension has a source: field (remote extension)
                            if let Some(Some(source)) = ext_sources.get(ext_name) {
                                // Remote extension with source field
                                required_extensions.insert(ExtensionDependency::Remote {
                                    name: ext_name.to_string(),
                                    source: source.clone(),
                                });
                            } else {
                                // Local extension (defined in ext section without source, or not in ext section)
                                required_extensions
                                    .insert(ExtensionDependency::Local(ext_name.to_string()));
                            }
                        }
                    }
                }
            }
        }

        let mut extensions: Vec<ExtensionDependency> = required_extensions.into_iter().collect();
        extensions.sort_by(|a, b| {
            let name_a = match a {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }
    /// Build a single extension without building runtimes
    async fn build_single_extension(
        &self,
        composed: &Arc<ComposedConfig>,
        extension_name: &str,
        target: &str,
    ) -> Result<()> {
        let config = &composed.config;
        let parsed = &composed.merged_value;

        print_info(
            &format!("Building single extension '{extension_name}' for target '{target}'"),
            OutputLevel::Normal,
        );

        // Check if this is a local extension or needs to be found in external configs
        let ext_config = parsed
            .get("extensions")
            .and_then(|ext| ext.get(extension_name));

        let extension_dep = if ext_config.is_some() {
            // Local extension
            ExtensionDependency::Local(extension_name.to_string())
        } else {
            // Try to find in external extensions - search all external dependencies
            self.find_external_extension(config, parsed, extension_name, target)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Extension '{extension_name}' not found in local extensions or external dependencies"
                    )
                })?
        };

        // Step 1: Build the extension (skip SDK installation for single extension builds)
        print_info(
            &format!("Step 1/2: Building extension '{extension_name}'"),
            OutputLevel::Normal,
        );

        match &extension_dep {
            ExtensionDependency::Local(ext_name) => {
                let ext_build_cmd = ExtBuildCommand::new(
                    ext_name.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone())
                .with_composed_config(Arc::clone(composed));
                ext_build_cmd
                    .execute()
                    .await
                    .with_context(|| format!("Failed to build extension '{ext_name}'"))?;
            }
            ExtensionDependency::Remote { name, source: _ } => {
                let ext_build_cmd = ExtBuildCommand::new(
                    name.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone())
                .with_composed_config(Arc::clone(composed));
                ext_build_cmd
                    .execute()
                    .await
                    .with_context(|| format!("Failed to build remote extension '{name}'"))?;
            }
        }

        // Step 2: Create extension image
        print_info(
            &format!("Step 2/2: Creating image for extension '{extension_name}'"),
            OutputLevel::Normal,
        );

        match &extension_dep {
            ExtensionDependency::Local(ext_name) => {
                let ext_image_cmd = ExtImageCommand::new(
                    ext_name.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone())
                .with_composed_config(Arc::clone(composed));
                ext_image_cmd.execute().await.with_context(|| {
                    format!("Failed to create image for extension '{ext_name}'")
                })?;
            }
            ExtensionDependency::Remote { name, source: _ } => {
                let ext_image_cmd = ExtImageCommand::new(
                    name.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port)
                .with_sdk_arch(self.sdk_arch.clone())
                .with_composed_config(Arc::clone(composed));
                ext_image_cmd.execute().await.with_context(|| {
                    format!("Failed to create image for remote extension '{name}'")
                })?;
            }
        }

        print_success(
            &format!("Successfully built extension '{extension_name}'!"),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Build a single runtime and its required extensions
    async fn build_single_runtime(
        &self,
        composed: &Arc<ComposedConfig>,
        runtime_name: &str,
        target: &str,
    ) -> Result<()> {
        let config = &composed.config;
        let parsed = &composed.merged_value;

        print_info(
            &format!("Building single runtime '{runtime_name}' for target '{target}'"),
            OutputLevel::Normal,
        );

        // Verify the runtime exists and is configured for this target
        let runtime_section = parsed
            .get("runtimes")
            .and_then(|r| r.as_mapping())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        if !runtime_section.contains_key(runtime_name) {
            return Err(anyhow::anyhow!(
                "Runtime '{runtime_name}' not found in configuration"
            ));
        }

        // Check if this runtime is configured for the target
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
        let runtime_config = if let Some(merged_value) = merged_runtime {
            merged_value
        } else {
            return Err(anyhow::anyhow!(
                "Runtime '{runtime_name}' has no configuration for target '{target}'"
            ));
        };

        // Check target compatibility
        if let Some(runtime_target) = runtime_config.get("target").and_then(|t| t.as_str()) {
            if runtime_target != target {
                return Err(anyhow::anyhow!(
                    "Runtime '{runtime_name}' is configured for target '{runtime_target}', not '{target}'"
                ));
            }
        }

        // Step 1: Find extensions required by this specific runtime
        print_info(
            "Step 1/4: Analyzing runtime dependencies",
            OutputLevel::Normal,
        );
        let required_extensions =
            self.find_extensions_for_runtime(config, parsed, &runtime_config, target)?;

        // Note: SDK compile sections are now compiled on-demand when extensions are built
        // This prevents duplicate compilation when sdk.compile sections are also extension dependencies

        // Step 2: Build required extensions
        if !required_extensions.is_empty() {
            print_info(
                &format!(
                    "Step 2/3: Building {} required extensions",
                    required_extensions.len()
                ),
                OutputLevel::Normal,
            );

            for extension_dep in &required_extensions {
                match extension_dep {
                    ExtensionDependency::Local(extension_name) => {
                        if self.verbose {
                            print_info(
                                &format!("Building local extension '{extension_name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        let ext_build_cmd = ExtBuildCommand::new(
                            extension_name.clone(),
                            self.config_path.clone(),
                            self.verbose,
                            self.target.clone(),
                            self.container_args.clone(),
                            self.dnf_args.clone(),
                        )
                        .with_no_stamps(self.no_stamps)
                        .with_runs_on(self.runs_on.clone(), self.nfs_port)
                        .with_sdk_arch(self.sdk_arch.clone())
                        .with_composed_config(Arc::clone(composed));
                        ext_build_cmd.execute().await.with_context(|| {
                            format!("Failed to build extension '{extension_name}'")
                        })?;

                        // Create extension image
                        let ext_image_cmd = ExtImageCommand::new(
                            extension_name.clone(),
                            self.config_path.clone(),
                            self.verbose,
                            self.target.clone(),
                            self.container_args.clone(),
                            self.dnf_args.clone(),
                        )
                        .with_no_stamps(self.no_stamps)
                        .with_runs_on(self.runs_on.clone(), self.nfs_port)
                        .with_sdk_arch(self.sdk_arch.clone())
                        .with_composed_config(Arc::clone(composed));
                        ext_image_cmd.execute().await.with_context(|| {
                            format!("Failed to create image for extension '{extension_name}'")
                        })?;
                    }
                    ExtensionDependency::Remote { name, source: _ } => {
                        if self.verbose {
                            print_info(
                                &format!("Building remote extension '{name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Build remote extension
                        let ext_build_cmd = ExtBuildCommand::new(
                            name.clone(),
                            self.config_path.clone(),
                            self.verbose,
                            self.target.clone(),
                            self.container_args.clone(),
                            self.dnf_args.clone(),
                        )
                        .with_no_stamps(self.no_stamps)
                        .with_runs_on(self.runs_on.clone(), self.nfs_port)
                        .with_sdk_arch(self.sdk_arch.clone())
                        .with_composed_config(Arc::clone(composed));
                        ext_build_cmd.execute().await.with_context(|| {
                            format!("Failed to build remote extension '{name}'")
                        })?;

                        // Create extension image
                        let ext_image_cmd = ExtImageCommand::new(
                            name.clone(),
                            self.config_path.clone(),
                            self.verbose,
                            self.target.clone(),
                            self.container_args.clone(),
                            self.dnf_args.clone(),
                        )
                        .with_no_stamps(self.no_stamps)
                        .with_runs_on(self.runs_on.clone(), self.nfs_port)
                        .with_sdk_arch(self.sdk_arch.clone())
                        .with_composed_config(Arc::clone(composed));
                        ext_image_cmd.execute().await.with_context(|| {
                            format!("Failed to create image for remote extension '{name}'")
                        })?;
                    }
                }
            }
        } else {
            print_info("Step 2/3: No extensions required", OutputLevel::Normal);
        }

        // Step 3: Build the runtime
        print_info(
            &format!("Step 3/3: Building runtime '{runtime_name}'"),
            OutputLevel::Normal,
        );

        let mut runtime_build_cmd = RuntimeBuildCommand::new(
            runtime_name.to_string(),
            self.config_path.clone(),
            self.verbose,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_no_stamps(self.no_stamps)
        .with_runs_on(self.runs_on.clone(), self.nfs_port)
        .with_sdk_arch(self.sdk_arch.clone())
        .with_composed_config(Arc::clone(composed));
        runtime_build_cmd
            .execute()
            .await
            .with_context(|| format!("Failed to build runtime '{runtime_name}'"))?;

        print_success(
            &format!("Successfully built runtime '{runtime_name}'!"),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Find extensions required by a specific runtime
    fn find_extensions_for_runtime(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        runtime_config: &serde_yaml::Value,
        target: &str,
    ) -> Result<Vec<ExtensionDependency>> {
        use crate::utils::interpolation::interpolate_name;

        let mut required_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Build a map of interpolated ext names to their source config
        // This is needed because ext section keys may contain templates like {{ avocado.target }}
        let mut ext_sources: std::collections::HashMap<String, Option<ExtensionSource>> =
            std::collections::HashMap::new();
        if let Some(ext_section) = parsed.get("extensions").and_then(|e| e.as_mapping()) {
            for (ext_key, ext_config) in ext_section {
                if let Some(raw_name) = ext_key.as_str() {
                    // Interpolate the extension name with the target
                    let interpolated_name = interpolate_name(raw_name, target);
                    // Use parse_extension_source which properly deserializes the source field
                    let source = Config::parse_extension_source(&interpolated_name, ext_config)
                        .ok()
                        .flatten();
                    ext_sources.insert(interpolated_name, source);
                }
            }
        }

        // Check extensions from the new `extensions` array format
        if let Some(extensions) = runtime_config
            .get("extensions")
            .and_then(|e| e.as_sequence())
        {
            for ext in extensions {
                if let Some(ext_name) = ext.as_str() {
                    // Check if this extension has a source: field (remote extension)
                    if let Some(Some(source)) = ext_sources.get(ext_name) {
                        // Remote extension with source field
                        required_extensions.insert(ExtensionDependency::Remote {
                            name: ext_name.to_string(),
                            source: source.clone(),
                        });
                    } else {
                        // Local extension (defined in ext section without source, or not in ext section)
                        required_extensions
                            .insert(ExtensionDependency::Local(ext_name.to_string()));

                        // Also check local extension dependencies
                        self.find_local_extension_dependencies(
                            config,
                            parsed,
                            ext_name,
                            &mut required_extensions,
                            &mut visited,
                        )?;
                    }
                }
            }
        }

        // Check runtime dependencies for extensions (old packages format for backwards compatibility)
        if let Some(dependencies) = runtime_config.get("packages").and_then(|d| d.as_mapping()) {
            for (_dep_name, dep_spec) in dependencies {
                // Check for extension dependency
                if let Some(ext_name) = dep_spec.get("extensions").and_then(|v| v.as_str()) {
                    // Local extension
                    required_extensions.insert(ExtensionDependency::Local(ext_name.to_string()));

                    // Also check local extension dependencies
                    self.find_local_extension_dependencies(
                        config,
                        &serde_yaml::from_str(&std::fs::read_to_string(&self.config_path)?)?,
                        ext_name,
                        &mut required_extensions,
                        &mut visited,
                    )?;
                }
            }
        }

        let mut extensions: Vec<ExtensionDependency> = required_extensions.into_iter().collect();
        extensions.sort_by(|a, b| {
            let name_a = match a {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Find an extension by searching through the full dependency tree
    fn find_external_extension(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        extension_name: &str,
        target: &str,
    ) -> Result<Option<ExtensionDependency>> {
        // First, collect all extensions in the dependency tree
        let mut all_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Get all extensions from runtime dependencies (this will recursively traverse)
        let runtime_section = parsed
            .get("runtimes")
            .and_then(|r| r.as_mapping())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        for runtime_name_val in runtime_section.keys() {
            // Convert runtime name from Value to String
            let runtime_name = match runtime_name_val.as_str() {
                Some(name) => name,
                None => continue, // Skip if runtime name is not a string
            };

            // Get merged runtime config for this target
            let merged_runtime =
                config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
            if let Some(merged_value) = merged_runtime {
                if let Some(dependencies) =
                    merged_value.get("packages").and_then(|d| d.as_mapping())
                {
                    for (_dep_name, dep_spec) in dependencies {
                        // Check for extension dependency
                        if let Some(ext_name) = dep_spec.get("extensions").and_then(|v| v.as_str())
                        {
                            // Local extension
                            all_extensions.insert(ExtensionDependency::Local(ext_name.to_string()));

                            // Also check local extension dependencies
                            self.find_local_extension_dependencies(
                                config,
                                parsed,
                                ext_name,
                                &mut all_extensions,
                                &mut visited,
                            )?;
                        }
                    }
                }
            }
        }

        // Now search for the target extension in all collected extensions
        for ext_dep in all_extensions {
            let found_name = match &ext_dep {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::Remote { name, .. } => name,
            };

            if found_name == extension_name {
                return Ok(Some(ext_dep));
            }
        }

        Ok(None)
    }

    /// Find dependencies of local extensions
    fn find_local_extension_dependencies(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        ext_name: &str,
        all_extensions: &mut HashSet<ExtensionDependency>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        self.find_local_extension_dependencies_in_config(
            config,
            parsed,
            ext_name,
            &std::path::PathBuf::from(&self.config_path),
            all_extensions,
            visited,
        )
    }

    /// Find dependencies of local extensions in a specific config
    #[allow(clippy::only_used_in_recursion)]
    fn find_local_extension_dependencies_in_config(
        &self,
        config: &Config,
        parsed_config: &serde_yaml::Value,
        ext_name: &str,
        config_path: &std::path::Path,
        all_extensions: &mut HashSet<ExtensionDependency>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        // Cycle detection for local extensions
        let ext_key = format!("local:{ext_name}:{}", config_path.display());
        if visited.contains(&ext_key) {
            return Ok(());
        }
        visited.insert(ext_key);

        // Get the local extension configuration
        if let Some(ext_config) = parsed_config
            .get("extensions")
            .and_then(|ext| ext.get(ext_name))
        {
            // Check if this local extension has dependencies
            let packages_value = ext_config.get("packages");
            if let Some(dependencies) = packages_value.and_then(|d| d.as_mapping()) {
                for (_dep_name, dep_spec) in dependencies {
                    // Check for extension dependency
                    if let Some(nested_ext_name) =
                        dep_spec.get("extensions").and_then(|v| v.as_str())
                    {
                        // Local extension dependency
                        all_extensions
                            .insert(ExtensionDependency::Local(nested_ext_name.to_string()));

                        // Recursively check this local extension's dependencies
                        self.find_local_extension_dependencies_in_config(
                            config,
                            parsed_config,
                            nested_ext_name,
                            config_path,
                            all_extensions,
                            visited,
                        )?;
                    }
                }
            } else if let Some(deps_value) = packages_value {
                // packages field exists but is not a YAML mapping — detect common syntax mistakes
                if !deps_value.is_null() {
                    let value_str = serde_yaml::to_string(deps_value)
                        .unwrap_or_else(|_| format!("{deps_value:?}"));
                    let hint = if value_str.contains('=') {
                        "\n\nIt looks like '=' was used instead of ':'. YAML uses ':' for key-value pairs.\n\
                         Example:\n  packages:\n    curl: \"*\"\n    iperf3: \"*\""
                    } else {
                        "\n\nExpected a YAML mapping (key: value pairs).\n\
                         Example:\n  packages:\n    curl: \"*\"\n    iperf3: \"*\""
                    };
                    return Err(anyhow::anyhow!(
                        "Invalid 'packages' format in extension '{ext_name}': \
                         expected a mapping but got: {}{hint}",
                        value_str.trim()
                    ));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = BuildCommand::new(
            "avocado.yaml".to_string(),
            true,
            Some("my-runtime".to_string()),
            Some("my-extension".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert_eq!(cmd.runtime, Some("my-runtime".to_string()));
        assert_eq!(cmd.extension, Some("my-extension".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_all_runtimes() {
        let cmd = BuildCommand::new(
            "config.toml".to_string(),
            false,
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.extension, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_new_with_runtime() {
        let cmd = BuildCommand::new(
            "avocado.yaml".to_string(),
            false,
            Some("test-runtime".to_string()),
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.extension, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_new_with_extension() {
        let cmd = BuildCommand::new(
            "avocado.yaml".to_string(),
            false,
            None,
            Some("test-extension".to_string()),
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.extension, Some("test-extension".to_string()));
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
