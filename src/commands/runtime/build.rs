use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    stamps::{
        compute_runtime_input_hash, generate_batch_read_stamps_script, generate_write_stamp_script,
        resolve_required_stamps_for_runtime_build, validate_stamps_batch, Stamp, StampOutputs,
    },
    target::resolve_target_required,
};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

pub struct RuntimeBuildCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
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
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Process container args with environment variable expansion
        let processed_container_args =
            crate::utils::config::Config::process_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        // Get runtime configuration
        let runtime_config = parsed
            .get("runtime")
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
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        // Initialize SDK container helper
        let container_helper =
            SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

        // Validate stamps before proceeding (unless --no-stamps)
        if !self.no_stamps {
            // Get detailed extension dependencies for this runtime
            // This distinguishes between local, external, and versioned extensions
            let ext_deps = config.get_runtime_extension_dependencies_detailed(
                &self.runtime_name,
                &target_arch,
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
                target: target_arch.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: processed_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
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

        // Collect extensions with versions for AVOCADO_EXT_LIST
        // This ensures the build scripts know exactly which extension versions to use
        let resolved_extensions = self
            .collect_runtime_extensions(
                &parsed,
                &config,
                &self.runtime_name,
                target_arch.as_str(),
                &self.config_path,
                container_image,
            )
            .await?;

        // Build var image
        let build_script = self.create_build_script(&parsed, &target_arch, &resolved_extensions)?;

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
            &target_arch,
            &self.config_path,
        )? {
            env_vars.insert("AVOCADO_STONE_INCLUDE_PATHS".to_string(), stone_paths);
        }

        // Get stone manifest if configured
        if let Some(stone_manifest) = config.get_stone_manifest_for_runtime(
            &self.runtime_name,
            &target_arch,
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

        let env_vars = if env_vars.is_empty() {
            None
        } else {
            Some(env_vars)
        };

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true, // need environment for build
            interactive: false,       // build script runs non-interactively
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: processed_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            env_vars,
            ..Default::default()
        };
        let complete_result = container_helper
            .run_in_container(run_config)
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
                .get_merged_runtime_config(&self.runtime_name, &target_arch, &self.config_path)?
                .unwrap_or_default();
            let inputs = compute_runtime_input_hash(&merged_runtime, &self.runtime_name)?;
            let outputs = StampOutputs::default();
            let stamp = Stamp::runtime_build(&self.runtime_name, &target_arch, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: processed_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

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
        parsed: &serde_yaml::Value,
        target_arch: &str,
        resolved_extensions: &[String],
    ) -> Result<String> {
        // Get merged runtime configuration including target-specific dependencies
        let config = crate::utils::config::Config::load(&self.config_path)?;
        let merged_runtime = config
            .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
            .with_context(|| {
                format!(
                    "Runtime '{}' not found or has no configuration for target '{}'",
                    self.runtime_name, target_arch
                )
            })?;

        let binding = serde_yaml::Mapping::new();
        let runtime_deps = merged_runtime
            .get("dependencies")
            .and_then(|v| v.as_mapping())
            .unwrap_or(&binding);

        // Extract extension names and any type overrides from runtime dependencies
        let mut required_extensions = HashSet::new();
        let mut extension_type_overrides: HashMap<String, Vec<String>> = HashMap::new();

        // First, collect direct runtime dependencies
        for (_dep_name, dep_spec) in runtime_deps {
            if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                required_extensions.insert(ext_name.to_string());

                // Check if the runtime dependency specifies custom types
                if let Some(types) = dep_spec.get("types").and_then(|v| v.as_sequence()) {
                    let type_strings: Vec<String> = types
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                    if !type_strings.is_empty() {
                        extension_type_overrides.insert(ext_name.to_string(), type_strings);
                    }
                }
            }
        }

        // Recursively discover all extension dependencies (including nested external extensions)
        let all_required_extensions =
            self.find_all_extension_dependencies(&config, &required_extensions, target_arch)?;

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
        if let Some(ext_config) = parsed.get("ext").and_then(|v| v.as_mapping()) {
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
mkdir -p "$VAR_DIR/lib/avocado/os-releases/$VERSION_ID"

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

# Potential future SDK target hook.
# echo "Run: avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME"
# avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME

# Create btrfs image with extensions and os-releases subvolumes
mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/avocado/extensions \
    --subvol rw:lib/avocado/os-releases \
    -f "$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"

echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '$TARGET_ARCH'."
avocado-build-$TARGET_ARCH $RUNTIME_NAME
"#,
            self.runtime_name, target_arch, copy_section, symlink_section
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
        config: &crate::utils::config::Config,
        ext_name: &str,
        all_extensions: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        target_arch: &str,
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

        // Check if this is a local extension
        if let Some(ext_config) = parsed
            .get("ext")
            .and_then(|e| e.as_mapping())
            .and_then(|table| table.get(ext_name))
        {
            // This is a local extension - check its dependencies
            if let Some(dependencies) = ext_config.get("dependencies").and_then(|d| d.as_mapping())
            {
                for (_dep_name, dep_spec) in dependencies {
                    if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                        // Check if this is an external extension dependency
                        if let Some(external_config_path) =
                            dep_spec.get("config").and_then(|v| v.as_str())
                        {
                            // This is an external extension - load its config and process recursively
                            let external_extensions = config.load_external_extensions(
                                &self.config_path,
                                external_config_path,
                            )?;

                            // Add the external extension itself
                            self.collect_extension_dependencies(
                                config,
                                nested_ext_name,
                                all_extensions,
                                visited,
                                target_arch,
                            )?;

                            // Process its dependencies from the external config
                            if let Some(ext_config) = external_extensions.get(nested_ext_name) {
                                if let Some(nested_deps) =
                                    ext_config.get("dependencies").and_then(|d| d.as_mapping())
                                {
                                    for (_nested_dep_name, nested_dep_spec) in nested_deps {
                                        if let Some(nested_nested_ext_name) =
                                            nested_dep_spec.get("ext").and_then(|v| v.as_str())
                                        {
                                            self.collect_extension_dependencies(
                                                config,
                                                nested_nested_ext_name,
                                                all_extensions,
                                                visited,
                                                target_arch,
                                            )?;
                                        }
                                    }
                                }
                            }
                        } else {
                            // This is a local extension dependency
                            self.collect_extension_dependencies(
                                config,
                                nested_ext_name,
                                all_extensions,
                                visited,
                                target_arch,
                            )?;
                        }
                    }
                }
            }
        } else {
            // This might be an external extension - we need to find it in the runtime dependencies
            // to get its config path, then process its dependencies
            let merged_runtime = config
                .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
                .with_context(|| {
                    format!(
                        "Runtime '{}' not found or has no configuration for target '{}'",
                        self.runtime_name, target_arch
                    )
                })?;

            if let Some(runtime_deps) = merged_runtime
                .get("dependencies")
                .and_then(|v| v.as_mapping())
            {
                for (_dep_name, dep_spec) in runtime_deps {
                    if let Some(dep_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                        if dep_ext_name == ext_name {
                            if let Some(external_config_path) =
                                dep_spec.get("config").and_then(|v| v.as_str())
                            {
                                // Found the external extension - process its dependencies
                                let external_extensions = config.load_external_extensions(
                                    &self.config_path,
                                    external_config_path,
                                )?;

                                if let Some(ext_config) = external_extensions.get(ext_name) {
                                    if let Some(nested_deps) =
                                        ext_config.get("dependencies").and_then(|d| d.as_mapping())
                                    {
                                        for (_nested_dep_name, nested_dep_spec) in nested_deps {
                                            if let Some(nested_ext_name) =
                                                nested_dep_spec.get("ext").and_then(|v| v.as_str())
                                            {
                                                self.collect_extension_dependencies(
                                                    config,
                                                    nested_ext_name,
                                                    all_extensions,
                                                    visited,
                                                    target_arch,
                                                )?;
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
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
    async fn collect_runtime_extensions(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        runtime_name: &str,
        target_arch: &str,
        config_path: &str,
        container_image: &str,
    ) -> Result<Vec<String>> {
        let merged_runtime =
            config.get_merged_runtime_config(runtime_name, target_arch, config_path)?;

        let runtime_dep_table = merged_runtime
            .as_ref()
            .and_then(|value| value.get("dependencies").and_then(|d| d.as_mapping()))
            .or_else(|| {
                parsed
                    .get("runtime")
                    .and_then(|r| r.get(runtime_name))
                    .and_then(|runtime_value| runtime_value.get("dependencies"))
                    .and_then(|d| d.as_mapping())
            });

        let mut extensions = Vec::new();

        if let Some(deps) = runtime_dep_table {
            for dep_spec in deps.values() {
                if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                    let version = self
                        .resolve_extension_version(
                            parsed,
                            config,
                            config_path,
                            ext_name,
                            dep_spec,
                            container_image,
                            target_arch,
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

    /// Resolve the version for an extension dependency.
    ///
    /// Priority order:
    /// 1. Explicit `vsn` field in the dependency spec (unless it's "*")
    /// 2. Version from external config file (if `config` field is specified)
    /// 3. Version from local `[ext]` section
    /// 4. Query RPM database for installed version
    #[allow(clippy::too_many_arguments)]
    async fn resolve_extension_version(
        &self,
        parsed: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        config_path: &str,
        ext_name: &str,
        dep_spec: &serde_yaml::Value,
        container_image: &str,
        target_arch: &str,
    ) -> Result<String> {
        // If version is explicitly specified with vsn field, use it (unless it's a wildcard)
        if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
            if version != "*" {
                return Ok(version.to_string());
            }
            // If vsn is "*", fall through to query RPM for the actual installed version
        }

        // If external config is specified, try to get version from it
        if let Some(external_config_path) = dep_spec.get("config").and_then(|v| v.as_str()) {
            let external_extensions =
                config.load_external_extensions(config_path, external_config_path)?;
            if let Some(ext_config) = external_extensions.get(ext_name) {
                if let Some(version) = ext_config.get("version").and_then(|v| v.as_str()) {
                    if version != "*" {
                        return Ok(version.to_string());
                    }
                    // If version is "*", fall through to query RPM
                }
            }
            // External config but no version found or version is "*" - query RPM database
            return self
                .query_rpm_version(ext_name, container_image, target_arch)
                .await;
        }

        // Try to get version from local [ext] section
        if let Some(version) = parsed
            .get("ext")
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
        self.query_rpm_version(ext_name, container_image, target_arch)
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

runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
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
        let resolved_extensions: Vec<String> = vec![];
        let script = cmd
            .create_build_script(&parsed, "x86_64", &resolved_extensions)
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

runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
      test-dep:
        ext: test-ext

ext:
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

        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&parsed, "x86_64", &resolved_extensions)
            .unwrap();

        assert!(script.contains("test-ext-1.0.0.raw"));
        // Extension should be copied from output/extensions to runtime-specific extensions directory
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext-1.0.0.raw"));
    }

    #[test]
    fn test_create_build_script_with_type_overrides() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
      test-dep:
        ext: test-ext
        types:
          - sysext

ext:
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

        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&parsed, "x86_64", &resolved_extensions)
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
    fn test_create_build_script_no_type_override_uses_extension_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
      test-dep:
        ext: test-ext

ext:
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

        let resolved_extensions = vec!["test-ext-1.0.0".to_string()];
        let script = cmd
            .create_build_script(&parsed, "x86_64", &resolved_extensions)
            .unwrap();

        // Extension should be copied from output/extensions to runtime-specific directory
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions"));
        assert!(script.contains("$RUNTIME_EXT_DIR/test-ext-1.0.0.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext-1.0.0.raw"));
        // Should NOT include symlinks to systemd directories (runtime will handle this)
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $SYSEXT"));
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext-1.0.0.raw $CONFEXT"));
    }
}
