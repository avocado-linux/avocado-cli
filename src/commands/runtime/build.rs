use crate::commands::sdk::SdkCompileCommand;
use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    stamps::{
        compute_runtime_input_hash, generate_batch_read_stamps_script, generate_write_stamp_script,
        resolve_required_stamps_for_runtime_build, validate_stamps_batch, Stamp, StampOutputs,
    },
    target::resolve_target_required,
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
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            // Validate all stamps from batch output
            let validation =
                validate_stamps_batch(&required, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error =
                    validation.into_error(&format!("Cannot build runtime '{}'", self.runtime_name));
                return Err(error.into());
            }
        }

        print_info(
            &format!("Building runtime images for '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Check for kernel configuration in the merged runtime config
        let merged_runtime =
            config.get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?;
        let kernel_config = merged_runtime
            .as_ref()
            .and_then(|v| Config::get_kernel_config_from_runtime(v).ok().flatten());

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
                    r#"mkdir -p "{runtime_build_dir}" && if [ -f '{install_script}' ]; then echo 'Running kernel install script: {install_script}'; export AVOCADO_RUNTIME_BUILD_DIR="{runtime_build_dir}"; bash '{install_script}'; else echo 'Kernel install script {install_script} not found.'; ls -la; exit 1; fi"#
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

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true, // need environment for build
            interactive: false,       // build script runs non-interactively
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            env_vars,
            // runs_on handled by shared context
            sdk_arch: self.sdk_arch.clone(),
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
            let inputs = compute_runtime_input_hash(&merged_runtime, &self.runtime_name)?;
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
                ..Default::default()
            };

            run_container_command(container_helper, run_config, runs_on_context).await?;

            if self.verbose {
                print_info(
                    &format!("Wrote build stamp for runtime '{}'.", self.runtime_name),
                    OutputLevel::Normal,
                );
            }
        }

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

        // Build extension symlink commands from config
        let mut symlink_commands = Vec::new();
        let mut processed_extensions = HashSet::new();

        // Process local extensions defined in [ext.*] sections
        if let Some(ext_config) = parsed.get("extensions").and_then(|v| v.as_mapping()) {
            for (ext_name_val, ext_data) in ext_config {
                if let Some(ext_name) = ext_name_val.as_str() {
                    // Only process extensions that are required by this runtime
                    if all_required_extensions.contains(ext_name) {
                        // Get version from extension config
                        let ext_version = ext_data
                            .get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("0.1.0");

                        // Add copy command for this extension
                        copy_commands.push(format!(
                            r#"
# Copy {ext_name}-{ext_version}.raw from output/extensions to runtime-specific directory
if [ -f "$AVOCADO_PREFIX/output/extensions/{ext_name}-{ext_version}.raw" ]; then
    cp -f "$AVOCADO_PREFIX/output/extensions/{ext_name}-{ext_version}.raw" "$RUNTIME_EXT_DIR/{ext_name}-{ext_version}.raw"
    echo "  Copied: {ext_name}-{ext_version}.raw"
fi"#
                        ));

                        symlink_commands.push(format!(
                            r#"
# Link from runtime-specific extensions directory
RUNTIME_EXT=$RUNTIME_EXT_DIR/{ext_name}-{ext_version}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{ext_name}-{ext_version}.raw

if [ -f "$RUNTIME_EXT" ]; then
    ln -f $RUNTIME_EXT $RUNTIMES_EXT
else
    echo "Missing image for extension {ext_name}-{ext_version}."
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
                // Check if we have a resolved version for this extension
                if let Some(versioned_name) = ext_version_map.get(ext_name) {
                    // Use the exact versioned name from resolved_extensions
                    copy_commands.push(format!(
                        r#"
# Copy external/versioned extension {versioned_name}.raw from output/extensions to runtime-specific directory
if [ -f "$AVOCADO_PREFIX/output/extensions/{versioned_name}.raw" ]; then
    cp -f "$AVOCADO_PREFIX/output/extensions/{versioned_name}.raw" "$RUNTIME_EXT_DIR/{versioned_name}.raw"
    echo "  Copied: {versioned_name}.raw"
else
    echo "ERROR: Extension image not found: $AVOCADO_PREFIX/output/extensions/{versioned_name}.raw"
    exit 1
fi"#
                    ));

                    symlink_commands.push(format!(
                        r#"
# Link external/versioned extension from runtime-specific directory
RUNTIME_EXT=$RUNTIME_EXT_DIR/{versioned_name}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{versioned_name}.raw

if [ -f "$RUNTIME_EXT" ]; then
    ln -f "$RUNTIME_EXT" "$RUNTIMES_EXT"
else
    echo "Missing image for extension {versioned_name}."
fi"#
                    ));
                } else {
                    // Fallback: use wildcard (should not happen if collect_runtime_extensions works correctly)
                    copy_commands.push(format!(
                        r#"
# Copy external extension {ext_name} (version not resolved, using wildcard)
EXT_FILE=$(ls "$AVOCADO_PREFIX/output/extensions/{ext_name}"-*.raw 2>/dev/null | head -n 1)
if [ -n "$EXT_FILE" ]; then
    EXT_BASENAME=$(basename "$EXT_FILE")
    cp -f "$EXT_FILE" "$RUNTIME_EXT_DIR/$EXT_BASENAME"
    echo "  Copied: $EXT_BASENAME"
fi"#
                    ));

                    symlink_commands.push(format!(
                        r#"
# Find external extension {ext_name} with any version from runtime-specific directory
RUNTIME_EXT=$(ls $RUNTIME_EXT_DIR/{ext_name}-*.raw 2>/dev/null | head -n 1)
if [ -n "$RUNTIME_EXT" ]; then
    EXT_FILENAME=$(basename "$RUNTIME_EXT")
    RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/$EXT_FILENAME
    ln -f "$RUNTIME_EXT" "$RUNTIMES_EXT"
else
    echo "Missing image for external extension {ext_name}."
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

        let symlink_section = if symlink_commands.is_empty() {
            "# No extensions configured for symlinking".to_string()
        } else {
            symlink_commands.join("\n")
        };

        // Generate build ID and timestamp for the manifest
        let build_id = uuid::Uuid::new_v4().to_string();
        let built_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let distro_version = config
            .get_distro_version()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        // Build "name:version" pairs for dynamic manifest generation.
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
                format!("{name}:{version}")
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
DISTRO_VERSION="{distro_version}"
MANIFEST_DIR="$VAR_DIR/lib/avocado/runtimes/$BUILD_ID"
mkdir -p "$MANIFEST_DIR"

export AVOCADO_NS_UUID="{namespace_uuid}"
export AVOCADO_RT_EXT_DIR="$RUNTIME_EXT_DIR"
export AVOCADO_IMAGES_DIR="$IMAGES_DIR"
export AVOCADO_MANIFEST_PATH="$MANIFEST_DIR/manifest.json"
export AVOCADO_BUILD_ID="$BUILD_ID"
export AVOCADO_BUILT_AT="$BUILT_AT"
export AVOCADO_RUNTIME_NAME="{runtime_name}"
export AVOCADO_DISTRO_VERSION="$DISTRO_VERSION"
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
distro_version = os.environ["AVOCADO_DISTRO_VERSION"]
ext_pairs_str = os.environ.get("AVOCADO_EXT_PAIRS", "")

ext_pairs = ext_pairs_str.split() if ext_pairs_str else []

extensions = []
for pair in ext_pairs:
    name, version = pair.split(":", 1)
    raw_file = os.path.join(runtime_ext_dir, name + "-" + version + ".raw")
    if not os.path.isfile(raw_file):
        print("WARNING: Extension image not found: " + raw_file)
        continue
    with open(raw_file, "rb") as f:
        sha256 = hashlib.sha256(f.read()).hexdigest()
    image_id = str(uuid.uuid5(namespace, sha256))
    dest = os.path.join(images_dir, image_id + ".raw")
    shutil.copy2(raw_file, dest)
    print("  Image: " + name + "-" + version + ".raw -> " + image_id + ".raw")
    extensions.append(dict(name=name, version=version, image_id=image_id))

manifest = dict(
    manifest_version=2,
    id=build_id,
    built_at=built_at,
    runtime=dict(name=runtime_name, version=distro_version),
    extensions=extensions,
)

with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
print("Created runtime manifest with " + str(len(extensions)) + " extension(s)")
PYEOF

ln -sfn "runtimes/$BUILD_ID" "$VAR_DIR/lib/avocado/active"
echo "Created runtime manifest: runtimes/$BUILD_ID/manifest.json"
echo "Set active runtime -> runtimes/$BUILD_ID""#,
            runtime_name = self.runtime_name,
        );

        // Generate update authority (root.json) for verified updates
        let signing_key_name = config.get_runtime_signing_key_name(&self.runtime_name);
        let project_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let (sk, pk) = crate::utils::update_signing::resolve_signing_key(
            signing_key_name.as_deref(),
            project_dir,
        )?;
        let root_json_content = crate::utils::update_signing::generate_root_json(&sk, &pk)?;

        let update_authority_section = format!(
            r#"
# Provision update authority (trust anchor for verified updates)
mkdir -p "$VAR_DIR/lib/avocado/metadata"

cat > "$VAR_DIR/lib/avocado/metadata/root.json" <<'ROOT_EOF'
{root_json_content}
ROOT_EOF

cp "$VAR_DIR/lib/avocado/metadata/root.json" "$VAR_DIR/lib/avocado/metadata/1.root.json"
echo "Provisioned update authority: metadata/root.json""#
        );

        let script = format!(
            r#"
# Set common variables
RUNTIME_NAME="{}"
TARGET_ARCH="{}"

# Read OS VERSION_ID from rootfs
if [ -f "$AVOCADO_PREFIX/rootfs/etc/os-release" ]; then
    # Source the os-release file and extract VERSION_ID
    . "$AVOCADO_PREFIX/rootfs/etc/os-release"
    if [ -z "$VERSION_ID" ]; then
        echo "Warning: VERSION_ID not found in os-release, using 'unknown'"
        VERSION_ID="unknown"
    fi
    echo "Using OS VERSION_ID: $VERSION_ID"
else
    echo "Warning: /etc/os-release not found in rootfs, using VERSION_ID='unknown'"
    VERSION_ID="unknown"
fi

VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging
mkdir -p "$VAR_DIR/lib/avocado/extensions"
mkdir -p "$VAR_DIR/lib/avocado/images"
mkdir -p "$VAR_DIR/lib/avocado/os-releases/$VERSION_ID"
mkdir -p "$VAR_DIR/lib/avocado/runtimes"

OUTPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
mkdir -p $OUTPUT_DIR

# Create runtime-specific extensions directory
RUNTIME_EXT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/extensions"
mkdir -p "$RUNTIME_EXT_DIR"

# Clean up stale extensions to ensure fresh copies
echo "Cleaning up stale extensions..."
rm -f "$RUNTIME_EXT_DIR"/*.raw 2>/dev/null || true
rm -f "$VAR_DIR/lib/avocado/extensions"/*.raw 2>/dev/null || true

# Copy required extension images from global output/extensions to runtime-specific location
echo "Copying required extension images to runtime-specific directory..."
{}

{}

# Create symlinks in os-releases/<VERSION_ID> pointing to enabled extensions
echo "Creating OS release symlinks for VERSION_ID: $VERSION_ID"
for ext in "$VAR_DIR/lib/avocado/extensions/"*.raw; do
    if [ -f "$ext" ]; then
        ext_filename=$(basename "$ext")
        ln -sf "../../extensions/$ext_filename" "$VAR_DIR/lib/avocado/os-releases/$VERSION_ID/$ext_filename"
        echo "Created symlink: os-releases/$VERSION_ID/$ext_filename -> extensions/$ext_filename"
    fi
done
{}
{}

# Potential future SDK target hook.
# echo "Run: avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME"
# avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME

# Create btrfs image with extensions, images, os-releases, runtimes, and metadata subvolumes
mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/avocado/extensions \
    --subvol rw:lib/avocado/images \
    --subvol rw:lib/avocado/os-releases \
    --subvol rw:lib/avocado/runtimes \
    --subvol rw:lib/avocado/metadata \
    -f "$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"

echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '$TARGET_ARCH'."
avocado-build-$TARGET_ARCH $RUNTIME_NAME
"#,
            self.runtime_name,
            target_arch,
            copy_section,
            symlink_section,
            manifest_section,
            update_authority_section
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

        extensions.sort();
        extensions.dedup();

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
        assert!(script.contains("avocado-build-$TARGET_ARCH $RUNTIME_NAME"));
        assert!(script.contains("mkfs.btrfs"));
    }

    #[test]
    fn test_create_build_script_with_extensions() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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
        // Extension should be copied from output/extensions to runtime-specific extensions directory
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext-1.0.0.raw"));
    }

    #[test]
    fn test_create_build_script_with_extension_types() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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

        // Extension should be copied from output/extensions to runtime-specific directory
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext-1.0.0.raw"));
        // Should NOT include symlinks to systemd directories (runtime will handle this)
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $SYSEXT"));
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $CONFEXT"));
    }

    #[test]
    fn test_create_build_script_uses_extension_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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

        // Extension should be copied from output/extensions to runtime-specific directory
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext-1.0.0.raw"));
        // Should NOT include symlinks to systemd directories (runtime will handle this)
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $SYSEXT"));
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $CONFEXT"));
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
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val).unwrap();
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
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val).unwrap();
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
            crate::utils::config::Config::get_kernel_config_from_runtime(runtime_val).unwrap();
        assert!(kernel_config.is_none());
    }

    #[test]
    fn test_create_build_script_generates_manifest() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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
        assert!(script.contains("AVOCADO_EXT_PAIRS=\"test-ext:1.0.0\""));
        assert!(script.contains("AVOCADO_NS_UUID="));

        // Active symlink should be created
        assert!(script.contains("ln -sfn \"runtimes/"));
        assert!(script.contains("$VAR_DIR/lib/avocado/active"));

        // Images subvolume should be included in btrfs
        assert!(script.contains("--subvol rw:lib/avocado/images"));
        assert!(script.contains("--subvol rw:lib/avocado/runtimes"));
    }

    #[test]
    fn test_create_build_script_manifest_no_extensions() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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
        assert!(script.contains("AVOCADO_DISTRO_VERSION=\"$DISTRO_VERSION\""));
        assert!(script.contains("AVOCADO_EXT_PAIRS=\"\""));
        assert!(script.contains("ln -sfn \"runtimes/"));
    }

    #[test]
    fn test_create_build_script_manifest_has_uuid() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

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
