//! Build command implementation that runs SDK compile, extension build, and runtime build.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

use crate::commands::{
    ext::{ExtBuildCommand, ExtImageCommand},
    runtime::RuntimeBuildCommand,
};
use crate::utils::{
    config::{ComposedConfig, Config, ExtensionSource},
    output::{print_info, print_success, OutputLevel},
};

/// Represents an extension dependency that can be either local, external, remote, or version-based
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionDependency {
    /// Extension defined in the main config file
    Local(String),
    /// Extension defined in an external config file (deprecated)
    External { name: String, config_path: String },
    /// Extension resolved via DNF with a version specification
    Versioned { name: String, version: String },
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

        print_info(
            "Starting comprehensive build process...",
            OutputLevel::Normal,
        );

        // Determine which runtimes to build based on target
        let runtimes_to_build = self.get_runtimes_to_build(config, parsed, &target)?;

        if runtimes_to_build.is_empty() {
            print_info("No runtimes found to build.", OutputLevel::Normal);
            return Ok(());
        }

        // Step 1: Analyze dependencies
        print_info("Step 1/4: Analyzing dependencies", OutputLevel::Normal);
        let required_extensions =
            self.find_required_extensions(config, parsed, &runtimes_to_build, &target)?;

        // Note: SDK compile sections are now compiled on-demand when extensions are built
        // This prevents duplicate compilation when sdk.compile sections are also extension dependencies

        // Step 2: Build extensions
        print_info("Step 2/4: Building extensions", OutputLevel::Normal);
        if !required_extensions.is_empty() {
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
                        .with_composed_config(Arc::clone(&composed));
                        ext_build_cmd.execute().await.with_context(|| {
                            format!("Failed to build extension '{extension_name}'")
                        })?;
                    }
                    ExtensionDependency::External {
                        name,
                        config_path: ext_config_path,
                    } => {
                        if self.verbose {
                            print_info(
                                &format!("Building external extension '{name}' from config '{ext_config_path}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Build external extension using its own config
                        self.build_external_extension(config, &self.config_path, name, ext_config_path, &target).await.with_context(|| {
                            format!("Failed to build external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Create images for external extension
                        self.create_external_extension_images(config, &self.config_path, name, ext_config_path, &target).await.with_context(|| {
                            format!("Failed to create images for external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Copy external extension images to output directory so runtime build can find them
                        self.copy_external_extension_images(config, name, &target)
                            .await
                            .with_context(|| {
                                format!("Failed to copy images for external extension '{name}'")
                            })?;
                    }
                    ExtensionDependency::Versioned { name, version } => {
                        if self.verbose {
                            print_info(
                                &format!("Skipping build for versioned extension '{name}' version '{version}' (installed via DNF)"),
                                OutputLevel::Normal,
                            );
                        }
                        // Versioned extensions are installed via DNF and don't need building
                    }
                    ExtensionDependency::Remote { name, source: _ } => {
                        if self.verbose {
                            print_info(
                                &format!("Building remote extension '{name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Build remote extension - ExtBuildCommand will load config from container
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
                        .with_composed_config(Arc::clone(&composed));
                        ext_build_cmd.execute().await.with_context(|| {
                            format!("Failed to build remote extension '{name}'")
                        })?;
                    }
                }
            }
        } else {
            print_info("No extensions to build.", OutputLevel::Normal);
        }

        // Step 3: Create extension images
        print_info("Step 3/4: Creating extension images", OutputLevel::Normal);
        if !required_extensions.is_empty() {
            for extension_dep in &required_extensions {
                match extension_dep {
                    ExtensionDependency::Local(extension_name) => {
                        if self.verbose {
                            print_info(
                                &format!("Creating image for local extension '{extension_name}'"),
                                OutputLevel::Normal,
                            );
                        }

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
                        .with_composed_config(Arc::clone(&composed));
                        ext_image_cmd.execute().await.with_context(|| {
                            format!("Failed to create image for extension '{extension_name}'")
                        })?;
                    }
                    ExtensionDependency::External {
                        name,
                        config_path: _ext_config_path,
                    } => {
                        if self.verbose {
                            print_info(
                                &format!("Skipping image creation for external extension '{name}' - already handled by ExtBuildCommand"),
                                OutputLevel::Normal,
                            );
                        }
                        // External extensions already have their images created by ExtBuildCommand::execute()
                        // No additional image creation needed
                    }
                    ExtensionDependency::Versioned { name, version } => {
                        if self.verbose {
                            print_info(
                                &format!("Creating image for versioned extension '{name}' version '{version}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Create image for versioned extension from its sysroot
                        self.create_versioned_extension_image(name, &target).await.with_context(|| {
                            format!("Failed to create image for versioned extension '{name}' version '{version}'")
                        })?;
                    }
                    ExtensionDependency::Remote { name, source: _ } => {
                        if self.verbose {
                            print_info(
                                &format!("Creating image for remote extension '{name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Create image for remote extension - ExtImageCommand will load config from container
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
                        .with_composed_config(Arc::clone(&composed));
                        ext_image_cmd.execute().await.with_context(|| {
                            format!("Failed to create image for remote extension '{name}'")
                        })?;
                    }
                }
            }
        } else {
            print_info("No extension images to create.", OutputLevel::Normal);
        }

        // Step 4: Build runtimes
        if let Some(ref runtime_name) = self.runtime {
            print_info(
                &format!("Step 4/4: Building runtime '{runtime_name}'"),
                OutputLevel::Normal,
            );
        } else {
            print_info("Step 4/4: Building all runtimes", OutputLevel::Normal);
        }

        for runtime_name in &runtimes_to_build {
            if self.verbose {
                print_info(
                    &format!("Building runtime '{runtime_name}'"),
                    OutputLevel::Normal,
                );
            }

            let runtime_build_cmd = RuntimeBuildCommand::new(
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
            runtime_build_cmd
                .execute()
                .await
                .with_context(|| format!("Failed to build runtime '{runtime_name}'"))?;
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
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Build an external extension using its own config file
    async fn build_external_extension(
        &self,
        config: &Config,
        base_config_path: &str,
        extension_name: &str,
        external_config_path: &str,
        target: &str,
    ) -> Result<()> {
        // Load the external extension configuration
        let external_extensions =
            config.load_external_extensions(base_config_path, external_config_path)?;

        let _extension_config = external_extensions.get(extension_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{extension_name}' not found in external config file '{external_config_path}'"
            )
        })?;

        // Load the external config as a TOML value to process the extension
        let resolved_external_config_path =
            config.resolve_path_relative_to_src_dir(base_config_path, external_config_path);
        let external_config_content = std::fs::read_to_string(&resolved_external_config_path)
            .with_context(|| {
                format!(
                    "Failed to read external config file: {}",
                    resolved_external_config_path.display()
                )
            })?;
        let _external_config_toml: serde_yaml::Value =
            serde_yaml::from_str(&external_config_content).with_context(|| {
                format!(
                    "Failed to parse external config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Create a temporary ExtBuildCommand to build the external extension
        let ext_build_cmd = crate::commands::ext::build::ExtBuildCommand::new(
            extension_name.to_string(),
            resolved_external_config_path.to_string_lossy().to_string(),
            self.verbose,
            Some(target.to_string()),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_no_stamps(self.no_stamps)
        .with_runs_on(self.runs_on.clone(), self.nfs_port)
        .with_sdk_arch(self.sdk_arch.clone());

        // Execute the extension build using the external config
        match ext_build_cmd.execute().await {
            Ok(_) => {
                print_info(
                    &format!("Successfully built external extension '{extension_name}' from '{external_config_path}'."),
                    OutputLevel::Normal,
                );
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "Failed to build external extension '{extension_name}' from '{external_config_path}': {e}"
            )),
        }
    }

    /// Create images for external extension using ExtImageCommand
    async fn create_external_extension_images(
        &self,
        config: &Config,
        base_config_path: &str,
        extension_name: &str,
        external_config_path: &str,
        target: &str,
    ) -> Result<()> {
        // Load the external extension configuration to get the types
        let external_extensions =
            config.load_external_extensions(base_config_path, external_config_path)?;

        let extension_config = external_extensions.get(extension_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{extension_name}' not found in external config file '{external_config_path}'"
            )
        })?;

        // Get extension types from the external config (defaults to ["sysext", "confext"])
        let types = extension_config
            .get("types")
            .and_then(|t| t.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_else(|| vec!["sysext".to_string(), "confext".to_string()]);

        // Resolve the external config path for ExtImageCommand
        let resolved_external_config_path =
            config.resolve_path_relative_to_src_dir(base_config_path, external_config_path);

        // Create ExtImageCommand for the external extension
        let ext_image_cmd = crate::commands::ext::image::ExtImageCommand::new(
            extension_name.to_string(),
            resolved_external_config_path.to_string_lossy().to_string(),
            self.verbose,
            Some(target.to_string()),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_no_stamps(self.no_stamps)
        .with_runs_on(self.runs_on.clone(), self.nfs_port)
        .with_sdk_arch(self.sdk_arch.clone());

        // Execute the image creation
        ext_image_cmd.execute().await.with_context(|| {
            format!("Failed to create images for external extension '{extension_name}'")
        })?;

        print_info(
            &format!("Successfully created images for external extension '{extension_name}' (types: {}).", types.join(", ")),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Copy external extension images from their sysroot to the output directory
    async fn copy_external_extension_images(
        &self,
        config: &Config,
        extension_name: &str,
        target: &str,
    ) -> Result<()> {
        // Get SDK configuration for container setup
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        let container_helper =
            crate::utils::container::SdkContainer::from_config(&self.config_path, config)?
                .verbose(self.verbose);

        // The images are already created in the correct location by ExtImageCommand
        // We just need to verify they exist
        let copy_command = format!(
            r#"
# Verify external extension images are in the output directory
echo "Verifying images for external extension {extension_name}:"
ls -la $AVOCADO_PREFIX/output/extensions/{extension_name}*.raw 2>/dev/null || {{
    echo "ERROR: No images found for external extension {extension_name} in output directory"
    exit 1
}}

echo "External extension {extension_name} images are ready in output directory"
"#
        );

        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: copy_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url,
            repo_release,
            container_args: merged_container_args,
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;

        if success {
            print_info(
                &format!("Successfully verified images for external extension '{extension_name}' in output directory."),
                OutputLevel::Normal,
            );
        } else {
            return Err(anyhow::anyhow!(
                "Failed to verify images for external extension '{extension_name}'"
            ));
        }

        Ok(())
    }

    /// Create image for a versioned extension from its sysroot
    async fn create_versioned_extension_image(
        &self,
        extension_name: &str,
        target: &str,
    ) -> Result<()> {
        print_info(
            &format!("Creating image for versioned extension '{extension_name}'."),
            OutputLevel::Normal,
        );

        // Load configuration
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let _parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Merge container args from config and CLI
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        // Initialize SDK container helper
        let container_helper = crate::utils::container::SdkContainer::new();

        // Query RPM version for the extension from the RPM database
        // Use the same RPM configuration that was used during installation
        let version_query_script = format!(
            r#"
set -e
# Query RPM version for extension from RPM database using the same config as installation
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
rpm --root="$AVOCADO_EXT_SYSROOTS/{extension_name}" --dbpath=/var/lib/extension.d/rpm -q {extension_name} --queryformat '%{{VERSION}}'
"#
        );

        let version_query_config = crate::utils::container::RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: version_query_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };

        let ext_version = container_helper
            .run_in_container_with_output(version_query_config)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to query RPM version for extension '{extension_name}'. The RPM database should contain this package. \
                    This may indicate the extension was not properly installed via packages, or the RPM database is corrupted."
                )
            })?;

        // Create the image creation script
        let source_date_epoch = config.source_date_epoch.unwrap_or(0);
        let image_script = format!(
            r#"
set -e

# Common variables
EXT_NAME="{extension_name}"
EXT_VERSION="{ext_version}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/extensions"
OUTPUT_FILE="$OUTPUT_DIR/$EXT_NAME-$EXT_VERSION.raw"

# Create output directory
mkdir -p $OUTPUT_DIR

# Remove existing file if it exists (including any old versions)
rm -f "$OUTPUT_DIR/$EXT_NAME"*.raw

# Check if extension sysroot exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_EXT_SYSROOTS/$EXT_NAME."
    exit 1
fi

# Ensure reproducible timestamps
export SOURCE_DATE_EPOCH={source_date_epoch}

# Create squashfs image from the versioned extension sysroot
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs \
  -reproducible

echo "Successfully created image for versioned extension '$EXT_NAME-$EXT_VERSION' at $OUTPUT_FILE"
"#
        );

        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: image_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url,
            repo_release,
            container_args: merged_container_args,
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;

        if success {
            print_info(
                &format!("Successfully created image for versioned extension '{extension_name}'."),
                OutputLevel::Normal,
            );
        } else {
            return Err(anyhow::anyhow!(
                "Failed to create image for versioned extension '{extension_name}'"
            ));
        }

        Ok(())
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
            ExtensionDependency::External {
                name,
                config_path: ext_config_path,
            } => {
                self.build_external_extension(config, &self.config_path, name, ext_config_path, target).await.with_context(|| {
                    format!("Failed to build external extension '{name}' from config '{ext_config_path}'")
                })?;
            }
            ExtensionDependency::Versioned { name, version } => {
                return Err(anyhow::anyhow!(
                    "Cannot build individual versioned extension '{name}' version '{version}'. Versioned extensions are installed via DNF."
                ));
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
            ExtensionDependency::External {
                name,
                config_path: ext_config_path,
            } => {
                self.create_external_extension_images(config, &self.config_path, name, ext_config_path, target).await.with_context(|| {
                    format!("Failed to create images for external extension '{name}' from config '{ext_config_path}'")
                })?;

                self.copy_external_extension_images(config, name, target)
                    .await
                    .with_context(|| {
                        format!("Failed to copy images for external extension '{name}'")
                    })?;
            }
            ExtensionDependency::Versioned { name, version } => {
                return Err(anyhow::anyhow!(
                    "Cannot create image for individual versioned extension '{name}' version '{version}'. Versioned extensions are installed via DNF."
                ));
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
                    ExtensionDependency::External {
                        name,
                        config_path: ext_config_path,
                    } => {
                        if self.verbose {
                            print_info(
                                &format!("Building external extension '{name}' from config '{ext_config_path}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Build external extension
                        self.build_external_extension(config, &self.config_path, name, ext_config_path, target).await.with_context(|| {
                            format!("Failed to build external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Create images for external extension
                        self.create_external_extension_images(config, &self.config_path, name, ext_config_path, target).await.with_context(|| {
                            format!("Failed to create images for external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Copy external extension images
                        self.copy_external_extension_images(config, name, target)
                            .await
                            .with_context(|| {
                                format!("Failed to copy images for external extension '{name}'")
                            })?;
                    }
                    ExtensionDependency::Versioned { name, version } => {
                        if self.verbose {
                            print_info(
                                &format!("Skipping build for versioned extension '{name}' version '{version}' (installed via DNF)"),
                                OutputLevel::Normal,
                            );
                        }
                        // Versioned extensions are installed via DNF and don't need building
                        // But they do need images created from their sysroots
                        self.create_versioned_extension_image(name, target).await.with_context(|| {
                            format!("Failed to create image for versioned extension '{name}' version '{version}'")
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

        let runtime_build_cmd = RuntimeBuildCommand::new(
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
                    // Check if this is a versioned extension (has vsn field)
                    if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
                        let ext_dep = ExtensionDependency::Versioned {
                            name: ext_name.to_string(),
                            version: version.to_string(),
                        };
                        required_extensions.insert(ext_dep);
                    }
                    // Check if this is an external extension (has config field)
                    else if let Some(external_config) =
                        dep_spec.get("config").and_then(|v| v.as_str())
                    {
                        let ext_dep = ExtensionDependency::External {
                            name: ext_name.to_string(),
                            config_path: external_config.to_string(),
                        };
                        required_extensions.insert(ext_dep.clone());

                        // Recursively find nested external extension dependencies
                        self.find_all_nested_extensions(
                            config,
                            &ext_dep,
                            &mut required_extensions,
                            &mut visited,
                        )?;
                    } else {
                        // Local extension
                        required_extensions
                            .insert(ExtensionDependency::Local(ext_name.to_string()));

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
        }

        let mut extensions: Vec<ExtensionDependency> = required_extensions.into_iter().collect();
        extensions.sort_by(|a, b| {
            let name_a = match a {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
                ExtensionDependency::Remote { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Find an external extension by searching through the full dependency tree
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
                            // Check if this is a versioned extension (has vsn field)
                            if let Some(version) = dep_spec.get("vsn").and_then(|v| v.as_str()) {
                                let ext_dep = ExtensionDependency::Versioned {
                                    name: ext_name.to_string(),
                                    version: version.to_string(),
                                };
                                all_extensions.insert(ext_dep);
                            }
                            // Check if this is an external extension (has config field)
                            else if let Some(external_config) =
                                dep_spec.get("config").and_then(|v| v.as_str())
                            {
                                let ext_dep = ExtensionDependency::External {
                                    name: ext_name.to_string(),
                                    config_path: external_config.to_string(),
                                };
                                all_extensions.insert(ext_dep.clone());

                                // Recursively find nested external extension dependencies
                                self.find_all_nested_extensions(
                                    config,
                                    &ext_dep,
                                    &mut all_extensions,
                                    &mut visited,
                                )?;
                            } else {
                                // Local extension
                                all_extensions
                                    .insert(ExtensionDependency::Local(ext_name.to_string()));

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
        }

        // Now search for the target extension in all collected extensions
        for ext_dep in all_extensions {
            let found_name = match &ext_dep {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
                ExtensionDependency::Remote { name, .. } => name,
            };

            if found_name == extension_name {
                return Ok(Some(ext_dep));
            }
        }

        Ok(None)
    }

    /// Recursively find all nested extensions (enhanced version of find_nested_external_extensions)
    fn find_all_nested_extensions(
        &self,
        config: &Config,
        ext_dep: &ExtensionDependency,
        all_extensions: &mut HashSet<ExtensionDependency>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        let (ext_name, ext_config_path) = match ext_dep {
            ExtensionDependency::External { name, config_path } => (name, config_path),
            ExtensionDependency::Local(name) => {
                // For local extensions, we need to check their dependencies too
                return self.find_local_extension_dependencies(
                    config,
                    &serde_yaml::from_str(&std::fs::read_to_string(&self.config_path)?)?,
                    name,
                    all_extensions,
                    visited,
                );
            }
            ExtensionDependency::Versioned { .. } => return Ok(()), // Versioned extensions don't have nested deps
            ExtensionDependency::Remote { .. } => return Ok(()), // Remote extensions are handled separately
        };

        // Cycle detection: check if we've already processed this extension
        let ext_key = format!("{ext_name}:{ext_config_path}");
        if visited.contains(&ext_key) {
            if self.verbose {
                print_info(
                    &format!("Skipping already processed extension '{ext_name}' to avoid cycles"),
                    OutputLevel::Normal,
                );
            }
            return Ok(());
        }
        visited.insert(ext_key);

        // Load the external extension configuration
        let resolved_external_config_path =
            config.resolve_path_relative_to_src_dir(&self.config_path, ext_config_path);
        let external_extensions =
            config.load_external_extensions(&self.config_path, ext_config_path)?;

        let extension_config = external_extensions.get(ext_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{ext_name}' not found in external config file '{ext_config_path}'"
            )
        })?;

        // Load the nested config file to get its src_dir setting
        let nested_config_content = std::fs::read_to_string(&resolved_external_config_path)
            .with_context(|| {
                format!(
                    "Failed to read nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;
        let nested_config: serde_yaml::Value = serde_yaml::from_str(&nested_config_content)
            .with_context(|| {
                format!(
                    "Failed to parse nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Create a temporary Config object for the nested config to handle its src_dir
        let nested_config_obj = serde_yaml::from_value::<Config>(nested_config.clone())?;

        // Check if this external extension has dependencies
        if let Some(dependencies) = extension_config
            .get("packages")
            .and_then(|d| d.as_mapping())
        {
            for (_dep_name, dep_spec) in dependencies {
                // Check for nested extension dependency
                if let Some(nested_ext_name) = dep_spec.get("extensions").and_then(|v| v.as_str()) {
                    // Check if this is a nested external extension (has config field)
                    if let Some(nested_external_config) =
                        dep_spec.get("config").and_then(|v| v.as_str())
                    {
                        // Resolve the nested config path relative to the nested config's src_dir
                        let nested_config_path = nested_config_obj
                            .resolve_path_relative_to_src_dir(
                                &resolved_external_config_path,
                                nested_external_config,
                            );

                        let nested_ext_dep = ExtensionDependency::External {
                            name: nested_ext_name.to_string(),
                            config_path: nested_config_path.to_string_lossy().to_string(),
                        };

                        // Add the nested extension to all extensions
                        all_extensions.insert(nested_ext_dep.clone());

                        if self.verbose {
                            print_info(
                                &format!("Found nested external extension '{nested_ext_name}' required by '{ext_name}' at '{}'", nested_config_path.display()),
                                OutputLevel::Normal,
                            );
                        }

                        // Recursively process the nested extension
                        self.find_all_nested_extensions(
                            config,
                            &nested_ext_dep,
                            all_extensions,
                            visited,
                        )?;
                    } else {
                        // This is a local extension dependency within the external config
                        all_extensions
                            .insert(ExtensionDependency::Local(nested_ext_name.to_string()));

                        if self.verbose {
                            print_info(
                                &format!("Found local extension dependency '{nested_ext_name}' in external extension '{ext_name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Check dependencies of this local extension in the external config
                        self.find_local_extension_dependencies_in_config(
                            config,
                            &nested_config,
                            nested_ext_name,
                            &resolved_external_config_path,
                            all_extensions,
                            visited,
                        )?;
                    }
                }
            }
        }

        Ok(())
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
                        // Check if this is an external extension (has config field)
                        if let Some(external_config) =
                            dep_spec.get("config").and_then(|v| v.as_str())
                        {
                            let ext_dep = ExtensionDependency::External {
                                name: nested_ext_name.to_string(),
                                config_path: external_config.to_string(),
                            };
                            all_extensions.insert(ext_dep.clone());

                            // Recursively find nested external extension dependencies
                            self.find_all_nested_extensions(
                                config,
                                &ext_dep,
                                all_extensions,
                                visited,
                            )?;
                        } else {
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

    /// Helper that replicates the versioned extension image script template
    /// from `create_versioned_extension_image` so we can unit-test the
    /// SOURCE_DATE_EPOCH interpolation without needing a container.
    fn build_versioned_image_script(
        extension_name: &str,
        ext_version: &str,
        source_date_epoch: u64,
    ) -> String {
        format!(
            r#"
set -e

# Common variables
EXT_NAME="{extension_name}"
EXT_VERSION="{ext_version}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/extensions"
OUTPUT_FILE="$OUTPUT_DIR/$EXT_NAME-$EXT_VERSION.raw"

# Create output directory
mkdir -p $OUTPUT_DIR

# Remove existing file if it exists (including any old versions)
rm -f "$OUTPUT_DIR/$EXT_NAME"*.raw

# Check if extension sysroot exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_EXT_SYSROOTS/$EXT_NAME."
    exit 1
fi

# Ensure reproducible timestamps
export SOURCE_DATE_EPOCH={source_date_epoch}

# Create squashfs image from the versioned extension sysroot
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs \
  -reproducible

echo "Successfully created image for versioned extension '$EXT_NAME-$EXT_VERSION' at $OUTPUT_FILE"
"#
        )
    }

    #[test]
    fn test_versioned_image_script_source_date_epoch_default() {
        let script = build_versioned_image_script("my-ext", "1.0.0", 0);

        assert!(
            script.contains("export SOURCE_DATE_EPOCH=0"),
            "script should set SOURCE_DATE_EPOCH=0 when default is used"
        );
        assert!(
            script.contains("-reproducible"),
            "script should include -reproducible flag"
        );
        assert!(
            script.contains("mksquashfs"),
            "script should invoke mksquashfs"
        );
    }

    #[test]
    fn test_versioned_image_script_source_date_epoch_custom() {
        let script = build_versioned_image_script("my-ext", "1.0.0", 1700000000);

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
    fn test_versioned_image_script_extension_name_and_version() {
        let script = build_versioned_image_script("test-extension", "2.3.4", 0);

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
    fn test_versioned_image_script_reproducible_flags() {
        let script = build_versioned_image_script("my-ext", "1.0.0", 0);

        assert!(
            script.contains("-reproducible"),
            "script should include -reproducible flag"
        );
        assert!(
            script.contains("-noappend"),
            "script should include -noappend flag"
        );
        assert!(
            script.contains("-no-xattrs"),
            "script should include -no-xattrs flag"
        );
    }
}
