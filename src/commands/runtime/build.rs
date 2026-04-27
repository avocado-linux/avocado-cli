use crate::commands::initramfs::image::generate_initramfs_build_script;
use crate::commands::rootfs::image::{generate_rootfs_build_script, NAMESPACE_UUID};
use crate::commands::sdk::SdkCompileCommand;
use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer, TuiContext},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    stamps::{
        compute_runtime_input_hash, generate_batch_read_stamps_script, generate_write_stamp_script,
        resolve_required_stamps_for_runtime_build, validate_stamps_batch, Stamp, StampComponent,
        StampOutputs,
    },
    target::resolve_target_required,
    tui::{TaskId, TuiGuard},
    update_repo,
};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct RuntimeBuildCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
    pub tui_context: Option<TuiContext>,
}

impl RuntimeBuildCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
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

    /// Build a one-entry `env_vars` map carrying `AVOCADO_RUNTIME` for the
    /// container entrypoint. The runtime name is intrinsic to this command
    /// (every `runtime build` is for a specific runtime), so this is always
    /// populated.
    fn runtime_env_vars(&self) -> Option<std::collections::HashMap<String, String>> {
        let mut m = std::collections::HashMap::new();
        m.insert("AVOCADO_RUNTIME".to_string(), self.runtime_name.clone());
        Some(m)
    }

    pub async fn execute(&mut self) -> Result<()> {
        // Create standalone TUI if not provided by parent orchestrator
        let name = self.runtime_name.clone();
        let tui_guard = if self.tui_context.is_none() {
            Some(TuiGuard::new(
                TaskId::RuntimeBuild(name.clone()),
                &format!("runtime build {}", name),
                self.verbose,
            ))
        } else {
            None
        };
        if self.tui_context.is_none() {
            self.tui_context = tui_guard.as_ref().and_then(|g| g.tui_context());
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
        let parsed = &composed.merged_value;

        // Merge container args from config and CLI with environment variable expansion
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        // Get runtime configuration
        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        // Check if runtime exists
        let runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        // Get target from runtime config
        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // Initialize SDK container helper
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

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

        // Execute the build and ensure cleanup
        let result = self
            .execute_build_internal(
                config,
                parsed,
                container_image,
                &target_arch,
                &merged_container_args,
                repo_url.as_ref(),
                repo_release.as_ref(),
                &container_helper,
                runs_on_context.as_ref(),
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

    /// Internal implementation of the build logic
    #[allow(clippy::too_many_arguments)]
    async fn execute_build_internal(
        &self,
        config: &crate::utils::config::Config,
        parsed: &serde_yaml::Value,
        container_image: &str,
        target_arch: &str,
        merged_container_args: &Option<Vec<String>>,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_helper: &SdkContainer,
        runs_on_context: Option<&RunsOnContext>,
    ) -> Result<()> {
        // Validate stamps before proceeding (unless --no-stamps)
        if !self.no_stamps {
            // Get detailed extension dependencies for this runtime
            // This distinguishes between local, external, and versioned extensions
            let ext_deps = config.get_runtime_extension_dependencies_detailed(
                &self.runtime_name,
                target_arch,
                &self.config_path,
            )?;

            // Resolve required stamps for runtime build
            // - Local/External extensions: require install + build
            // - Versioned extensions: require install only (prebuilt from package repo)
            let required = resolve_required_stamps_for_runtime_build(&self.runtime_name, &ext_deps);

            // Batch all stamp reads into a single container invocation for performance
            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.to_string(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                // runs_on handled by shared context
                sdk_arch: self.sdk_arch.clone(),
                tui_context: self.tui_context.clone(),
                env_vars: self.runtime_env_vars(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            // Compute current inputs for staleness detection so that changes to
            // extension packages (e.g. from path-based sources) are detected.
            // Only compare against Runtime stamps — SDK/compile-deps stamps use their own hash.
            // Use get_merged_runtime_config to match how the install stamp was created.
            let merged_runtime = config
                .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)
                .ok()
                .flatten();
            let current_inputs = merged_runtime
                .as_ref()
                .and_then(|mr| compute_runtime_input_hash(mr, &self.runtime_name, parsed).ok());
            let validation = validate_stamps_batch(
                &required,
                output.as_deref().unwrap_or(""),
                current_inputs
                    .as_ref()
                    .map(|i| (&StampComponent::Runtime, i)),
            );

            if !validation.is_satisfied() {
                validation
                    .into_error(&format!("Cannot build runtime '{}'", self.runtime_name))
                    .print_and_exit();
            }
        }

        print_info(
            &format!("Building runtime images for '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Check for kernel configuration in the merged runtime config
        let merged_runtime =
            config.get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?;
        let kernel_config = merged_runtime.as_ref().and_then(|v| {
            Config::get_kernel_config_from_runtime(v, config.kernel.as_ref())
                .ok()
                .flatten()
        });

        // Handle kernel cross-compilation if kernel.compile is configured
        if let Some(ref kc) = kernel_config {
            if let (Some(ref compile_section), Some(ref install_script)) =
                (&kc.compile, &kc.install)
            {
                print_info(
                    &format!(
                        "Compiling kernel via sdk.compile.{compile_section} for runtime '{}'",
                        self.runtime_name
                    ),
                    OutputLevel::Normal,
                );

                // Step 1: Run the SDK compile section
                let compile_command = SdkCompileCommand::new(
                    self.config_path.clone(),
                    self.verbose,
                    vec![compile_section.clone()],
                    Some(target_arch.to_string()),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_sdk_arch(self.sdk_arch.clone());

                compile_command.execute().await.with_context(|| {
                    format!(
                        "Failed to compile kernel SDK section '{compile_section}' for runtime '{}'",
                        self.runtime_name
                    )
                })?;

                // Step 2: Run the kernel install script in the SDK container
                // The install script copies kernel artifacts to $AVOCADO_RUNTIME_BUILD_DIR
                let runtime_build_dir = format!(
                    "/opt/_avocado/{}/runtimes/{}",
                    target_arch, self.runtime_name
                );
                let install_cmd = format!(
                    r#"mkdir -p "{runtime_build_dir}" && if [ -f '{install_script}' ]; then echo 'Running kernel install script: {install_script}'; export AVOCADO_RUNTIME_BUILD_DIR="{runtime_build_dir}"; export AVOCADO_BUILD_DIR="$AVOCADO_SDK_PREFIX/build/{compile_section}"; bash '{install_script}'; else echo 'Kernel install script {install_script} not found.'; ls -la; exit 1; fi"#
                );

                if self.verbose {
                    print_info(
                        &format!("Running kernel install script: {install_script}"),
                        OutputLevel::Normal,
                    );
                }

                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target_arch.to_string(),
                    command: install_cmd,
                    verbose: self.verbose,
                    source_environment: true,
                    interactive: false,
                    repo_url: repo_url.cloned(),
                    repo_release: repo_release.cloned(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    sdk_arch: self.sdk_arch.clone(),
                    tui_context: self.tui_context.clone(),
                    env_vars: self.runtime_env_vars(),
                    ..Default::default()
                };

                let install_result =
                    run_container_command(container_helper, run_config, runs_on_context)
                        .await
                        .context("Failed to run kernel install script")?;

                if !install_result {
                    return Err(anyhow::anyhow!(
                        "Kernel install script '{}' failed for runtime '{}'",
                        install_script,
                        self.runtime_name
                    ));
                }

                print_success(
                    &format!(
                        "Kernel compiled and installed for runtime '{}'",
                        self.runtime_name
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        // Collect extensions with versions for AVOCADO_EXT_LIST
        // This ensures the build scripts know exactly which extension versions to use
        let resolved_extensions = self
            .collect_runtime_extensions(
                parsed,
                config,
                &self.runtime_name,
                target_arch,
                &self.config_path,
                container_image,
                merged_container_args.clone(),
            )
            .await?;

        // Build var image
        let build_script =
            self.create_build_script(config, parsed, target_arch, &resolved_extensions)?;

        if self.verbose {
            print_info(
                "Executing complete image build script.",
                OutputLevel::Normal,
            );
        }

        // Get stone include paths if configured
        let mut env_vars = std::collections::HashMap::new();

        // AVOCADO_RUNTIME tells the container entrypoint to scope
        // $AVOCADO_EXT_SYSROOTS to runtimes/<r>/extensions (and create the
        // legacy compat symlink). Always set for runtime build since the
        // runtime name is intrinsic.
        env_vars.insert("AVOCADO_RUNTIME".to_string(), self.runtime_name.clone());

        // Set AVOCADO_EXT_LIST with versioned extension names
        if !resolved_extensions.is_empty() {
            env_vars.insert(
                "AVOCADO_EXT_LIST".to_string(),
                resolved_extensions.join(" "),
            );
        }

        // Set AVOCADO_VERBOSE=1 when verbose mode is enabled
        if self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        if let Some(stone_paths) = config.get_stone_include_paths_for_runtime(
            &self.runtime_name,
            target_arch,
            &self.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_INCLUDE_PATHS".to_string(), stone_paths);
        }

        // Get stone manifest if configured
        if let Some(stone_manifest) = config.get_stone_manifest_for_runtime(
            &self.runtime_name,
            target_arch,
            &self.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_MANIFEST".to_string(), stone_manifest);
        }

        // Set AVOCADO_RUNTIME_BUILD_DIR
        env_vars.insert(
            "AVOCADO_RUNTIME_BUILD_DIR".to_string(),
            format!(
                "/opt/_avocado/{}/runtimes/{}",
                target_arch, self.runtime_name
            ),
        );

        // Set AVOCADO_DISTRO_VERSION if configured
        if let Some(distro_version) = config.get_distro_version() {
            env_vars.insert("AVOCADO_DISTRO_VERSION".to_string(), distro_version.clone());
        }

        // Set kernel-related environment variables for the avocado-build hook
        if let Some(ref kc) = kernel_config {
            if kc.compile.is_some() {
                env_vars.insert("AVOCADO_KERNEL_SOURCE".to_string(), "compile".to_string());
            } else if let Some(ref package) = kc.package {
                env_vars.insert("AVOCADO_KERNEL_SOURCE".to_string(), "package".to_string());
                env_vars.insert("AVOCADO_KERNEL_PACKAGE".to_string(), package.clone());
            }
        }

        let env_vars = if env_vars.is_empty() {
            None
        } else {
            Some(env_vars)
        };

        // If any extension in this runtime declares docker_images, add --privileged
        // to container args so dockerd can run inside the SDK container (Docker-in-Docker)
        let build_container_args = {
            let ext_list: Vec<&str> = merged_runtime
                .as_ref()
                .and_then(|rt| rt.get("extensions"))
                .and_then(|e| e.as_sequence())
                .map(|seq| seq.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let has_docker_images = ext_list.iter().any(|ext_name| {
                parsed
                    .get("extensions")
                    .and_then(|e| e.get(*ext_name))
                    .map(|ext| !crate::utils::config::get_docker_images(ext).is_empty())
                    .unwrap_or(false)
            });

            if has_docker_images {
                let mut args = merged_container_args.clone().unwrap_or_default();
                if !args.iter().any(|a| a == "--privileged") {
                    args.push("--privileged".to_string());
                }
                Some(args)
            } else {
                merged_container_args.clone()
            }
        };

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true, // need environment for build
            interactive: false,       // build script runs non-interactively
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: build_container_args,
            dnf_args: self.dnf_args.clone(),
            env_vars,
            // runs_on handled by shared context
            sdk_arch: self.sdk_arch.clone(),
            tui_context: self.tui_context.clone(),
            ..Default::default()
        };
        let complete_result = run_container_command(container_helper, run_config, runs_on_context)
            .await
            .context("Failed to build complete image")?;

        if !complete_result {
            return Err(anyhow::anyhow!("Failed to build complete image"));
        }

        print_success(
            &format!("Successfully built runtime '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Write runtime build stamp (unless --no-stamps)
        if !self.no_stamps {
            let merged_runtime = config
                .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
                .unwrap_or_default();
            let inputs = compute_runtime_input_hash(&merged_runtime, &self.runtime_name, parsed)?;
            let outputs = StampOutputs::default();
            let stamp = Stamp::runtime_build(&self.runtime_name, target_arch, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.to_string(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                // runs_on handled by shared context
                sdk_arch: self.sdk_arch.clone(),
                tui_context: self.tui_context.clone(),
                env_vars: self.runtime_env_vars(),
                ..Default::default()
            };

            let stamp_ok =
                run_container_command(container_helper, run_config, runs_on_context).await?;
            if !stamp_ok {
                return Err(anyhow::anyhow!(
                    "Failed to write build stamp for runtime '{}'",
                    self.runtime_name
                ));
            }

            if self.verbose {
                print_info(
                    &format!("Wrote build stamp for runtime '{}'.", self.runtime_name),
                    OutputLevel::Normal,
                );
            }
        }

        // Generate TUF delegation staging and write into the build volume.
        // If signing keys are not configured this step is skipped with a warning.
        let project_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        if let Err(e) = self
            .generate_tuf_staging(
                config,
                container_image,
                target_arch,
                merged_container_args,
                repo_url,
                repo_release,
                container_helper,
                runs_on_context,
                project_dir,
            )
            .await
        {
            print_info(
                &format!("Skipping TUF delegation staging: {e:#}"),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Generate a shell script that enumerates built artifacts, computes their SHA-256
    /// hashes and sizes, and outputs structured JSON for TUF metadata signing.
    fn create_hash_collection_script(&self) -> String {
        format!(
            r#"
set -e

RUNTIME_NAME="{runtime_name}"
VAR_STAGING="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
IMAGES_DIR="$VAR_STAGING/lib/avocado/images"

# Find the active manifest (prefer active symlink over find)
ACTIVE_LINK="$VAR_STAGING/lib/avocado/active"
RUNTIME_UUID=""
if [ -L "$ACTIVE_LINK" ]; then
    ACTIVE_TARGET=$(readlink "$ACTIVE_LINK")
    MANIFEST_FILE="$VAR_STAGING/lib/avocado/$ACTIVE_TARGET/manifest.json"
    # Extract UUID from path like "runtimes/<uuid>"
    RUNTIME_UUID=$(basename "$ACTIVE_TARGET")
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_STAGING/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
    if [ -n "$MANIFEST_FILE" ]; then
        RUNTIME_UUID=$(basename "$(dirname "$MANIFEST_FILE")")
    fi
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    echo "ERROR: No manifest.json found in $VAR_STAGING/lib/avocado/runtimes/" >&2
    exit 1
fi
if [ -z "$RUNTIME_UUID" ]; then
    echo "ERROR: Could not determine runtime UUID from active symlink" >&2
    exit 1
fi

# Start building JSON output
echo -n '{{"targets":['

FIRST=true

# Hash the manifest
HASH=$(sha256sum "$MANIFEST_FILE" | awk '{{print $1}}')
SIZE=$(stat -c '%s' "$MANIFEST_FILE")
echo -n '{{"name":"manifest.json","sha256":"'"$HASH"'","size":'"$SIZE"'}}'
FIRST=false

# Hash all image files (content-addressable by UUIDv5) — .raw and .kab
if [ -d "$IMAGES_DIR" ]; then
    for IMG_FILE in "$IMAGES_DIR"/*.raw "$IMAGES_DIR"/*.kab; do
        [ -f "$IMG_FILE" ] || continue
        BASENAME=$(basename "$IMG_FILE")
        HASH=$(sha256sum "$IMG_FILE" | awk '{{print $1}}')
        SIZE=$(stat -c '%s' "$IMG_FILE")
        if [ "$FIRST" = "false" ]; then
            echo -n ','
        fi
        echo -n '{{"name":"'"$BASENAME"'","sha256":"'"$HASH"'","size":'"$SIZE"'}}'
        FIRST=false
    done
fi

echo -n ']'

# Include root.json if present (Level 2 / Sideload). Absent at Level 1.
ROOT_JSON_FILE="$VAR_STAGING/lib/avocado/metadata/root.json"
if [ -f "$ROOT_JSON_FILE" ]; then
    ROOT_JSON_ESCAPED=$(python3 -c "import json,sys; print(json.dumps(open(sys.argv[1]).read()))" "$ROOT_JSON_FILE")
    echo -n ',"root_json":'
    echo -n "$ROOT_JSON_ESCAPED"
fi

echo -n ',"runtime_uuid":"'"$RUNTIME_UUID"'"'
echo -n '}}'
"#,
            runtime_name = self.runtime_name,
        )
    }

    /// Collect artifact hashes from the build volume, sign TUF metadata, and write the
    /// delegation files into `lib/avocado/tuf-staging/` inside the build volume so that
    /// `avocado connect upload` can include them without a separate `avocado deploy` step.
    #[allow(clippy::too_many_arguments)]
    async fn generate_tuf_staging(
        &self,
        config: &crate::utils::config::Config,
        container_image: &str,
        target_arch: &str,
        merged_container_args: &Option<Vec<String>>,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_helper: &SdkContainer,
        runs_on_context: Option<&RunsOnContext>,
        project_dir: &std::path::Path,
    ) -> Result<()> {
        // Resolve signing keys. At Level 0 (Connect, no local key), skip TUF staging entirely.
        let signing_key_name = config.get_runtime_signing_key_name(&self.runtime_name);
        let content_key_name = config.get_runtime_content_key_name(&self.runtime_name);
        let is_connect = config
            .connect
            .as_ref()
            .and_then(|c| c.org.as_ref())
            .is_some();

        let mut resolved_signing_key_name = signing_key_name.clone();
        let mut signer =
            crate::utils::update_signing::resolve_signing_key(signing_key_name.as_deref())?;

        // Determine build level based on key configuration:
        // - Level 0: Connect project, no signing key, no content key → server manages everything
        // - Level 1: Connect project, content key only → CLI signs delegation, server signs rest
        // - Level 2: Connect project, signing key (+ optional content key) → CLI signs everything
        // - Sideload: no Connect, auto-generate dev key if needed
        let content_signer = crate::utils::update_signing::resolve_content_key(
            content_key_name.as_deref(),
            None, // Don't fall back to signing key yet — check Level 1 first
        )?;

        if let (None, Some(content_signer)) = (&signer, content_signer) {
            if is_connect {
                // Level 1: content key only, server manages root/targets/snapshot/timestamp
                print_info(
                    "Connect Level 1: generating content delegation (server manages root signing)",
                    OutputLevel::Normal,
                );
                return self
                    .run_content_only_staging(
                        config,
                        container_image,
                        target_arch,
                        merged_container_args,
                        repo_url,
                        repo_release,
                        container_helper,
                        runs_on_context,
                        project_dir,
                        &content_signer,
                    )
                    .await;
            }
        }

        if signer.is_none() {
            if is_connect {
                // Level 0: no signing key, no content key — server manages everything
                print_info(
                    "Connect Level 0: skipping TUF staging (server manages signing)",
                    OutputLevel::Normal,
                );
                return Ok(());
            } else {
                // Auto-generate a development signing key for local builds
                let dev_key_name = crate::utils::signing_keys::ensure_dev_signing_key()?;
                signer = crate::utils::update_signing::resolve_signing_key(Some(&dev_key_name))?;
                resolved_signing_key_name = Some(dev_key_name);
            }
        }
        let signer = signer.unwrap();

        // Level 2 or Sideload: resolve content signer (falls back to signing key)
        let content_signer = crate::utils::update_signing::resolve_content_key(
            content_key_name.as_deref(),
            resolved_signing_key_name.as_deref(),
        )?
        .unwrap_or_else(|| {
            // Sideload: no content key, reuse signing key for content delegation
            // This is handled by the caller providing the same key
            unreachable!("signing key is set, so content key resolution should return Some")
        });

        // Phase 1: Collect artifact hashes from inside the build volume.
        let hash_script = self.create_hash_collection_script();
        let hash_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: hash_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: merged_container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            tui_context: self.tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };
        let hash_output =
            run_container_command_with_output(container_helper, hash_run_config, runs_on_context)
                .await?
                .context("Hash collection script produced no output")?;

        let collection: update_repo::HashCollectionOutput =
            serde_json::from_str(&hash_output).context("Failed to parse hash collection output")?;

        // Phase 2: Generate and sign TUF metadata on the host.
        let repo_metadata = update_repo::generate_repo_metadata(
            &collection.targets,
            &collection.runtime_uuid,
            &signer,
            &content_signer,
        )?;

        // Phase 3: Write signed files into the build volume via a container run.
        // We write them to a temp dir under the project directory (accessible inside
        // the container as /opt/src/.tuf-staging-tmp/), then copy into the volume.
        let tmp_dir = project_dir.join(".tuf-staging-tmp");
        let tmp_delegations = tmp_dir.join("delegations");
        std::fs::create_dir_all(&tmp_delegations)
            .context("Failed to create TUF staging temp directory")?;

        std::fs::write(tmp_dir.join("targets.json"), &repo_metadata.targets_json)
            .context("Failed to write targets.json to temp dir")?;
        std::fs::write(
            tmp_delegations.join(format!("runtime-{}.json", collection.runtime_uuid)),
            &repo_metadata.delegated_targets_json,
        )
        .context("Failed to write delegated targets to temp dir")?;

        let runtime_name = &self.runtime_name;
        let runtime_uuid = &collection.runtime_uuid;
        let copy_script = format!(
            r#"set -euo pipefail
DEST="$AVOCADO_PREFIX/runtimes/{runtime_name}/var-staging/lib/avocado/tuf-staging"
mkdir -p "$DEST/delegations"
cp /opt/src/.tuf-staging-tmp/targets.json "$DEST/targets.json"
cp /opt/src/.tuf-staging-tmp/delegations/runtime-{runtime_uuid}.json \
   "$DEST/delegations/runtime-{runtime_uuid}.json"
"#
        );

        let copy_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: copy_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: merged_container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            tui_context: self.tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };
        run_container_command(container_helper, copy_run_config, runs_on_context).await?;

        // Clean up host temp files.
        let _ = std::fs::remove_dir_all(&tmp_dir);

        print_info(
            &format!(
                "Generated deployment delegation staging for runtime '{}'.",
                self.runtime_name
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Level 1: generate only the content delegation metadata (no root/snapshot/timestamp).
    /// The server manages those roles; the CLI only signs the delegated-targets file.
    #[allow(clippy::too_many_arguments)]
    async fn run_content_only_staging(
        &self,
        _config: &crate::utils::config::Config,
        container_image: &str,
        target_arch: &str,
        merged_container_args: &Option<Vec<String>>,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_helper: &SdkContainer,
        runs_on_context: Option<&RunsOnContext>,
        project_dir: &std::path::Path,
        content_signer: &crate::utils::update_signing::TufSigner,
    ) -> Result<()> {
        // Phase 1: Collect artifact hashes from inside the build volume.
        let hash_script = self.create_hash_collection_script();
        let hash_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: hash_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: merged_container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            tui_context: self.tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };
        let hash_output =
            run_container_command_with_output(container_helper, hash_run_config, runs_on_context)
                .await?
                .context("Hash collection script produced no output")?;

        let collection: update_repo::HashCollectionOutput =
            serde_json::from_str(&hash_output).context("Failed to parse hash collection output")?;

        // Phase 2: Generate content-only metadata (delegation + unsigned targets carrier).
        let metadata = update_repo::generate_content_only_metadata(
            &collection.targets,
            &collection.runtime_uuid,
            content_signer,
        )?;

        // Phase 3: Write files into the build volume.
        let tmp_dir = project_dir.join(".tuf-staging-tmp");
        let tmp_delegations = tmp_dir.join("delegations");
        std::fs::create_dir_all(&tmp_delegations)
            .context("Failed to create TUF staging temp directory")?;

        std::fs::write(tmp_dir.join("targets.json"), &metadata.targets_json)
            .context("Failed to write targets.json to temp dir")?;
        std::fs::write(
            tmp_delegations.join(format!("runtime-{}.json", collection.runtime_uuid)),
            &metadata.delegated_targets_json,
        )
        .context("Failed to write delegated targets to temp dir")?;

        let runtime_name = &self.runtime_name;
        let runtime_uuid = &collection.runtime_uuid;
        let copy_script = format!(
            r#"set -euo pipefail
DEST="$AVOCADO_PREFIX/runtimes/{runtime_name}/var-staging/lib/avocado/tuf-staging"
mkdir -p "$DEST/delegations"
cp /opt/src/.tuf-staging-tmp/targets.json "$DEST/targets.json"
cp /opt/src/.tuf-staging-tmp/delegations/runtime-{runtime_uuid}.json \
   "$DEST/delegations/runtime-{runtime_uuid}.json"
"#
        );

        let copy_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: copy_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: merged_container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            tui_context: self.tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };
        run_container_command(container_helper, copy_run_config, runs_on_context).await?;

        let _ = std::fs::remove_dir_all(&tmp_dir);

        print_info(
            &format!(
                "Generated content delegation staging for runtime '{}' (Level 1).",
                self.runtime_name
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn create_build_script(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        target_arch: &str,
        resolved_extensions: &[String],
    ) -> Result<String> {
        // Get merged runtime configuration including target-specific dependencies
        let merged_runtime = config
            .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
            .with_context(|| {
                format!(
                    "Runtime '{}' not found or has no configuration for target '{}'",
                    self.runtime_name, target_arch
                )
            })?;

        // Extract extension names from the `extensions` array
        let mut required_extensions = HashSet::new();
        let _extension_type_overrides: HashMap<String, Vec<String>> = HashMap::new();

        // Collect extensions from the new `extensions` array format
        if let Some(extensions) = merged_runtime
            .get("extensions")
            .and_then(|e| e.as_sequence())
        {
            for ext in extensions {
                if let Some(ext_name) = ext.as_str() {
                    required_extensions.insert(ext_name.to_string());
                }
            }
        }

        // Recursively discover all extension dependencies (including nested external extensions)
        let all_required_extensions =
            self.find_all_extension_dependencies(config, &required_extensions, target_arch)?;

        // Build a map from extension name to versioned name from resolved_extensions
        // Format of resolved_extensions items: "ext_name-version" (e.g., "my-ext-1.0.0")
        let mut ext_version_map: HashMap<String, String> = HashMap::new();
        for versioned_name in resolved_extensions {
            // Parse "ext_name-version" - find the last occurrence of -X.Y.Z pattern
            if let Some(idx) = versioned_name.rfind('-') {
                let (name, version_with_dash) = versioned_name.split_at(idx);
                let version = &version_with_dash[1..]; // Skip the leading '-'
                                                       // Verify it looks like a version (starts with a digit)
                if version
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                {
                    ext_version_map.insert(name.to_string(), versioned_name.clone());
                }
            }
        }

        // Build copy commands for required extensions
        let mut copy_commands = Vec::new();
        let mut processed_extensions = HashSet::new();

        // Process local extensions defined in [ext.*] sections
        if let Some(ext_config) = parsed.get("extensions").and_then(|v| v.as_mapping()) {
            for (ext_name_val, ext_data) in ext_config {
                if let Some(ext_name) = ext_name_val.as_str() {
                    if all_required_extensions.contains(ext_name) {
                        let ext_version = ext_data
                            .get("version")
                            .map(|v| {
                                v.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                                    format!(
                                        "{}",
                                        v.as_i64()
                                            .or_else(|| v.as_f64().map(|f| f as i64))
                                            .unwrap_or(0)
                                    )
                                })
                            })
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Extension '{ext_name}' is missing a 'version' field. \
                                 Check that the extension config was parsed and merged correctly."
                                )
                            })?;

                        let ext_suffix = crate::utils::config::get_ext_image_type(ext_data)
                            .map(|t| {
                                if t == "kab" {
                                    "kab".to_string()
                                } else {
                                    "raw".to_string()
                                }
                            })
                            .unwrap_or_else(|| "raw".to_string());
                        copy_commands.push(format!(
                            r#"
if [ -f "$AVOCADO_PREFIX/output/extensions/{ext_name}-{ext_version}.{ext_suffix}" ]; then
    cp -f "$AVOCADO_PREFIX/output/extensions/{ext_name}-{ext_version}.{ext_suffix}" "$RUNTIME_EXT_DIR/{ext_name}-{ext_version}.{ext_suffix}"
    echo "  Copied: {ext_name}-{ext_version}.{ext_suffix}"
fi"#
                        ));
                        processed_extensions.insert(ext_name.to_string());
                    }
                }
            }
        }

        // Process external/versioned extensions (those required but not defined locally)
        for ext_name in &all_required_extensions {
            if !processed_extensions.contains(ext_name) {
                if let Some(versioned_name) = ext_version_map.get(ext_name) {
                    copy_commands.push(format!(
                        r#"
if [ -f "$AVOCADO_PREFIX/output/extensions/{versioned_name}.raw" ]; then
    cp -f "$AVOCADO_PREFIX/output/extensions/{versioned_name}.raw" "$RUNTIME_EXT_DIR/{versioned_name}.raw"
    echo "  Copied: {versioned_name}.raw"
else
    echo "ERROR: Extension image not found: $AVOCADO_PREFIX/output/extensions/{versioned_name}.raw"
    exit 1
fi"#
                    ));
                } else {
                    copy_commands.push(format!(
                        r#"
EXT_FILE=$(ls "$AVOCADO_PREFIX/output/extensions/{ext_name}"-*.raw 2>/dev/null | head -n 1)
if [ -n "$EXT_FILE" ]; then
    EXT_BASENAME=$(basename "$EXT_FILE")
    cp -f "$EXT_FILE" "$RUNTIME_EXT_DIR/$EXT_BASENAME"
    echo "  Copied: $EXT_BASENAME"
fi"#
                    ));
                }
            }
        }

        let copy_section = if copy_commands.is_empty() {
            "# No extensions to copy".to_string()
        } else {
            copy_commands.join("\n")
        };

        // Generate build ID and timestamp for the manifest
        let build_id = uuid::Uuid::new_v4().to_string();
        let built_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // Runtime version: use runtimes.<name>.version if declared, otherwise short UUID
        let runtime_version = config
            .get_runtime_version(&self.runtime_name)
            .unwrap_or_else(|| build_id[..8].to_string());

        // Build "name:version:image_type" triples for dynamic manifest generation.
        // The shell script will compute SHA-256 + UUIDv5 image IDs at build time.
        let ext_info_pairs: Vec<String> = resolved_extensions
            .iter()
            .map(|versioned_name| {
                let (name, version) = if let Some(idx) = versioned_name.rfind('-') {
                    let (n, v_with_dash) = versioned_name.split_at(idx);
                    let v = &v_with_dash[1..];
                    if v.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        (n.to_string(), v.to_string())
                    } else {
                        (versioned_name.clone(), "0.0.0".to_string())
                    }
                } else {
                    (versioned_name.clone(), "0.0.0".to_string())
                };
                // Look up image type from parsed config (image.type field)
                let image_type = parsed
                    .get("extensions")
                    .and_then(|e| e.get(&name))
                    .and_then(crate::utils::config::get_ext_image_type)
                    .unwrap_or_else(|| "raw".to_string());
                format!("{name}:{version}:{image_type}")
            })
            .collect();
        let ext_info_str = ext_info_pairs.join(" ");

        let namespace_uuid = crate::utils::update_repo::AVOCADO_IMAGE_NAMESPACE.to_string();

        let manifest_section = format!(
            r#"
# Generate Avocado Runtime Manifest with content-addressable image IDs
IMAGES_DIR="$VAR_DIR/lib/avocado/images"
mkdir -p "$IMAGES_DIR"
BUILD_ID="{build_id}"
BUILT_AT="{built_at}"
RUNTIME_VERSION="{runtime_version}"
# Clean stale runtime manifests from previous builds
rm -rf "$VAR_DIR/lib/avocado/runtimes"
MANIFEST_DIR="$VAR_DIR/lib/avocado/runtimes/$BUILD_ID"
mkdir -p "$MANIFEST_DIR"

export AVOCADO_NS_UUID="{namespace_uuid}"
export AVOCADO_RT_EXT_DIR="$RUNTIME_EXT_DIR"
export AVOCADO_IMAGES_DIR="$IMAGES_DIR"
export AVOCADO_MANIFEST_PATH="$MANIFEST_DIR/manifest.json"
export AVOCADO_SPOT_HASHES_PATH="$MANIFEST_DIR/spot_hashes.json"
export AVOCADO_BUILD_ID="$BUILD_ID"
export AVOCADO_BUILT_AT="$BUILT_AT"
export AVOCADO_RUNTIME_NAME="{runtime_name}"
export AVOCADO_RUNTIME_VERSION="$RUNTIME_VERSION"
export AVOCADO_EXT_PAIRS="{ext_info_str}"

echo "Computing content-addressable image IDs..."
python3 << 'PYEOF'
import json, hashlib, uuid, os, shutil

namespace = uuid.UUID(os.environ["AVOCADO_NS_UUID"])
runtime_ext_dir = os.environ["AVOCADO_RT_EXT_DIR"]
images_dir = os.environ["AVOCADO_IMAGES_DIR"]
manifest_path = os.environ["AVOCADO_MANIFEST_PATH"]
build_id = os.environ["AVOCADO_BUILD_ID"]
built_at = os.environ["AVOCADO_BUILT_AT"]
runtime_name = os.environ["AVOCADO_RUNTIME_NAME"]
runtime_version = os.environ["AVOCADO_RUNTIME_VERSION"]
ext_pairs_str = os.environ.get("AVOCADO_EXT_PAIRS", "")

ext_pairs = ext_pairs_str.split() if ext_pairs_str else []

extensions = []
for pair in ext_pairs:
    parts = pair.split(":", 2)
    name, version = parts[0], parts[1]
    image_type = parts[2] if len(parts) > 2 else "raw"
    ext_suffix = ".kab" if image_type == "kab" else ".raw"
    img_file = os.path.join(runtime_ext_dir, name + "-" + version + ext_suffix)
    if not os.path.isfile(img_file):
        print("WARNING: Extension image not found: " + img_file)
        continue
    with open(img_file, "rb") as f:
        sha256 = hashlib.sha256(f.read()).hexdigest()
    image_id = str(uuid.uuid5(namespace, sha256))
    dest = os.path.join(images_dir, image_id + ext_suffix)
    shutil.copy2(img_file, dest)
    print("  Image: " + name + "-" + version + ext_suffix + " -> " + image_id + ext_suffix)
    entry = dict(name=name, version=version, image_id=image_id, sha256=sha256)
    if image_type != "raw":
        entry["image_type"] = image_type
    extensions.append(entry)

manifest = dict(
    manifest_version=2,
    id=build_id,
    built_at=built_at,
    runtime=dict(name=runtime_name, version=runtime_version),
    extensions=extensions,
)

with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
print("Created runtime manifest with " + str(len(extensions)) + " extension(s)")

# Clean up stale extension images (os_bundle cleanup happens after stone bundle)
current_image_files = set()
for ext in extensions:
    suffix = ".kab" if ext.get("image_type") == "kab" else ".raw"
    current_image_files.add(ext["image_id"] + suffix)
for fname in os.listdir(images_dir):
    if (fname.endswith(".raw") or fname.endswith(".kab")) and fname not in current_image_files:
        stale_path = os.path.join(images_dir, fname)
        os.remove(stale_path)
        print("  Removed stale image: " + fname)

# Generate spot_hashes.json for fast integrity checking at merge time.
# Hashes file_size (8 LE bytes) + first N bytes + last N bytes of each image.
spot_check_bytes = int(os.environ.get("AVOCADO_SPOT_CHECK_BYTES", "4096"))
spot_hashes_path = os.environ.get("AVOCADO_SPOT_HASHES_PATH", "")

def compute_spot_hash(filepath, spot_size):
    import struct
    file_size = os.path.getsize(filepath)
    h = hashlib.sha256()
    h.update(struct.pack("<Q", file_size))
    with open(filepath, "rb") as f:
        if file_size == 0:
            pass
        elif file_size <= spot_size * 2:
            h.update(f.read())
        else:
            h.update(f.read(spot_size))
            f.seek(-spot_size, 2)
            h.update(f.read(spot_size))
    return h.hexdigest()

if spot_hashes_path:
    spot_hashes = {{}}
    for ext in extensions:
        suffix = ".kab" if ext.get("image_type") == "kab" else ".raw"
        fname = ext["image_id"] + suffix
        fpath = os.path.join(images_dir, fname)
        if os.path.isfile(fpath):
            spot_hashes[fname] = compute_spot_hash(fpath, spot_check_bytes)
    cache = dict(version=1, spot_check_bytes=spot_check_bytes, hashes=spot_hashes)
    with open(spot_hashes_path, "w") as f:
        json.dump(cache, f, indent=2)
    print("Created spot hash cache with " + str(len(spot_hashes)) + " image(s)")
PYEOF

ln -sfn "runtimes/$BUILD_ID" "$VAR_DIR/lib/avocado/active"
echo "Created runtime manifest: runtimes/$BUILD_ID/manifest.json"
echo "Set active runtime -> runtimes/$BUILD_ID""#,
            runtime_name = self.runtime_name,
        );

        // Generate update authority (root.json) for verified updates.
        // Level 0 (Connect, no signing key): skip — device gets root.json at claim time.
        // Level 2 (Connect + signing key): multi-key root.json with server key.
        // Sideload (signing key, no server): single-key root.json.
        let signing_key_name = config.get_runtime_signing_key_name(&self.runtime_name);
        let server_key = config.get_server_key_for_runtime(&self.runtime_name);
        let is_connect = config
            .connect
            .as_ref()
            .and_then(|c| c.org.as_ref())
            .is_some();

        let mut signer =
            crate::utils::update_signing::resolve_signing_key(signing_key_name.as_deref())?;

        // Auto-generate a development signing key if none configured and not a Connect project
        if signer.is_none() && !is_connect {
            let dev_key_name = crate::utils::signing_keys::ensure_dev_signing_key()?;
            signer = crate::utils::update_signing::resolve_signing_key(Some(&dev_key_name))?;
        }

        let update_authority_section = match (&signer, is_connect) {
            // Level 0/1: Connect project, no local signing key — device gets root.json at claim time
            (None, true) => {
                print_info(
                    "Connect Level 0: root.json will be delivered at device claim time",
                    OutputLevel::Normal,
                );
                "# Level 0: root.json delivered at claim time (not baked into image)".to_string()
            }
            // Level 2: user's root key + Connect with server key
            (Some(signer), true) => {
                match &server_key {
                    Some(server_key_hex) => {
                        let keyid =
                            crate::utils::update_signing::compute_key_id_from_hex(server_key_hex);
                        print_info(
                            &format!(
                                "Generating root.json with Connect server key trust (keyid: {}...)",
                                &keyid[..16]
                            ),
                            OutputLevel::Normal,
                        );
                        let root_json_content =
                            crate::utils::update_signing::generate_multi_key_root_json(
                                signer,
                                server_key_hex,
                                1,
                                3650, // 10-year expiry for Connect-managed devices
                            )?;
                        format!(
                            r#"
# Provision update authority (trust anchor for verified updates)
mkdir -p "$VAR_DIR/lib/avocado/metadata"

cat > "$VAR_DIR/lib/avocado/metadata/root.json" <<'ROOT_EOF'
{root_json_content}
ROOT_EOF

cp "$VAR_DIR/lib/avocado/metadata/root.json" "$VAR_DIR/lib/avocado/metadata/1.root.json"
echo "Provisioned update authority: metadata/root.json""#
                        )
                    }
                    None => {
                        anyhow::bail!(
                            "Signing key is configured but connect.server_key is missing.\n\
                             Level 2 requires the server key to build a multi-key root.json.\n\
                             Run 'avocado connect trust promote-root' to set up Level 2,\n\
                             or remove signing.key to use Level 0 (server-managed)."
                        );
                    }
                }
            }
            // Sideload: user's key for all roles, not a Connect project
            (Some(signer), false) => {
                let root_json_content = crate::utils::update_signing::generate_root_json(signer)?;
                format!(
                    r#"
# Provision update authority (trust anchor for verified updates)
mkdir -p "$VAR_DIR/lib/avocado/metadata"

cat > "$VAR_DIR/lib/avocado/metadata/root.json" <<'ROOT_EOF'
{root_json_content}
ROOT_EOF

cp "$VAR_DIR/lib/avocado/metadata/root.json" "$VAR_DIR/lib/avocado/metadata/1.root.json"
echo "Provisioned update authority: metadata/root.json""#
                )
            }
            // Unreachable: handled above by auto-generating a dev signing key
            (None, false) => unreachable!("dev signing key should have been auto-generated above"),
        };

        // Extension list from runtime config (used by var_files, docker priming, and subvolumes)
        let ext_list: Vec<&str> = merged_runtime
            .get("extensions")
            .and_then(|e| e.as_sequence())
            .map(|seq| seq.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // Resolve subvolumes from extensions + runtime config
        let (resolved_subvolumes, subvol_warnings) =
            crate::utils::config::resolve_subvolumes(&ext_list, parsed, &merged_runtime)?;

        // Generate subvolume-related script sections
        let subvol_warnings_section = if subvol_warnings.is_empty() {
            String::new()
        } else {
            subvol_warnings
                .iter()
                .map(|w| format!("echo \"WARNING: {w}\""))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Generate mkdir commands for subvolume paths
        let subvol_mkdir_section = resolved_subvolumes
            .iter()
            .map(|s| format!("mkdir -p \"$VAR_DIR/{}\"", s.path))
            .collect::<Vec<_>>()
            .join("\n");

        // Generate mkfs.btrfs --subvol flags
        // Always create as rw at mkfs time -- read-only subvolumes need properties
        // set first (compression, etc.), then flipped to ro in the post-creation step.
        let has_ro_subvolumes = resolved_subvolumes.iter().any(|s| !s.writable);
        let subvol_flags: Vec<String> = resolved_subvolumes
            .iter()
            .map(|s| format!("    --subvol rw:{}", s.path))
            .collect();

        let mkfs_flags = subvol_flags.join(" \\\n");

        // Determine if we can use mkfs.btrfs --compress for a global default
        // (applies compression at image creation time to all packed files)
        let global_compress_flag = {
            // Use the runtime-level var.compression as a global --compress flag
            let var_compression = merged_runtime
                .get("var")
                .and_then(|v| v.get("compression"))
                .and_then(|v| v.as_str());
            match var_compression {
                Some(c) if c != "no" => format!("    --compress {c}"),
                _ => String::new(),
            }
        };

        // Generate post-creation section for nodatacow, per-subvolume compression, and quotas.
        // These require loop-mounting the btrfs image:
        //   nodatacow: chattr +C (requires e2fsprogs in SDK)
        //   compression: btrfs property set
        //   quotas: btrfs quota enable + btrfs qgroup limit
        let needs_post_creation = has_ro_subvolumes
            || resolved_subvolumes
                .iter()
                .any(|s| s.nodatacow || s.quota.is_some() || s.compression.is_some());

        let post_creation_section = if needs_post_creation {
            let mut commands = vec![
                "# Post-creation: apply per-subvolume properties via loop mount".to_string(),
                "echo \"Applying subvolume properties...\"".to_string(),
                "LOOP_DEV=$(losetup --find --show \"$VAR_IMAGE\")".to_string(),
                "mkdir -p /tmp/btrfs-var-setup".to_string(),
                "mount -t btrfs \"$LOOP_DEV\" /tmp/btrfs-var-setup".to_string(),
            ];

            // nodatacow via chattr +C (requires e2fsprogs in SDK)
            for s in &resolved_subvolumes {
                if s.nodatacow {
                    commands.push(format!("chattr +C /tmp/btrfs-var-setup/{}", s.path));
                    commands.push(format!("echo \"  {}: nodatacow\"", s.path));
                }
            }

            // Per-subvolume compression properties
            // Skip subvolumes with nodatacow -- NOCOW and compression are mutually
            // exclusive on btrfs (COW is required for transparent compression).
            for s in &resolved_subvolumes {
                if s.nodatacow {
                    continue;
                }
                if let Some(ref comp) = s.compression {
                    if comp != "no" {
                        commands.push(format!(
                            "btrfs property set /tmp/btrfs-var-setup/{} compression {}",
                            s.path, comp
                        ));
                        commands.push(format!("echo \"  {}: compression={}\"", s.path, comp));
                    }
                }
            }

            // Quotas
            let has_quotas = resolved_subvolumes.iter().any(|s| s.quota.is_some());
            if has_quotas {
                commands.push("btrfs quota enable /tmp/btrfs-var-setup".to_string());
                for s in &resolved_subvolumes {
                    if let Some(ref quota) = s.quota {
                        if quota != "none" {
                            commands.push(format!(
                                "btrfs qgroup limit {} /tmp/btrfs-var-setup/{}",
                                quota, s.path
                            ));
                            commands.push(format!("echo \"  {}: quota={}\"", s.path, quota));
                        }
                    }
                }
            }

            // Flip read-only subvolumes to ro (created as rw so properties could be set first)
            for s in &resolved_subvolumes {
                if !s.writable {
                    commands.push(format!(
                        "btrfs property set /tmp/btrfs-var-setup/{} ro true",
                        s.path
                    ));
                    commands.push(format!("echo \"  {}: read-only\"", s.path));
                }
            }

            commands.push("umount /tmp/btrfs-var-setup".to_string());
            commands.push("losetup -d \"$LOOP_DEV\"".to_string());
            commands.join("\n")
        } else {
            String::new()
        };

        // Build var_files section: apply extension var_files to var staging in reverse order
        // (last in extensions list applied first = lowest priority, first applied last = wins conflicts)
        let var_files_section = {
            let mut var_files_commands = Vec::new();

            // Process in reverse order so first-listed extension wins conflicts
            for ext_name in ext_list.iter().rev() {
                let var_files = parsed
                    .get("extensions")
                    .and_then(|e| e.get(*ext_name))
                    .map(crate::utils::config::get_ext_var_files)
                    .unwrap_or_default();

                if !var_files.is_empty() {
                    for pattern in &var_files {
                        // Strip trailing glob suffixes and leading "var/" to get the dest path under $VAR_DIR
                        let clean_pattern = pattern.trim_end_matches("/**").trim_end_matches("/*");
                        // The pattern is relative to the sysroot (e.g., "var/lib/docker")
                        // $VAR_DIR maps to /var on the target, so strip the leading "var/" for dest
                        let dest = clean_pattern.strip_prefix("var/").unwrap_or(clean_pattern);
                        var_files_commands.push(format!(
                            r#"
if [ -d "$AVOCADO_EXT_SYSROOTS/{ext_name}/{clean_pattern}" ]; then
    echo "  Applying var files from extension '{ext_name}': {clean_pattern}/"
    mkdir -p "$VAR_DIR/{dest}"
    rsync -a "$AVOCADO_EXT_SYSROOTS/{ext_name}/{clean_pattern}/" "$VAR_DIR/{dest}/"
elif [ -f "$AVOCADO_EXT_SYSROOTS/{ext_name}/{clean_pattern}" ]; then
    echo "  Applying var file from extension '{ext_name}': {clean_pattern}"
    mkdir -p "$(dirname "$VAR_DIR/{dest}")"
    cp -f "$AVOCADO_EXT_SYSROOTS/{ext_name}/{clean_pattern}" "$VAR_DIR/{dest}"
fi"#
                        ));
                    }
                }
            }

            if var_files_commands.is_empty() {
                "# No extension var_files to apply".to_string()
            } else {
                format!(
                    "echo \"Applying extension var files to var partition...\"\n{}",
                    var_files_commands.join("\n")
                )
            }
        };

        // Build runtime-level var_files section
        let runtime_var_files_section = {
            let runtime_var_files = crate::utils::config::get_runtime_var_files(&merged_runtime);
            if runtime_var_files.is_empty() {
                "# No runtime var_files to apply".to_string()
            } else {
                let commands: Vec<String> = runtime_var_files
                    .iter()
                    .map(|mapping| {
                        format!(
                            r#"
if [ -e "/opt/src/{source}" ]; then
    mkdir -p "$VAR_DIR/{dest}"
    rsync -a "/opt/src/{source}" "$VAR_DIR/{dest}"
    echo "  Copied runtime var_files: {source} -> {dest}"
else
    echo "WARNING: runtime var_files source not found: /opt/src/{source}"
fi"#,
                            source = mapping.source,
                            dest = mapping.dest
                        )
                    })
                    .collect();
                format!(
                    "echo \"Applying runtime var files to var partition...\"\n{}",
                    commands.join("\n")
                )
            }
        };

        // Build Docker image priming section
        // Collect docker_images from all extensions in the runtime
        let docker_section = {
            let docker_images: Vec<crate::utils::config::DockerImageRef> = ext_list
                .iter()
                .flat_map(|ext_name| {
                    parsed
                        .get("extensions")
                        .and_then(|e| e.get(*ext_name))
                        .map(crate::utils::config::get_docker_images)
                        .unwrap_or_default()
                })
                .collect();
            if docker_images.is_empty() {
                "# No Docker images to prime".to_string()
            } else {
                let pull_commands: Vec<String> = docker_images
                    .iter()
                    .map(|img| {
                        format!(
                            r#"docker --host unix:///tmp/avocado-dockerd.sock pull --platform "linux/$DOCKER_ARCH" "{image}:{tag}"
echo "  Primed: {image}:{tag}""#,
                            image = img.image,
                            tag = img.tag
                        )
                    })
                    .collect();

                format!(
                    r#"# Prime Docker image cache on var partition
echo "Priming Docker images on var partition..."
mkdir -p "$VAR_DIR/lib/docker"

# Verify dockerd is available
if ! command -v dockerd >/dev/null 2>&1; then
    echo "ERROR: dockerd not found in SDK container. Docker image priming requires dockerd, containerd, runc, and docker CLI."
    exit 1
fi

# Map target arch to Docker platform
# Use OECORE_TARGET_ARCH (CPU arch like x86_64/aarch64) from SDK environment
DOCKER_TARGET_ARCH="${{OECORE_TARGET_ARCH:-$TARGET_ARCH}}"
case "$DOCKER_TARGET_ARCH" in
    aarch64) DOCKER_ARCH="arm64" ;;
    x86_64) DOCKER_ARCH="amd64" ;;
    *) echo "WARNING: Unknown target architecture '$DOCKER_TARGET_ARCH' for Docker platform mapping, defaulting to amd64"; DOCKER_ARCH="amd64" ;;
esac

# The SDK container may have the host's /sys bind-mounted (-v /sys:/sys),
# and --privileged gives write access even without that flag.
# Make the /sys/fs/cgroup mount private so the inner dockerd's mount
# events do not propagate to the host.  A bind+private mount preserves
# the existing cgroup controllers (required by dockerd) while isolating
# mount propagation.
_AVOCADO_CGROUP_PRIVATE=0
if mount --bind /sys/fs/cgroup /sys/fs/cgroup 2>/dev/null \
   && mount --make-private /sys/fs/cgroup 2>/dev/null; then
    _AVOCADO_CGROUP_PRIVATE=1
else
    echo "WARNING: Could not make /sys/fs/cgroup private — inner dockerd may leave stale cgroup entries on the host."
fi

# When the SDK container uses --network=host the inner dockerd shares the
# host network namespace and may delete the host's docker0 bridge on exit.
# Save its address now so we can restore it if needed.
_DOCKER0_ADDR=""
if ip link show docker0 >/dev/null 2>&1; then
    _DOCKER0_ADDR=$(ip -4 addr show docker0 2>/dev/null | awk '/inet /{{print $2}}' | head -1)
fi

_avocado_docker_cleanup() {{
    kill $DOCKERD_PID 2>/dev/null || true
    wait $DOCKERD_PID 2>/dev/null || true
    rm -f /tmp/avocado-dockerd.sock /tmp/avocado-dockerd.pid /tmp/avocado-dockerd.log
    [ "$_AVOCADO_CGROUP_PRIVATE" = "1" ] && umount /sys/fs/cgroup 2>/dev/null || true
    # Restore docker0 if the inner dockerd removed it from the host network namespace
    if [ -n "$_DOCKER0_ADDR" ] && ! ip link show docker0 >/dev/null 2>&1; then
        echo "NOTE: inner dockerd removed host docker0 — restoring."
        ip link add name docker0 type bridge 2>/dev/null || true
        ip addr add "$_DOCKER0_ADDR" dev docker0 2>/dev/null || true
        ip link set docker0 up 2>/dev/null || true
    fi
}}
trap _avocado_docker_cleanup EXIT

# Start temporary dockerd with data-root pointing at var staging.
# cgroupdriver=cgroupfs avoids systemd-cgroup interaction inside the container.
dockerd --data-root "$VAR_DIR/lib/docker" \
    --host unix:///tmp/avocado-dockerd.sock \
    --exec-opt native.cgroupdriver=cgroupfs \
    --iptables=false --ip-masq=false \
    --bridge=none \
    --exec-root /tmp/avocado-dockerd \
    --pidfile /tmp/avocado-dockerd.pid \
    >/tmp/avocado-dockerd.log 2>&1 &
DOCKERD_PID=$!

# Wait for dockerd to be ready
echo "Waiting for temporary dockerd..."
for i in $(seq 1 30); do
    if docker --host unix:///tmp/avocado-dockerd.sock info >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 $DOCKERD_PID 2>/dev/null; then
        echo "ERROR: dockerd exited unexpectedly. Check /tmp/avocado-dockerd.log"
        cat /tmp/avocado-dockerd.log
        exit 1
    fi
    sleep 1
done

if ! docker --host unix:///tmp/avocado-dockerd.sock info >/dev/null 2>&1; then
    echo "ERROR: dockerd failed to start within 30 seconds"
    cat /tmp/avocado-dockerd.log
    exit 1
fi

echo "Pulling Docker images for platform linux/$DOCKER_ARCH..."
{pull_commands}

trap - EXIT
_avocado_docker_cleanup
echo "Docker image priming complete.""#,
                    pull_commands = pull_commands.join("\n")
                )
            }
        };

        let rootfs_build_section =
            generate_rootfs_build_script(NAMESPACE_UUID, &config.get_rootfs_filesystem());

        let initramfs_build_section =
            generate_initramfs_build_script(NAMESPACE_UUID, &config.get_initramfs_filesystem());

        let script = format!(
            r#"
# Set common variables
RUNTIME_NAME="{runtime_name}"
TARGET_ARCH="{target_arch}"
RUNTIME_VERSION="{runtime_version}"

VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging
mkdir -p "$VAR_DIR/lib/avocado/images"
mkdir -p "$VAR_DIR/lib/avocado/runtimes"
{subvol_mkdir_section}
{subvol_warnings_section}

OUTPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
mkdir -p $OUTPUT_DIR

# Create runtime-specific extensions directory (staging area for image ID computation)
RUNTIME_EXT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/extensions"
mkdir -p "$RUNTIME_EXT_DIR"

# Clean up stale extensions to ensure fresh copies
echo "Cleaning up stale extensions..."
rm -f "$RUNTIME_EXT_DIR"/*.raw "$RUNTIME_EXT_DIR"/*.kab 2>/dev/null || true

# Copy required extension images from global output/extensions to runtime-specific location
echo "Copying required extension images to runtime-specific directory..."
{copy_section}

# Build rootfs and initramfs images from package sysroots
{rootfs_build_section}
{initramfs_build_section}

# Assemble var partition content and build var image
{var_files_section}
{runtime_var_files_section}
{manifest_section}
{update_authority_section}
{docker_section}

VAR_IMAGE="$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"
VAR_INPUT_SIZE=$(du -sb "$VAR_DIR" 2>/dev/null | awk '{{print $1}}')
VAR_INPUT_MB=$(( VAR_INPUT_SIZE / 1048576 ))
echo "Building var image (${{VAR_INPUT_MB}}MB source)..."

# Background progress reporter — prints size and estimated % every 5s
(
    while [ ! -f "$VAR_IMAGE" ]; do sleep 1; done
    while kill -0 $$ 2>/dev/null; do
        CUR=$(stat -c%s "$VAR_IMAGE" 2>/dev/null || echo 0)
        CUR_MB=$(( CUR / 1048576 ))
        if [ "$VAR_INPUT_SIZE" -gt 0 ] 2>/dev/null; then
            PCT=$(( CUR * 100 / VAR_INPUT_SIZE ))
            [ "$PCT" -gt 99 ] && PCT=99
            printf "\r  var image: %dMB written (~%d%%)" "$CUR_MB" "$PCT"
        else
            printf "\r  var image: %dMB written" "$CUR_MB"
        fi
        sleep 5
    done
) &
_PROGRESS_PID=$!

mkfs.btrfs -r "$VAR_DIR" \
{mkfs_flags} \
{global_compress_flag}    -f "$VAR_IMAGE"

kill $_PROGRESS_PID 2>/dev/null; wait $_PROGRESS_PID 2>/dev/null || true

{post_creation_section}
FINAL_SIZE=$(stat -c%s "$VAR_IMAGE" 2>/dev/null || echo 0)
FINAL_MB=$(( FINAL_SIZE / 1048576 ))
echo ""
echo "Built var image: ${{FINAL_MB}}MB"

# Build OS bundle (.aos) — needs rootfs + initramfs + kernel + var (all built above)
STONE_MANIFEST="${{AVOCADO_STONE_MANIFEST:-$AVOCADO_SDK_PREFIX/stone/stone-$TARGET_ARCH.json}}"
STONE_INPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
STONE_BUILD_DIR="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/stone"
# Clean previous stone build artifacts to prevent stale image reuse
rm -rf "$STONE_BUILD_DIR"
STONE_AOS_OUTPUT="$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/os-bundle.aos"
export STONE_AOS_OUTPUT

# Build include path flags from AVOCADO_STONE_INCLUDE_PATHS
STONE_INCLUDE_FLAGS=""
if [ -n "${{AVOCADO_STONE_INCLUDE_PATHS:-}}" ]; then
    for path in $AVOCADO_STONE_INCLUDE_PATHS; do
        STONE_INCLUDE_FLAGS="$STONE_INCLUDE_FLAGS -i $path"
    done
fi
STONE_INCLUDE_FLAGS="$STONE_INCLUDE_FLAGS -i $STONE_INPUT_DIR"

echo -e "\033[94m[INFO]\033[0m Running stone bundle."
echo -e "  Manifest:  $STONE_MANIFEST"
echo -e "  Output:    $STONE_AOS_OUTPUT"
echo -e "  Build dir: $STONE_BUILD_DIR"

STONE_INITRD_FLAG=""
INITRD_OS_RELEASE="$AVOCADO_PREFIX/initramfs/usr/lib/os-release-initrd"
if [ -f "$INITRD_OS_RELEASE" ]; then
    STONE_INITRD_FLAG="--os-release-initrd $INITRD_OS_RELEASE"
fi

stone bundle \
    --os-release "$AVOCADO_PREFIX/rootfs/usr/lib/os-release" \
    $STONE_INITRD_FLAG \
    -m "$STONE_MANIFEST" \
    $STONE_INCLUDE_FLAGS \
    -o "$STONE_AOS_OUTPUT" \
    --build-dir "$STONE_BUILD_DIR"

# Patch manifest in var-staging to add os_bundle reference (for connect upload)
# The btrfs image for provisioning doesn't need os_bundle — initial flash doesn't OTA.
# Connect upload reads from var-staging directly, so it sees this update.
python3 << 'PYEOF'
import json, hashlib, uuid, os, shutil

aos_path = os.environ.get("STONE_AOS_OUTPUT", "")
if not (aos_path and os.path.isfile(aos_path)):
    print("No .aos file found, skipping os_bundle manifest patch.")
    exit(0)

namespace = uuid.UUID(os.environ["AVOCADO_NS_UUID"])
images_dir = os.environ["AVOCADO_IMAGES_DIR"]
manifest_path = os.environ["AVOCADO_MANIFEST_PATH"]

with open(aos_path, "rb") as f:
    aos_sha256 = hashlib.sha256(f.read()).hexdigest()
aos_image_id = str(uuid.uuid5(namespace, aos_sha256))
dest = os.path.join(images_dir, aos_image_id + ".raw")
shutil.copy2(aos_path, dest)
print("  OS bundle: os-bundle.aos -> " + aos_image_id + ".raw")

with open(manifest_path, "r") as f:
    manifest = json.load(f)
os_build_id = None
os_release_path = os.path.join(os.environ.get("AVOCADO_PREFIX", ""), "rootfs/usr/lib/os-release")
if os.path.isfile(os_release_path):
    with open(os_release_path) as f:
        for line in f:
            if line.startswith("AVOCADO_OS_BUILD_ID="):
                os_build_id = line.strip().split("=", 1)[1]
                break

initramfs_build_id = os.environ.get("AVOCADO_INITRAMFS_BUILD_ID")

os_bundle = dict(image_id=aos_image_id, sha256=aos_sha256)
if os_build_id:
    os_bundle["os_build_id"] = os_build_id
if initramfs_build_id:
    os_bundle["initramfs_build_id"] = initramfs_build_id
manifest["os_bundle"] = os_bundle
with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
print("Patched manifest with os_bundle reference.")

# Clean up stale os_bundle images
current_image_files = set()
for ext in manifest.get("extensions", []):
    current_image_files.add(ext["image_id"] + ".raw")
current_image_files.add(aos_image_id + ".raw")
for fname in os.listdir(images_dir):
    if fname.endswith(".raw") and fname not in current_image_files:
        os.remove(os.path.join(images_dir, fname))
        print("  Removed stale image: " + fname)
PYEOF
"#,
            runtime_name = self.runtime_name,
            target_arch = target_arch,
            runtime_version = runtime_version,
            copy_section = copy_section,
            rootfs_build_section = rootfs_build_section,
            initramfs_build_section = initramfs_build_section,
            subvol_mkdir_section = subvol_mkdir_section,
            subvol_warnings_section = subvol_warnings_section,
            mkfs_flags = mkfs_flags,
            global_compress_flag = if global_compress_flag.is_empty() {
                "".to_string()
            } else {
                format!("{global_compress_flag} \\\n")
            },
            post_creation_section = post_creation_section,
            var_files_section = var_files_section,
            runtime_var_files_section = runtime_var_files_section,
            manifest_section = manifest_section,
            update_authority_section = update_authority_section,
            docker_section = docker_section,
        );

        Ok(script)
    }

    /// Recursively find all extension dependencies, including nested external extensions
    fn find_all_extension_dependencies(
        &self,
        config: &crate::utils::config::Config,
        direct_extensions: &HashSet<String>,
        target_arch: &str,
    ) -> Result<HashSet<String>> {
        let mut all_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Process each direct extension dependency
        for ext_name in direct_extensions {
            self.collect_extension_dependencies(
                config,
                ext_name,
                &mut all_extensions,
                &mut visited,
                target_arch,
            )?;
        }

        Ok(all_extensions)
    }

    /// Recursively collect all dependencies for a single extension
    fn collect_extension_dependencies(
        &self,
        _config: &crate::utils::config::Config,
        ext_name: &str,
        all_extensions: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        _target_arch: &str,
    ) -> Result<()> {
        // Avoid infinite loops
        if visited.contains(ext_name) {
            return Ok(());
        }
        visited.insert(ext_name.to_string());

        // Add this extension to the result set
        all_extensions.insert(ext_name.to_string());

        // Load the main config to check for local extensions
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Check if this is a local extension defined in the ext section
        // Extension source configuration (repo, git, path) is now in the ext section
        if let Some(ext_config) = parsed
            .get("extensions")
            .and_then(|e| e.as_mapping())
            .and_then(|table| table.get(ext_name))
        {
            // This is a local extension - check if it has an extensions array for nested deps
            if let Some(nested_extensions) =
                ext_config.get("extensions").and_then(|e| e.as_sequence())
            {
                for nested_ext in nested_extensions {
                    if let Some(nested_ext_name) = nested_ext.as_str() {
                        self.collect_extension_dependencies(
                            _config,
                            nested_ext_name,
                            all_extensions,
                            visited,
                            _target_arch,
                        )?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Collect extensions required by this runtime with their resolved versions.
    ///
    /// Returns a list of versioned extension names in the format "ext_name-version"
    /// (e.g., "my-ext-1.0.0"). This ensures AVOCADO_EXT_LIST and the build script
    /// use exact versions from the configuration, not wildcards.
    #[allow(clippy::too_many_arguments)]
    async fn collect_runtime_extensions(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        runtime_name: &str,
        target_arch: &str,
        config_path: &str,
        container_image: &str,
        container_args: Option<Vec<String>>,
    ) -> Result<Vec<String>> {
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target_arch, config_path)?;

        let mut extensions = Vec::new();

        // Read extensions from the new `extensions` array format
        let ext_list = merged_runtime
            .as_ref()
            .and_then(|value| value.get("extensions").and_then(|e| e.as_sequence()))
            .or_else(|| {
                parsed
                    .get("runtimes")
                    .and_then(|r| r.get(runtime_name))
                    .and_then(|runtime_value| runtime_value.get("extensions"))
                    .and_then(|e| e.as_sequence())
            });

        if let Some(ext_seq) = ext_list {
            for ext in ext_seq {
                if let Some(ext_name) = ext.as_str() {
                    let version = self
                        .resolve_extension_version(
                            parsed,
                            config,
                            config_path,
                            ext_name,
                            container_image,
                            target_arch,
                            container_args.clone(),
                        )
                        .await?;
                    extensions.push(format!("{ext_name}-{version}"));
                }
            }
        }

        // Deduplicate while preserving declaration order from the config.
        // The order in the extensions array determines merge priority in avocadoctl.
        let mut seen = std::collections::HashSet::new();
        extensions.retain(|ext| seen.insert(ext.clone()));

        Ok(extensions)
    }

    /// Resolve the version for an extension.
    ///
    /// Priority order:
    /// 1. Version from local `[ext]` section
    /// 2. Query RPM database for installed version (for repo-sourced extensions)
    #[allow(clippy::too_many_arguments)]
    async fn resolve_extension_version(
        &self,
        parsed: &serde_yaml::Value,
        _config: &crate::utils::config::Config,
        _config_path: &str,
        ext_name: &str,
        container_image: &str,
        target_arch: &str,
        container_args: Option<Vec<String>>,
    ) -> Result<String> {
        // Try to get version from local [ext] section
        if let Some(version) = parsed
            .get("extensions")
            .and_then(|ext_section| ext_section.as_mapping())
            .and_then(|ext_table| ext_table.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
        {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If version is "*", fall through to query RPM
        }

        // No version found in config - this is likely a package repository extension
        // Query RPM database for the installed version
        self.query_rpm_version(ext_name, container_image, target_arch, container_args)
            .await
    }

    /// Query RPM database for the actual installed version of an extension.
    ///
    /// This queries the RPM database in the extension's sysroot at $AVOCADO_EXT_SYSROOTS/{ext_name}
    /// to get the actual installed version. This ensures AVOCADO_EXT_LIST contains
    /// precise version information.
    async fn query_rpm_version(
        &self,
        ext_name: &str,
        container_image: &str,
        target: &str,
        container_args: Option<Vec<String>>,
    ) -> Result<String> {
        let container_helper = SdkContainer::new();

        let version_query_script = format!(
            r#"
set -e
# Query RPM version for extension from RPM database using the same config as installation
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
rpm --root="$AVOCADO_EXT_SYSROOTS/{ext_name}" --dbpath=/var/lib/extension.d/rpm -q {ext_name} --queryformat '%{{VERSION}}'
"#
        );

        let version_query_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: version_query_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            // runs_on handled by shared context
            container_args,
            sdk_arch: self.sdk_arch.clone(),
            tui_context: self.tui_context.clone(),
            env_vars: self.runtime_env_vars(),
            ..Default::default()
        };

        match container_helper
            .run_in_container_with_output(version_query_config)
            .await
        {
            Ok(Some(actual_version)) => {
                let trimmed_version = actual_version.trim();
                if self.verbose {
                    print_info(
                        &format!(
                            "Resolved extension '{ext_name}' to version '{trimmed_version}' from RPM database"
                        ),
                        OutputLevel::Normal,
                    );
                }
                Ok(trimmed_version.to_string())
            }
            Ok(None) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
            Err(e) => Err(anyhow::anyhow!(
                "Failed to query version for extension '{ext_name}' from RPM database: {e}. \
                    Extension may not be installed yet. Run 'avocado install' first."
            )),
        }
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

/// Helper function to run a container command and capture its output,
/// using shared context if available.
async fn run_container_command_with_output(
    container_helper: &SdkContainer,
    config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
) -> Result<Option<String>> {
    if let Some(context) = runs_on_context {
        container_helper
            .run_in_container_with_output_remote(&config, context)
            .await
    } else {
        container_helper.run_in_container_with_output(config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config_file(temp_dir: &TempDir, content: &str) -> String {
        let config_path = temp_dir.path().join("avocado.yaml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_new() {
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_create_build_script() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  test-runtime:
    target: "x86_64"
    packages:
      test-dep:
        ext: test-ext
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Pass empty resolved_extensions since no extensions are defined with versions
        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions: Vec<String> = vec![];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("RUNTIME_NAME=\"test-runtime\""));
        assert!(script.contains("TARGET_ARCH=\"x86_64\""));
        assert!(script.contains("VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"));
        assert!(script.contains("stone bundle"));
        assert!(script.contains("mkfs.btrfs"));
    }

    #[test]
    fn test_create_build_script_with_extensions() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  test-runtime:
    target: "x86_64"
    extensions:
      - test-ext

extensions:
  test-ext:
    version: "1.0.0"
    types:
      - sysext
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("test-ext-1.0.0.raw"));
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(!script.contains("$VAR_DIR/lib/avocado/extensions/"));
    }

    #[test]
    fn test_create_build_script_with_extension_types() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  test-runtime:
    target: "x86_64"
    extensions:
      - test-ext

extensions:
  test-ext:
    version: "1.0.0"
    types:
      - sysext
      - confext
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(!script.contains("$VAR_DIR/lib/avocado/extensions/"));
    }

    #[test]
    fn test_create_build_script_uses_extension_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  test-runtime:
    target: "x86_64"
    extensions:
      - test-ext

extensions:
  test-ext:
    version: "1.0.0"
    types:
      - confext
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(!script.contains("$VAR_DIR/lib/avocado/extensions/"));
    }

    #[test]
    fn test_kernel_config_parsed_from_runtime() {
        let config_content = r#"
sdk:
  image: "test-image"

runtimes:
  test-runtime:
    target: "x86_64"
    kernel:
      package: kernel-image
      version: "*"
    packages:
      avocado-img-rootfs: "*"
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let runtime_val = parsed
            .get("runtimes")
            .and_then(|r| r.get("test-runtime"))
            .unwrap();

        let kernel_config =
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val, None)
                .unwrap();
        assert!(kernel_config.is_some());
        let kc = kernel_config.unwrap();
        assert_eq!(kc.package.as_deref(), Some("kernel-image"));
        assert_eq!(kc.version.as_deref(), Some("*"));
        assert!(kc.compile.is_none());
    }

    #[test]
    fn test_kernel_config_compile_mode_from_runtime() {
        let config_content = r#"
sdk:
  image: "test-image"
  compile:
    kernel-build:
      compile: kernel-compile.sh

runtimes:
  test-runtime:
    target: "x86_64"
    kernel:
      compile: kernel-build
      install: kernel-install.sh
    packages:
      avocado-img-rootfs: "*"
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let runtime_val = parsed
            .get("runtimes")
            .and_then(|r| r.get("test-runtime"))
            .unwrap();

        let kernel_config =
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val, None)
                .unwrap();
        assert!(kernel_config.is_some());
        let kc = kernel_config.unwrap();
        assert!(kc.package.is_none());
        assert_eq!(kc.compile.as_deref(), Some("kernel-build"));
        assert_eq!(kc.install.as_deref(), Some("kernel-install.sh"));
    }

    #[test]
    fn test_kernel_config_absent_from_runtime() {
        let config_content = r#"
sdk:
  image: "test-image"

runtimes:
  test-runtime:
    target: "x86_64"
    packages:
      avocado-img-rootfs: "*"
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let runtime_val = parsed
            .get("runtimes")
            .and_then(|r| r.get("test-runtime"))
            .unwrap();

        let kernel_config =
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val, None)
                .unwrap();
        assert!(kernel_config.is_none());
    }

    #[test]
    fn test_create_build_script_generates_manifest() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

distro:
  version: "0.1.0"

runtimes:
  test-runtime:
    target: "x86_64"
    extensions:
      - test-ext

extensions:
  test-ext:
    version: "1.0.0"
    types:
      - sysext
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        // Manifest should be generated dynamically via Python
        assert!(script.contains("manifest.json"));
        assert!(script.contains("manifest_version=2"));
        assert!(script.contains("AVOCADO_RUNTIME_NAME=\"test-runtime\""));
        assert!(script.contains("AVOCADO_EXT_PAIRS=\"test-ext:1.0.0:raw\""));
        assert!(script.contains("AVOCADO_NS_UUID="));

        // Active symlink should be created
        assert!(script.contains("ln -sfn \"runtimes/"));
        assert!(script.contains("$VAR_DIR/lib/avocado/active"));

        // Spot hash cache should be generated
        assert!(script.contains("AVOCADO_SPOT_HASHES_PATH"));
        assert!(script.contains("spot_hashes.json"));
        assert!(script.contains("compute_spot_hash"));

        assert!(script.contains("--subvol rw:lib/avocado "));
    }

    #[test]
    fn test_create_build_script_manifest_no_extensions() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

distro:
  version: "2.0.0"

runtimes:
  empty-runtime:
    target: "x86_64"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "empty-runtime".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let resolved_extensions: Vec<String> = vec![];
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("AVOCADO_RUNTIME_NAME=\"empty-runtime\""));
        assert!(script.contains("AVOCADO_RUNTIME_VERSION=\"$RUNTIME_VERSION\""));
        assert!(script.contains("AVOCADO_EXT_PAIRS=\"\""));
        assert!(script.contains("ln -sfn \"runtimes/"));
    }

    #[test]
    fn test_create_build_script_manifest_has_uuid() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

connect:
  org: test

runtimes:
  dev:
    target: "x86_64"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "dev".to_string(),
            config_path,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let config = Config::load(&cmd.config_path).unwrap();
        let script = cmd
            .create_build_script(&config, &parsed, "x86_64", &[])
            .unwrap();

        // The manifest section should set BUILD_ID with a UUID
        assert!(script.contains("BUILD_ID=\""));
        // The manifest section should set BUILT_AT with a timestamp
        assert!(script.contains("BUILT_AT=\""));
    }
}
