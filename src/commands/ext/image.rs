use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use super::find_ext_in_mapping;
use crate::utils::config::{ComposedConfig, Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::stamps::{
    compute_ext_input_hash, compute_ext_input_hash_with_fs, generate_batch_read_stamps_script,
    generate_write_stamp_script, resolve_required_stamps, validate_stamps_batch, Stamp,
    StampCommand, StampComponent, StampOutputs,
};
use crate::utils::target::resolve_target_required;

pub struct ExtImageCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    sdk_arch: Option<String>,
    output_dir: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl ExtImageCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            output_dir: None,
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

    /// Set host output directory to copy the image to after creation
    pub fn with_output_dir(mut self, output_dir: Option<String>) -> Self {
        self.output_dir = output_dir;
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

        // Merge container args from config and CLI
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Get SDK configuration from interpolated config (needed for stamp validation)
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        // Resolve the effective filesystem for this extension early — needed for stamp hashing.
        // If the extension explicitly sets `filesystem`, use that; otherwise inherit from rootfs.
        let rootfs_fs = config.get_rootfs_filesystem();
        let effective_fs = parsed
            .get("extensions")
            .and_then(|e| e.get(&self.extension))
            .and_then(|ext| ext.get("filesystem"))
            .and_then(|v| v.as_str())
            .unwrap_or(&rootfs_fs);

        // Validate stamps before proceeding (unless --no-stamps)
        if !self.no_stamps {
            let container_helper =
                SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

            // Resolve required stamps for extension image
            let required = resolve_required_stamps(
                StampCommand::Image,
                StampComponent::Extension,
                Some(&self.extension),
                &[], // No extension dependencies for ext image
            );

            // Batch all stamp reads into a single container invocation for performance
            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                runs_on: self.runs_on.clone(),
                nfs_port: self.nfs_port,
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            // Compute current inputs from composed config for staleness detection.
            // Use the base hash (without filesystem) to match what install/build stamps wrote.
            // The filesystem-aware hash is only used when writing/reading the image stamp itself.
            let current_inputs = compute_ext_input_hash(parsed, &self.extension).ok();
            let validation = validate_stamps_batch(
                &required,
                output.as_deref().unwrap_or(""),
                current_inputs
                    .as_ref()
                    .map(|i| (&StampComponent::Extension, i)),
            );

            if !validation.is_satisfied() {
                validation
                    .into_error(&format!(
                        "Cannot create image for extension '{}'",
                        self.extension
                    ))
                    .print_and_exit();
            }
        }

        // Determine extension location by checking the composed (interpolated) config
        // This is more reliable than find_extension_in_dependency_tree which reads the raw file
        // and may not find templated extension names like "avocado-bsp-{{ avocado.target }}"
        let extension_location = {
            // First check if extension exists in the composed config's ext section
            // Use find_ext_in_mapping to handle template keys like "avocado-bsp-{{ avocado.target }}"
            let ext_in_composed = find_ext_in_mapping(parsed, &self.extension, &target);

            if let Some(ext_config) = ext_in_composed {
                // Check if it has a source: field (indicating remote extension)
                if ext_config.get("source").is_some() {
                    // Parse the source to get ExtensionSource
                    let source = Config::parse_extension_source(&self.extension, ext_config)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Extension '{}' has source field but failed to parse it",
                                self.extension
                            )
                        })?;
                    ExtensionLocation::Remote {
                        name: self.extension.clone(),
                        source,
                    }
                } else {
                    // Local extension defined in main config
                    ExtensionLocation::Local {
                        name: self.extension.clone(),
                        config_path: self.config_path.clone(),
                    }
                }
            } else {
                // Fall back to comprehensive lookup for external extensions
                config
                    .find_extension_in_dependency_tree(&self.config_path, &self.extension, &target)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Extension '{}' not found in configuration.",
                            self.extension
                        )
                    })?
            }
        };

        // Get the config path where this extension is actually defined
        let _ext_config_path = match &extension_location {
            ExtensionLocation::Local { config_path, .. } => config_path.clone(),
            ExtensionLocation::Remote { name, .. } => {
                // Remote extensions are installed to $AVOCADO_PREFIX/includes/<name>/
                let ext_install_path =
                    config.get_extension_install_path(&self.config_path, name, &target);
                ext_install_path
                    .join("avocado.yaml")
                    .to_string_lossy()
                    .to_string()
            }
        };

        if self.verbose {
            match &extension_location {
                ExtensionLocation::Local { name, config_path } => {
                    print_info(
                        &format!("Found local extension '{name}' in config '{config_path}'"),
                        OutputLevel::Normal,
                    );
                }
                ExtensionLocation::Remote { name, source } => {
                    print_info(
                        &format!("Found remote extension '{name}' with source: {source:?}"),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Get extension configuration from the composed/merged config
        // For remote extensions, this comes from the merged remote extension config (already read via container)
        // For local extensions, this uses get_merged_ext_config which reads from the file
        let ext_config = match &extension_location {
            ExtensionLocation::Remote { .. } => {
                // Use the already-merged config from `parsed` which contains remote extension configs
                // Then apply target-specific overrides manually
                // Use find_ext_in_mapping to handle template keys like "avocado-bsp-{{ avocado.target }}"
                let ext_section = find_ext_in_mapping(parsed, &self.extension, &target);

                if self.verbose {
                    if let Some(all_ext) = parsed.get("extensions") {
                        if let Some(ext_map) = all_ext.as_mapping() {
                            let ext_names: Vec<_> =
                                ext_map.keys().filter_map(|k| k.as_str()).collect();
                            eprintln!(
                                "[DEBUG] Available extensions in composed config: {ext_names:?}"
                            );
                        }
                    }
                    eprintln!(
                        "[DEBUG] Looking for extension '{}' in composed config, found: {}",
                        self.extension,
                        ext_section.is_some()
                    );
                    if let Some(ext_val) = &ext_section {
                        eprintln!(
                            "[DEBUG] Extension '{}' config:\n{}",
                            self.extension,
                            serde_yaml::to_string(ext_val).unwrap_or_default()
                        );
                    }
                }

                if let Some(ext_val) = ext_section {
                    let base_ext = ext_val.clone();
                    // Check for target-specific override within this extension
                    let target_override = ext_val.get(&target).cloned();
                    if let Some(override_val) = target_override {
                        // Merge target override into base, filtering out other target sections
                        Some(config.merge_target_override(base_ext, override_val, &target))
                    } else {
                        Some(base_ext)
                    }
                } else {
                    None
                }
            }
            ExtensionLocation::Local { config_path, .. } => {
                // For local extensions, read from the file with proper target merging
                config.get_merged_ext_config(&self.extension, &target, config_path)?
            }
        }
        .ok_or_else(|| {
            anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
        })?;

        if self.verbose {
            eprintln!(
                "[DEBUG] Final ext_config for '{}':\n{}",
                self.extension,
                serde_yaml::to_string(&ext_config).unwrap_or_default()
            );
        }

        // Get extension version from the composed config (source of truth).
        // For remote extensions, the version comes from the merged remote extension avocado.yaml.
        let config_version = ext_config
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Extension '{}' is missing required 'version' field",
                    self.extension
                )
            })?;

        // Get extension types from the types array (defaults to ["sysext", "confext"])
        let ext_types = ext_config
            .get("types")
            .and_then(|v| v.as_sequence())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["sysext", "confext"]);

        // Use the effective filesystem resolved earlier (from ext config or rootfs default).
        // The per-extension `filesystem` key can still override via ext_config, but
        // effective_fs already accounts for that from the raw parsed YAML.
        let filesystem = ext_config
            .get("filesystem")
            .and_then(|v| v.as_str())
            .unwrap_or(effective_fs);

        match filesystem {
            "squashfs" | "erofs" | "erofs-lz4" | "erofs-zst" => {}
            other => {
                return Err(anyhow::anyhow!(
                    "Extension '{}' has invalid filesystem type '{}'. Must be 'squashfs', 'erofs', 'erofs-lz4', or 'erofs-zst'.",
                    self.extension,
                    other
                ));
            }
        }

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
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Config is the source of truth for extension versions — no wildcard resolution.
        let ext_version = config_version.to_string();

        // Validate semver format
        crate::utils::version::validate_semver(&ext_version).with_context(|| {
            format!(
                "Extension '{}' has invalid version '{}'. Version must be in semantic versioning format (e.g., '1.0.0', '2.1.3')",
                self.extension, ext_version
            )
        })?;

        // Create a single image for the extension
        // The runtime will decide whether to use it as sysext, confext, or both
        print_info(
            &format!(
                "Creating image for extension '{}' (types: {}).",
                self.extension,
                ext_types.join(", ")
            ),
            OutputLevel::Normal,
        );

        let source_date_epoch = config.source_date_epoch.unwrap_or(0);

        // Get var_files patterns — these files go on the var partition and are excluded from the .raw image
        let var_files = crate::utils::config::get_ext_var_files(&ext_config);

        let result = self
            .create_image(
                &container_helper,
                container_image,
                &target_arch,
                &ext_version,
                &ext_types.join(","), // Pass types for potential future use
                repo_url.as_ref(),
                repo_release.as_ref(),
                &merged_container_args,
                source_date_epoch,
                filesystem,
                &var_files,
            )
            .await?;

        if result {
            let image_filename = format!("{}-{}.raw", self.extension, ext_version);
            let container_image_path =
                format!("/opt/_avocado/{target_arch}/output/extensions/{image_filename}");

            // Copy image to host if --out specified
            if let Some(output_dir) = &self.output_dir {
                let cwd = std::env::current_dir().context("Failed to get current directory")?;
                let volume_manager =
                    crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
                let volume_state = volume_manager.get_or_create_volume(&cwd).await?;
                self.copy_image_to_host(
                    &volume_state.volume_name,
                    &container_image_path,
                    output_dir,
                    &image_filename,
                    container_image,
                )
                .await?;
                print_success(
                    &format!(
                        "Successfully created image for extension '{}-{}': {}",
                        self.extension,
                        &ext_version,
                        PathBuf::from(output_dir).join(&image_filename).display()
                    ),
                    OutputLevel::Normal,
                );
            } else {
                print_success(
                    &format!(
                        "Successfully created image for extension '{}-{}' (types: {}).",
                        self.extension,
                        &ext_version,
                        ext_types.join(", ")
                    ),
                    OutputLevel::Normal,
                );
            }

            // Write extension image stamp (unless --no-stamps)
            if !self.no_stamps {
                let inputs =
                    compute_ext_input_hash_with_fs(parsed, &self.extension, Some(filesystem))?;
                let outputs = StampOutputs::default();
                let stamp = Stamp::ext_image(&self.extension, &target, inputs, outputs);
                let stamp_script = generate_write_stamp_script(&stamp)?;

                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target.clone(),
                    command: stamp_script,
                    verbose: self.verbose,
                    source_environment: true,
                    interactive: false,
                    repo_url: repo_url.clone(),
                    repo_release: repo_release.clone(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    sdk_arch: self.sdk_arch.clone(),
                    ..Default::default()
                };

                let container_helper =
                    SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);
                container_helper.run_in_container(run_config).await?;

                if self.verbose {
                    print_info(
                        &format!("Wrote image stamp for extension '{}'.", self.extension),
                        OutputLevel::Normal,
                    );
                }
            }
        } else {
            return Err(anyhow::anyhow!(
                "Failed to create extension image for '{}-{}'",
                self.extension,
                ext_version
            ));
        }

        Ok(())
    }

    async fn copy_image_to_host(
        &self,
        volume_name: &str,
        container_image_path: &str,
        output_dir: &str,
        image_filename: &str,
        _container_image: &str,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Copying image to host: {output_dir}/{image_filename}"),
                OutputLevel::Normal,
            );
        }

        let host_output_dir = if output_dir.starts_with('/') {
            PathBuf::from(output_dir)
        } else {
            std::env::current_dir()?.join(output_dir)
        };
        std::fs::create_dir_all(&host_output_dir)?;

        let temp_container_id = self.create_temp_container(volume_name).await?;

        let docker_cp_source = format!("{temp_container_id}:{container_image_path}");
        let docker_cp_dest = host_output_dir.join(image_filename);

        let output = tokio::process::Command::new("docker")
            .args(["cp", &docker_cp_source, docker_cp_dest.to_str().unwrap()])
            .output()
            .await
            .context("Failed to run docker cp")?;

        let _ = tokio::process::Command::new("docker")
            .args(["rm", "-f", &temp_container_id])
            .output()
            .await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Failed to copy image to host: {stderr}"));
        }

        Ok(())
    }

    async fn create_temp_container(&self, volume_name: &str) -> Result<String> {
        let output = tokio::process::Command::new("docker")
            .args([
                "create",
                "--rm",
                "-v",
                &format!("{volume_name}:/opt/_avocado"),
                "busybox",
                "true",
            ])
            .output()
            .await
            .context("Failed to create temp container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Failed to create temp container: {stderr}"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_image(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        ext_version: &str,
        extension_type: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
        source_date_epoch: u64,
        filesystem: &str,
        var_files: &[String],
    ) -> Result<bool> {
        // Create the build script
        let build_script = self.create_build_script(
            ext_version,
            extension_type,
            source_date_epoch,
            filesystem,
            var_files,
        );

        // Execute the build script in the SDK container
        if self.verbose {
            print_info("Executing image build script.", OutputLevel::Normal);
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            runs_on: self.runs_on.clone(),
            nfs_port: self.nfs_port,
            ..Default::default()
        };
        let result = container_helper.run_in_container(config).await?;

        Ok(result)
    }

    fn create_build_script(
        &self,
        ext_version: &str,
        _extension_type: &str,
        source_date_epoch: u64,
        filesystem: &str,
        var_files: &[String],
    ) -> String {
        // Build exclude flags for var_files patterns (these go on the var partition, not in the .raw image)
        let var_excludes = var_files
            .iter()
            .map(|pattern| {
                // Strip trailing /** or /* glob suffixes to get the directory path for exclusion
                let clean = pattern.trim_end_matches("/**").trim_end_matches("/*");
                clean.to_string()
            })
            .collect::<Vec<_>>();

        let mkfs_command = match filesystem {
            "erofs" | "erofs-lz4" | "erofs-zst" => {
                let compress_flag = match filesystem {
                    "erofs-lz4" => "\n  -z lz4hc \\",
                    "erofs-zst" => "\n  -z zstd \\",
                    _ => "",
                };
                let exclude_flags = var_excludes
                    .iter()
                    .map(|p| format!("  --exclude-path={p} \\"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let exclude_section = if exclude_flags.is_empty() {
                    String::new()
                } else {
                    format!("\n{exclude_flags}")
                };
                format!(
                    r#"# Create erofs image
mkfs.erofs \
  -T "$SOURCE_DATE_EPOCH" \
  -U 00000000-0000-0000-0000-000000000000 \
  -x -1 \
  --all-root \{compress_flag}{exclude_section}
  "$OUTPUT_FILE" \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME""#
                )
            }
            _ => {
                let exclude_flags = var_excludes
                    .iter()
                    .map(|p| format!("  -e \"{p}\""))
                    .collect::<Vec<_>>()
                    .join(" \\\n");
                let exclude_section = if exclude_flags.is_empty() {
                    String::new()
                } else {
                    format!(" \\\n{exclude_flags}")
                };
                format!(
                    r#"# Create squashfs image
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs \
  -reproducible{exclude_section}"#
                )
            }
        };

        format!(
            r#"
set -e

# Common variables
EXT_NAME="{}"
EXT_VERSION="{}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/extensions"
OUTPUT_FILE="$OUTPUT_DIR/$EXT_NAME-$EXT_VERSION.raw"

# Create output directory
mkdir -p $OUTPUT_DIR

# Remove existing file if it exists
rm -f "$OUTPUT_FILE"

# Check if extension directory exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_EXT_SYSROOTS/$EXT_NAME."
    exit 1
fi

# Ensure reproducible timestamps
export SOURCE_DATE_EPOCH={source_date_epoch}

{mkfs_command}

echo "Created extension image: $OUTPUT_FILE"
"#,
            self.extension,
            ext_version,
            source_date_epoch = source_date_epoch,
            mkfs_command = mkfs_command,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cmd(extension: &str) -> ExtImageCommand {
        ExtImageCommand::new(
            extension.to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        )
    }

    #[test]
    fn test_create_build_script_erofs_contains_reproducible_flags() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs", &[]);

        assert!(
            script.contains("mkfs.erofs"),
            "script should invoke mkfs.erofs"
        );
        assert!(
            script.contains("-U 00000000-0000-0000-0000-000000000000"),
            "script should include nil UUID flag"
        );
        assert!(
            script.contains("-x -1"),
            "script should include -x -1 flag to disable xattr inlining"
        );
        assert!(
            script.contains("--all-root"),
            "script should include --all-root flag"
        );
    }

    #[test]
    fn test_create_build_script_erofs_lz4_includes_compression() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs-lz4", &[]);

        assert!(
            script.contains("mkfs.erofs"),
            "erofs-lz4 should invoke mkfs.erofs"
        );
        assert!(
            script.contains("-z lz4hc"),
            "erofs-lz4 should include -z lz4hc compression flag"
        );
        assert!(
            !script.contains("-z zstd"),
            "erofs-lz4 should not include zstd compression"
        );
    }

    #[test]
    fn test_create_build_script_erofs_zst_includes_compression() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs-zst", &[]);

        assert!(
            script.contains("mkfs.erofs"),
            "erofs-zst should invoke mkfs.erofs"
        );
        assert!(
            script.contains("-z zstd"),
            "erofs-zst should include -z zstd compression flag"
        );
        assert!(
            !script.contains("-z lz4hc"),
            "erofs-zst should not include lz4 compression"
        );
    }

    #[test]
    fn test_create_build_script_erofs_uncompressed_no_z_flag() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs", &[]);

        assert!(
            script.contains("mkfs.erofs"),
            "erofs should invoke mkfs.erofs"
        );
        assert!(
            !script.contains("-z "),
            "plain erofs should not include any -z compression flag"
        );
    }

    #[test]
    fn test_create_build_script_squashfs_contains_reproducible_flags() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "squashfs", &[]);

        assert!(
            script.contains("mksquashfs"),
            "script should invoke mksquashfs"
        );
        assert!(
            script.contains("-noappend"),
            "script should include -noappend flag"
        );
        assert!(
            script.contains("-no-xattrs"),
            "script should include -no-xattrs flag"
        );
        assert!(
            script.contains("-reproducible"),
            "script should include -reproducible flag"
        );
        assert!(
            !script.contains("mkfs.erofs"),
            "script should not invoke mkfs.erofs"
        );
    }

    #[test]
    fn test_create_build_script_defaults_to_squashfs() {
        let cmd = make_cmd("my-ext");
        // Passing "squashfs" simulates the default behavior
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "squashfs", &[]);

        assert!(
            script.contains("mksquashfs"),
            "default filesystem should use mksquashfs"
        );
    }

    #[test]
    fn test_create_build_script_source_date_epoch_default() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs", &[]);

        assert!(
            script.contains("export SOURCE_DATE_EPOCH=0"),
            "script should set SOURCE_DATE_EPOCH=0 when default is used"
        );
        assert!(
            script.contains("-T \"$SOURCE_DATE_EPOCH\""),
            "script should pass SOURCE_DATE_EPOCH to mkfs.erofs via -T"
        );
    }

    #[test]
    fn test_create_build_script_source_date_epoch_custom() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 1700000000, "erofs", &[]);

        assert!(
            script.contains("export SOURCE_DATE_EPOCH=1700000000"),
            "script should set SOURCE_DATE_EPOCH to the custom value"
        );
        assert!(
            !script.contains("SOURCE_DATE_EPOCH=0"),
            "script should not contain the default value when a custom one is set"
        );
    }

    #[test]
    fn test_create_build_script_extension_name_and_version() {
        let cmd = make_cmd("test-extension");
        let script = cmd.create_build_script("2.3.4", "sysext", 0, "squashfs", &[]);

        assert!(
            script.contains("EXT_NAME=\"test-extension\""),
            "script should contain the extension name"
        );
        assert!(
            script.contains("EXT_VERSION=\"2.3.4\""),
            "script should contain the extension version"
        );
    }

    #[test]
    fn test_create_build_script_output_path() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "squashfs", &[]);

        assert!(
            script.contains("OUTPUT_FILE=\"$OUTPUT_DIR/$EXT_NAME-$EXT_VERSION.raw\""),
            "script should set the output file with .raw extension"
        );
    }

    #[test]
    fn test_create_build_script_squashfs_var_files_excludes() {
        let cmd = make_cmd("my-ext");
        let var_files = vec![
            "var/lib/docker/**".to_string(),
            "var/lib/myapp/data".to_string(),
        ];
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "squashfs", &var_files);

        assert!(
            script.contains("-e \"var/lib/docker\""),
            "squashfs script should exclude var/lib/docker"
        );
        assert!(
            script.contains("-e \"var/lib/myapp/data\""),
            "squashfs script should exclude var/lib/myapp/data"
        );
    }

    #[test]
    fn test_create_build_script_erofs_var_files_excludes() {
        let cmd = make_cmd("my-ext");
        let var_files = vec!["var/lib/docker/**".to_string()];
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "erofs", &var_files);

        assert!(
            script.contains("--exclude-path=var/lib/docker"),
            "erofs script should exclude var/lib/docker"
        );
    }

    #[test]
    fn test_create_build_script_no_var_files_no_excludes() {
        let cmd = make_cmd("my-ext");
        let script = cmd.create_build_script("1.0.0", "sysext", 0, "squashfs", &[]);

        assert!(
            !script.contains("-e \"var/"),
            "script should not contain exclude flags when no var_files"
        );
    }
}
