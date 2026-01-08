// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

use anyhow::{Context, Result};

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::stamps::{
    compute_ext_input_hash, generate_batch_read_stamps_script, generate_write_stamp_script,
    resolve_required_stamps, validate_stamps_batch, Stamp, StampCommand, StampComponent,
    StampOutputs,
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

    pub async fn execute(&self) -> Result<()> {
        // Load composed configuration (includes remote extension configs)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;
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

            // Validate all stamps from batch output
            let validation =
                validate_stamps_batch(&required, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error = validation.into_error(&format!(
                    "Cannot create image for extension '{}'",
                    self.extension
                ));
                return Err(error.into());
            }
        }

        // Determine extension location by checking the composed (interpolated) config
        // This is more reliable than find_extension_in_dependency_tree which reads the raw file
        // and may not find templated extension names like "avocado-bsp-{{ avocado.target }}"
        let extension_location = {
            // First check if extension exists in the composed config's ext section
            let ext_in_composed = parsed
                .get("extensions")
                .and_then(|e| e.get(&self.extension));

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
            ExtensionLocation::External { config_path, .. } => config_path.clone(),
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
                ExtensionLocation::External { name, config_path } => {
                    print_info(
                        &format!("Found external extension '{name}' in config '{config_path}'"),
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
                let ext_section = parsed
                    .get("extensions")
                    .and_then(|ext| ext.get(&self.extension));

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
            #[allow(deprecated)]
            ExtensionLocation::External { config_path, .. } => {
                // For deprecated external configs, read from the file
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

        // Get extension version
        let ext_version = ext_config
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Extension '{}' is missing required 'version' field",
                    self.extension
                )
            })?;

        // Validate semver format
        Self::validate_semver(ext_version).with_context(|| {
            format!(
                "Extension '{}' has invalid version '{}'. Version must be in semantic versioning format (e.g., '1.0.0', '2.1.3')",
                self.extension, ext_version
            )
        })?;

        // Get extension types from the types array (defaults to ["sysext", "confext"])
        let ext_types = ext_config
            .get("types")
            .and_then(|v| v.as_sequence())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["sysext", "confext"]);

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

        let result = self
            .create_image(
                &container_helper,
                container_image,
                &target_arch,
                ext_version,
                &ext_types.join(","), // Pass types for potential future use
                repo_url.as_ref(),
                repo_release.as_ref(),
                &merged_container_args,
            )
            .await?;

        if result {
            print_success(
                &format!(
                    "Successfully created image for extension '{}-{}' (types: {}).",
                    self.extension,
                    ext_version,
                    ext_types.join(", ")
                ),
                OutputLevel::Normal,
            );

            // Write extension image stamp (unless --no-stamps)
            if !self.no_stamps {
                let inputs = compute_ext_input_hash(parsed, &self.extension)?;
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
    ) -> Result<bool> {
        // Create the build script
        let build_script = self.create_build_script(ext_version, extension_type);

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

    fn create_build_script(&self, ext_version: &str, _extension_type: &str) -> String {
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

# Create squashfs image
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs

echo "Created extension image: $OUTPUT_FILE"
"#,
            self.extension, ext_version
        )
    }

    /// Validate semantic versioning format (X.Y.Z where X, Y, Z are non-negative integers)
    fn validate_semver(version: &str) -> Result<()> {
        let parts: Vec<&str> = version.split('.').collect();

        if parts.len() < 3 {
            return Err(anyhow::anyhow!(
                "Version must follow semantic versioning format with at least MAJOR.MINOR.PATCH components (e.g., '1.0.0', '2.1.3')"
            ));
        }

        // Validate the first 3 components (MAJOR.MINOR.PATCH)
        for (i, part) in parts.iter().take(3).enumerate() {
            // Handle pre-release and build metadata (e.g., "1.0.0-alpha" or "1.0.0+build")
            let component = part.split(&['-', '+'][..]).next().unwrap_or(part);

            component.parse::<u32>().with_context(|| {
                let component_name = match i {
                    0 => "MAJOR",
                    1 => "MINOR",
                    2 => "PATCH",
                    _ => "component",
                };
                format!(
                    "{component_name} version component '{component}' must be a non-negative integer in semantic versioning format"
                )
            })?;
        }

        Ok(())
    }
}
