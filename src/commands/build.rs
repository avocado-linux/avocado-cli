//! Build command implementation that runs SDK compile, extension build, and runtime build.

use anyhow::{Context, Result};
use std::collections::HashSet;

use crate::commands::{
    ext::{ExtBuildCommand, ExtImageCommand},
    runtime::RuntimeBuildCommand,
    sdk::SdkCompileCommand,
};
use crate::utils::{
    config::Config,
    output::{print_info, print_success, OutputLevel},
};

/// Represents an extension dependency that can be either local or external
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionDependency {
    /// Extension defined in the main config file
    Local(String),
    /// Extension defined in an external config file
    External { name: String, config_path: String },
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
        }
    }

    /// Execute the build command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration and parse raw TOML
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Early target validation and logging - fail fast if target is unsupported
        let target =
            crate::utils::target::validate_and_log_target(self.target.as_deref(), &config)?;

        // If a specific extension is requested, build only that extension
        if let Some(ref ext_name) = self.extension {
            return self
                .build_single_extension(&config, &parsed, ext_name, &target)
                .await;
        }

        print_info(
            "Starting comprehensive build process...",
            OutputLevel::Normal,
        );

        // Determine which runtimes to build based on target
        let runtimes_to_build = self.get_runtimes_to_build(&config, &parsed, &target)?;

        if runtimes_to_build.is_empty() {
            print_info("No runtimes found to build.", OutputLevel::Normal);
            return Ok(());
        }

        // Step 1: Analyze dependencies and compile SDK code if needed
        print_info(
            "Step 1/4: Analyzing dependencies and preparing SDK",
            OutputLevel::Normal,
        );
        let required_extensions =
            self.find_required_extensions(&config, &parsed, &runtimes_to_build, &target)?;
        let sdk_sections = self.find_sdk_compile_sections(&config, &required_extensions)?;

        // Compile SDK sections if needed (but don't install dependencies - that's for 'avocado install')
        if !sdk_sections.is_empty() {
            if self.verbose {
                print_info(
                    &format!(
                        "Found {} SDK compile sections needed: {}",
                        sdk_sections.len(),
                        sdk_sections.join(", ")
                    ),
                    OutputLevel::Normal,
                );
            }

            let sdk_compile_cmd = SdkCompileCommand::new(
                self.config_path.clone(),
                self.verbose,
                sdk_sections,
                self.target.clone(),
                self.container_args.clone(),
                self.dnf_args.clone(),
            );
            sdk_compile_cmd
                .execute()
                .await
                .with_context(|| "Failed to compile SDK sections")?;
        } else {
            print_info("No SDK compilation needed.", OutputLevel::Normal);
        }

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
                        );
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
                        self.build_external_extension(&config, &self.config_path, name, ext_config_path, &target).await.with_context(|| {
                            format!("Failed to build external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Create images for external extension
                        self.create_external_extension_images(&config, &self.config_path, name, ext_config_path, &target).await.with_context(|| {
                            format!("Failed to create images for external extension '{name}' from config '{ext_config_path}'")
                        })?;

                        // Copy external extension images to output directory so runtime build can find them
                        self.copy_external_extension_images(&config, name, &target)
                            .await
                            .with_context(|| {
                                format!("Failed to copy images for external extension '{name}'")
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
                        );
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
            );
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
        parsed: &toml::Value,
        target: &str,
    ) -> Result<Vec<String>> {
        let runtime_section = parsed
            .get("runtime")
            .and_then(|r| r.as_table())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        let mut target_runtimes = Vec::new();

        for runtime_name in runtime_section.keys() {
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
                if let Some(runtime_target) = merged_value.get("target").and_then(|t| t.as_str()) {
                    // Runtime has explicit target - only include if it matches
                    if runtime_target == target {
                        target_runtimes.push(runtime_name.clone());
                    }
                } else {
                    // Runtime has no target specified - include for all targets
                    target_runtimes.push(runtime_name.clone());
                }
            } else {
                // If there's no merged config, check the base runtime config
                if let Some(runtime_config) = runtime_section.get(runtime_name) {
                    if let Some(runtime_target) =
                        runtime_config.get("target").and_then(|t| t.as_str())
                    {
                        // Runtime has explicit target - only include if it matches
                        if runtime_target == target {
                            target_runtimes.push(runtime_name.clone());
                        }
                    } else {
                        // Runtime has no target specified - include for all targets
                        target_runtimes.push(runtime_name.clone());
                    }
                }
            }
        }

        // If a specific runtime was requested but doesn't match the target, return an error
        if let Some(ref requested_runtime) = self.runtime {
            if target_runtimes.is_empty() {
                return Err(anyhow::anyhow!(
                    "Runtime '{}' is not configured for target '{}'",
                    requested_runtime,
                    target
                ));
            }
        }

        Ok(target_runtimes)
    }

    /// Find all extensions required by the specified runtimes and target
    fn find_required_extensions(
        &self,
        config: &Config,
        parsed: &toml::Value,
        runtimes: &[String],
        target: &str,
    ) -> Result<Vec<ExtensionDependency>> {
        let mut required_extensions = HashSet::new();
        let mut visited = HashSet::new(); // For cycle detection

        // If no runtimes are found for this target, don't build any extensions
        if runtimes.is_empty() {
            return Ok(vec![]);
        }

        let _runtime_section = parsed.get("runtime").and_then(|r| r.as_table()).unwrap();

        for runtime_name in runtimes {
            // Get merged runtime config for this target
            let merged_runtime =
                config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
            if let Some(merged_value) = merged_runtime {
                if let Some(dependencies) =
                    merged_value.get("dependencies").and_then(|d| d.as_table())
                {
                    for (_dep_name, dep_spec) in dependencies {
                        // Check for extension dependency
                        if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                            // Check if this is an external extension (has config field)
                            if let Some(external_config) =
                                dep_spec.get("config").and_then(|v| v.as_str())
                            {
                                let ext_dep = ExtensionDependency::External {
                                    name: ext_name.to_string(),
                                    config_path: external_config.to_string(),
                                };
                                required_extensions.insert(ext_dep.clone());

                                // Recursively find nested external extension dependencies
                                self.find_nested_external_extensions(
                                    config,
                                    &ext_dep,
                                    &mut required_extensions,
                                    &mut visited,
                                )?;
                            } else {
                                // Local extension
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
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Recursively find nested external extension dependencies
    fn find_nested_external_extensions(
        &self,
        config: &Config,
        ext_dep: &ExtensionDependency,
        required_extensions: &mut HashSet<ExtensionDependency>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        let (ext_name, ext_config_path) = match ext_dep {
            ExtensionDependency::External { name, config_path } => (name, config_path),
            ExtensionDependency::Local(_) => return Ok(()), // Local extensions don't have nested external deps
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
                "Extension '{}' not found in external config file '{}'",
                ext_name,
                ext_config_path
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
        let nested_config: toml::Value =
            toml::from_str(&nested_config_content).with_context(|| {
                format!(
                    "Failed to parse nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Create a temporary Config object for the nested config to handle its src_dir
        let nested_config_obj = Config::from_toml_value(&nested_config)?;

        // Check if this external extension has dependencies
        if let Some(dependencies) = extension_config
            .get("dependencies")
            .and_then(|d| d.as_table())
        {
            for (_dep_name, dep_spec) in dependencies {
                // Check for nested extension dependency
                if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
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

                        // Add the nested extension to required extensions
                        required_extensions.insert(nested_ext_dep.clone());

                        if self.verbose {
                            print_info(
                                &format!("Found nested external extension '{nested_ext_name}' required by '{ext_name}' at '{}'", nested_config_path.display()),
                                OutputLevel::Normal,
                            );
                        }

                        // Recursively process the nested extension
                        self.find_nested_external_extensions(
                            config,
                            &nested_ext_dep,
                            required_extensions,
                            visited,
                        )?;
                    } else {
                        // This is a local extension dependency within the external config
                        // We don't need to process it further as it will be handled during build
                        if self.verbose {
                            print_info(
                                &format!("Found local extension dependency '{nested_ext_name}' in external extension '{ext_name}'"),
                                OutputLevel::Normal,
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Find SDK compile sections needed for the required extensions
    fn find_sdk_compile_sections(
        &self,
        config: &Config,
        required_extensions: &[ExtensionDependency],
    ) -> Result<Vec<String>> {
        let mut needed_sections = HashSet::new();

        // If we have extensions to build, compile all SDK sections
        // A more sophisticated implementation could analyze which specific sections
        // are needed based on the extension's SDK dependencies
        if !required_extensions.is_empty() {
            let compile_dependencies = config.get_compile_dependencies();
            for section_name in compile_dependencies.keys() {
                needed_sections.insert(section_name.clone());
            }

            if self.verbose && !needed_sections.is_empty() {
                print_info(
                    &format!(
                        "Found {} extensions requiring SDK compilation",
                        required_extensions.len()
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        let mut sections: Vec<String> = needed_sections.into_iter().collect();
        sections.sort();
        Ok(sections)
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
                "Extension '{}' not found in external config file '{}'",
                extension_name,
                external_config_path
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
        let _external_config_toml: toml::Value = toml::from_str(&external_config_content)
            .with_context(|| {
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
        );

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
                "Failed to build external extension '{}' from '{}': {}",
                extension_name,
                external_config_path,
                e
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
                "Extension '{}' not found in external config file '{}'",
                extension_name,
                external_config_path
            )
        })?;

        // Get extension types from the external config
        let types = extension_config
            .get("types")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_else(|| vec!["sysext".to_string()]);

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
        );

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
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
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
                "Failed to verify images for external extension '{}'",
                extension_name
            ));
        }

        Ok(())
    }

    /// Build a single extension without building runtimes
    async fn build_single_extension(
        &self,
        config: &Config,
        parsed: &toml::Value,
        extension_name: &str,
        target: &str,
    ) -> Result<()> {
        print_info(
            &format!("Building single extension '{extension_name}' for target '{target}'"),
            OutputLevel::Normal,
        );

        // Check if this is a local extension or needs to be found in external configs
        let ext_config = parsed.get("ext").and_then(|ext| ext.get(extension_name));

        let extension_dep = if ext_config.is_some() {
            // Local extension
            ExtensionDependency::Local(extension_name.to_string())
        } else {
            // Try to find in external extensions - search all external dependencies
            self.find_external_extension(config, parsed, extension_name, target)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Extension '{}' not found in local extensions or external dependencies",
                        extension_name
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
                );
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
                );
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
        }

        print_success(
            &format!("Successfully built extension '{extension_name}'!"),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Find an external extension by searching through the full dependency tree
    fn find_external_extension(
        &self,
        config: &Config,
        parsed: &toml::Value,
        extension_name: &str,
        target: &str,
    ) -> Result<Option<ExtensionDependency>> {
        // First, collect all extensions in the dependency tree
        let mut all_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Get all extensions from runtime dependencies (this will recursively traverse)
        let runtime_section = parsed
            .get("runtime")
            .and_then(|r| r.as_table())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        for runtime_name in runtime_section.keys() {
            // Get merged runtime config for this target
            let merged_runtime =
                config.get_merged_runtime_config(runtime_name, target, &self.config_path)?;
            if let Some(merged_value) = merged_runtime {
                if let Some(dependencies) =
                    merged_value.get("dependencies").and_then(|d| d.as_table())
                {
                    for (_dep_name, dep_spec) in dependencies {
                        // Check for extension dependency
                        if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                            // Check if this is an external extension (has config field)
                            if let Some(external_config) =
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
                    &toml::from_str(&std::fs::read_to_string(&self.config_path)?)?,
                    name,
                    all_extensions,
                    visited,
                );
            }
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
                "Extension '{}' not found in external config file '{}'",
                ext_name,
                ext_config_path
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
        let nested_config: toml::Value =
            toml::from_str(&nested_config_content).with_context(|| {
                format!(
                    "Failed to parse nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Create a temporary Config object for the nested config to handle its src_dir
        let nested_config_obj = Config::from_toml_value(&nested_config)?;

        // Check if this external extension has dependencies
        if let Some(dependencies) = extension_config
            .get("dependencies")
            .and_then(|d| d.as_table())
        {
            for (_dep_name, dep_spec) in dependencies {
                // Check for nested extension dependency
                if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
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
        parsed: &toml::Value,
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
        parsed_config: &toml::Value,
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
        if let Some(ext_config) = parsed_config.get("ext").and_then(|ext| ext.get(ext_name)) {
            // Check if this local extension has dependencies
            if let Some(dependencies) = ext_config.get("dependencies").and_then(|d| d.as_table()) {
                for (_dep_name, dep_spec) in dependencies {
                    // Check for extension dependency
                    if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
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
            "avocado.toml".to_string(),
            true,
            Some("my-runtime".to_string()),
            Some("my-extension".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.toml");
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
            "avocado.toml".to_string(),
            false,
            Some("test-runtime".to_string()),
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.toml");
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
            "avocado.toml".to_string(),
            false,
            None,
            Some("test-extension".to_string()),
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.extension, Some("test-extension".to_string()));
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
