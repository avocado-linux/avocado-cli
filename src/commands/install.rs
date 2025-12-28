//! Install command implementation that runs SDK, extension, and runtime installs.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::commands::{
    ext::ExtInstallCommand, runtime::RuntimeInstallCommand, sdk::SdkInstallCommand,
};
use crate::utils::{
    config::{ComposedConfig, Config},
    container::SdkContainer,
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{print_info, print_success, OutputLevel},
    target::validate_and_log_target,
};

/// Represents an extension dependency that can be either local or external
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExtensionDependency {
    /// Extension defined in the main config file
    Local(String),
    /// Extension defined in an external config file
    External { name: String, config_path: String },
    /// Extension resolved via DNF with a version specification
    Versioned { name: String, version: String },
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

    /// Execute the install command
    pub async fn execute(&self) -> Result<()> {
        // Early target validation - load basic config first to validate target
        let basic_config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let _target = validate_and_log_target(self.target.as_deref(), &basic_config)?;

        // Load the composed configuration (merges external configs, applies interpolation)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;

        let config = &composed.config;
        let parsed = &composed.merged_value;

        print_info(
            "Starting comprehensive install process...",
            OutputLevel::Normal,
        );

        // Load lock file for reproducible builds (used for versioned extensions in this command)
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                PathBuf::from(&self.config_path)
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            });

        // We'll load the lock file lazily when needed (for external/versioned extensions)
        let mut lock_file;

        // 1. Install SDK dependencies
        print_info("Step 1/3: Installing SDK dependencies", OutputLevel::Normal);
        let sdk_install_cmd = SdkInstallCommand::new(
            self.config_path.clone(),
            self.verbose,
            self.force,
            self.target.clone(),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_no_stamps(self.no_stamps)
        .with_runs_on(self.runs_on.clone(), self.nfs_port);
        sdk_install_cmd
            .execute()
            .await
            .with_context(|| "Failed to install SDK dependencies")?;

        // 2. Install extension dependencies
        print_info(
            "Step 2/3: Installing extension dependencies",
            OutputLevel::Normal,
        );

        // Determine which extensions to install based on runtime dependencies and target
        let extensions_to_install = self.find_required_extensions(&composed, &_target)?;

        if !extensions_to_install.is_empty() {
            for extension_dep in &extensions_to_install {
                match extension_dep {
                    ExtensionDependency::Local(extension_name) => {
                        if self.verbose {
                            print_info(
                                &format!("Installing local extension dependencies for '{extension_name}'"),
                                OutputLevel::Normal,
                            );
                        }

                        let ext_install_cmd = ExtInstallCommand::new(
                            Some(extension_name.clone()),
                            self.config_path.clone(),
                            self.verbose,
                            self.force,
                            self.target.clone(),
                            self.container_args.clone(),
                            self.dnf_args.clone(),
                        )
                        .with_no_stamps(self.no_stamps)
                        .with_runs_on(self.runs_on.clone(), self.nfs_port);
                        ext_install_cmd.execute().await.with_context(|| {
                            format!(
                                "Failed to install extension dependencies for '{extension_name}'"
                            )
                        })?;
                    }
                    ExtensionDependency::External {
                        name,
                        config_path: ext_config_path,
                    } => {
                        if self.verbose {
                            print_info(
                                &format!("Installing external extension dependencies for '{name}' from config '{ext_config_path}'"),
                                OutputLevel::Normal,
                            );
                        }

                        // Reload lock file from disk to get latest state from previous installs
                        lock_file = LockFile::load(&src_dir)?;

                        // Install external extension to ${AVOCADO_PREFIX}/extensions/<ext_name>
                        self.install_external_extension(config, &self.config_path, name, ext_config_path, &_target, &mut lock_file).await.with_context(|| {
                            format!("Failed to install external extension '{name}' from config '{ext_config_path}'")
                        })?;
                    }
                    ExtensionDependency::Versioned { name, version } => {
                        if self.verbose {
                            print_info(
                                &format!(
                                    "Installing versioned extension '{name}' version '{version}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }

                        // Reload lock file from disk to get latest state from previous installs
                        lock_file = LockFile::load(&src_dir)?;

                        // Install versioned extension to its own sysroot
                        self.install_versioned_extension(config, name, version, &_target, &mut lock_file).await.with_context(|| {
                            format!("Failed to install versioned extension '{name}' version '{version}'")
                        })?;
                    }
                }
            }
        } else {
            print_info("No extension dependencies to install.", OutputLevel::Normal);
        }

        // 3. Install runtime dependencies (filtered by target)
        let target_runtimes = self.find_target_relevant_runtimes(config, parsed, &_target)?;

        if target_runtimes.is_empty() {
            print_info(
                &format!("Step 3/3: No runtimes found for target '{_target}'. Skipping runtime dependencies."),
                OutputLevel::Normal,
            );
        } else {
            if target_runtimes.len() == 1 {
                print_info(
                    &format!(
                        "Step 3/3: Installing runtime dependencies for '{}' (target: {_target})",
                        target_runtimes[0]
                    ),
                    OutputLevel::Normal,
                );
            } else {
                print_info(
                    &format!("Step 3/3: Installing runtime dependencies for {} runtimes (target: {_target})", target_runtimes.len()),
                    OutputLevel::Normal,
                );
            }

            for runtime_name in &target_runtimes {
                if self.verbose {
                    print_info(
                        &format!("Installing runtime dependencies for '{runtime_name}'"),
                        OutputLevel::Normal,
                    );
                }

                let runtime_install_cmd = RuntimeInstallCommand::new(
                    Some(runtime_name.clone()), // Install for this specific target-relevant runtime
                    self.config_path.clone(),
                    self.verbose,
                    self.force,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                )
                .with_no_stamps(self.no_stamps)
                .with_runs_on(self.runs_on.clone(), self.nfs_port);
                runtime_install_cmd.execute().await.with_context(|| {
                    format!("Failed to install runtime dependencies for '{runtime_name}'")
                })?;
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
        let mut visited = HashSet::new(); // For cycle detection

        let config = &composed.config;
        let parsed = &composed.merged_value;
        let config_path = &composed.config_path;

        // First, find which runtimes are relevant for this target
        let target_runtimes = self.find_target_relevant_runtimes(config, parsed, target)?;

        if target_runtimes.is_empty() {
            if self.verbose {
                print_info(
                    &format!("No runtimes found for target '{target}'. Installing all extensions."),
                    OutputLevel::Normal,
                );
            }
            // If no runtimes match this target, install all local extensions
            if let Some(ext_section) = parsed.get("ext").and_then(|e| e.as_mapping()) {
                for ext_name_val in ext_section.keys() {
                    if let Some(ext_name) = ext_name_val.as_str() {
                        required_extensions
                            .insert(ExtensionDependency::Local(ext_name.to_string()));
                    }
                }
            }
        } else {
            // Only install extensions needed by the target-relevant runtimes
            if let Some(runtime_section) = parsed.get("runtime").and_then(|r| r.as_mapping()) {
                for runtime_name in &target_runtimes {
                    if let Some(_runtime_config) = runtime_section.get(runtime_name) {
                        // Check both base dependencies and target-specific dependencies
                        let merged_runtime =
                            config.get_merged_runtime_config(runtime_name, target, config_path)?;
                        if let Some(merged_value) = merged_runtime {
                            if let Some(dependencies) = merged_value
                                .get("dependencies")
                                .and_then(|d| d.as_mapping())
                            {
                                for (_dep_name, dep_spec) in dependencies {
                                    // Check for extension dependency
                                    if let Some(ext_name) =
                                        dep_spec.get("ext").and_then(|v| v.as_str())
                                    {
                                        // Check if this is a versioned extension (has vsn field)
                                        if let Some(version) =
                                            dep_spec.get("vsn").and_then(|v| v.as_str())
                                        {
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
                                            self.find_nested_external_extensions(
                                                config,
                                                config_path,
                                                &ext_dep,
                                                &mut required_extensions,
                                                &mut visited,
                                            )?;
                                        } else {
                                            // Local extension
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
        }

        let mut extensions: Vec<ExtensionDependency> = required_extensions.into_iter().collect();
        extensions.sort_by(|a, b| {
            let name_a = match a {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
            };
            let name_b = match b {
                ExtensionDependency::Local(name) => name,
                ExtensionDependency::External { name, .. } => name,
                ExtensionDependency::Versioned { name, .. } => name,
            };
            name_a.cmp(name_b)
        });
        Ok(extensions)
    }

    /// Recursively find nested external extension dependencies
    fn find_nested_external_extensions(
        &self,
        config: &Config,
        base_config_path: &str,
        ext_dep: &ExtensionDependency,
        required_extensions: &mut std::collections::HashSet<ExtensionDependency>,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<()> {
        let (ext_name, ext_config_path) = match ext_dep {
            ExtensionDependency::External { name, config_path } => (name, config_path),
            ExtensionDependency::Local(_) => return Ok(()), // Local extensions don't have nested external deps
            ExtensionDependency::Versioned { .. } => return Ok(()), // Versioned extensions don't have nested deps
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
            config.resolve_path_relative_to_src_dir(base_config_path, ext_config_path);
        let external_extensions =
            config.load_external_extensions(base_config_path, ext_config_path)?;

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
            .get("dependencies")
            .and_then(|d| d.as_mapping())
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
                            base_config_path,
                            &nested_ext_dep,
                            required_extensions,
                            visited,
                        )?;
                    } else {
                        // This is a local extension dependency within the external config
                        // We don't need to process it further as it will be handled during installation
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

    /// Find runtimes that are relevant for the specified target
    fn find_target_relevant_runtimes(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        target: &str,
    ) -> Result<Vec<String>> {
        let mut relevant_runtimes = Vec::new();

        if let Some(runtime_section) = parsed.get("runtime").and_then(|r| r.as_mapping()) {
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

    /// Install an external extension to ${AVOCADO_PREFIX}/extensions/<ext_name>
    async fn install_external_extension(
        &self,
        config: &Config,
        base_config_path: &str,
        extension_name: &str,
        external_config_path: &str,
        target: &str,
        lock_file: &mut LockFile,
    ) -> Result<()> {
        // Load the external extension configuration
        let external_extensions =
            config.load_external_extensions(base_config_path, external_config_path)?;

        let extension_config = external_extensions.get(extension_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{extension_name}' not found in external config file '{external_config_path}'"
            )
        })?;

        // Create the sysroot for external extension
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        let container_helper =
            crate::utils::container::SdkContainer::from_config(&self.config_path, config)?
                .verbose(self.verbose);

        // Check if extension sysroot already exists
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{extension_name} ]");
        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.clone(),
            target: target.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };
        let sysroot_exists = container_helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            // Create the sysroot for external extension
            let setup_command = format!(
                "mkdir -p $AVOCADO_EXT_SYSROOTS/{extension_name}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm $AVOCADO_EXT_SYSROOTS/{extension_name}/var/lib"
            );
            let run_config = crate::utils::container::RunConfig {
                container_image: container_image.clone(),
                target: target.to_string(),
                command: setup_command,
                verbose: self.verbose,
                source_environment: false,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };
            let success = container_helper.run_in_container(run_config).await?;

            if !success {
                return Err(anyhow::anyhow!(
                    "Failed to create sysroot for external extension '{extension_name}'"
                ));
            }

            print_info(
                &format!("Created sysroot for external extension '{extension_name}'."),
                crate::utils::output::OutputLevel::Normal,
            );
        }

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

        // First, install SDK dependencies from the external extension's config
        self.install_external_extension_sdk_deps(
            config,
            base_config_path,
            external_config_path,
            target,
            lock_file,
        )
        .await?;

        // Process the extension's dependencies (packages, not extension or compile dependencies)
        let sysroot = SysrootType::Extension(extension_name.to_string());

        if let Some(serde_yaml::Value::Mapping(deps_map)) = extension_config.get("dependencies") {
            if !deps_map.is_empty() {
                let mut packages = Vec::new();
                let mut package_names = Vec::new();

                // Process package dependencies (not extension or compile dependencies)
                for (package_name_val, version_spec) in deps_map {
                    // Convert package name from Value to String
                    let package_name = match package_name_val.as_str() {
                        Some(name) => name,
                        None => continue, // Skip if package name is not a string
                    };

                    // Skip non-package dependencies (extension or compile dependencies)
                    if let serde_yaml::Value::Mapping(spec_map) = version_spec {
                        // Skip extension dependencies (they have "ext" field) - handled by recursive logic
                        if spec_map.get("ext").is_some() {
                            continue;
                        }
                        // Skip compile dependencies (they have "compile" field) - SDK-compiled, not from repo
                        if spec_map.get("compile").is_some() {
                            if self.verbose {
                                print_info(
                                    &format!("Skipping compile dependency '{package_name}' (SDK-compiled, not from repo)"),
                                    OutputLevel::Normal,
                                );
                            }
                            continue;
                        }
                    }

                    // Process package dependencies only (simple string versions or version objects)
                    let config_version = match version_spec {
                        serde_yaml::Value::String(version) => version.clone(),
                        serde_yaml::Value::Mapping(spec_map) => {
                            // Only process if it has a "version" key (already checked it doesn't have ext/compile)
                            spec_map
                                .get("version")
                                .and_then(|v| v.as_str())
                                .unwrap_or("*")
                                .to_string()
                        }
                        _ => continue,
                    };

                    let package_spec = build_package_spec_with_lock(
                        lock_file,
                        target,
                        &sysroot,
                        package_name,
                        &config_version,
                    );
                    packages.push(package_spec);
                    package_names.push(package_name.to_string());
                }

                if !packages.is_empty() {
                    // Build DNF install command using the same format as regular extensions
                    let yes = if self.force { "-y" } else { "" };
                    let installroot = format!("$AVOCADO_EXT_SYSROOTS/{extension_name}");
                    let dnf_args_str = if let Some(args) = &self.dnf_args {
                        format!(" {} ", args.join(" "))
                    } else {
                        String::new()
                    };
                    let install_command = format!(
                        r#"
RPM_NO_CHROOT_FOR_SCRIPTS=1 \
AVOCADO_EXT_INSTALLROOT={} \
PATH=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin:$PATH \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot={} \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    install \
    {} \
    {}
"#,
                        installroot,
                        installroot,
                        dnf_args_str,
                        yes,
                        packages.join(" ")
                    );

                    if self.verbose {
                        print_info(
                            &format!("Running command: {install_command}"),
                            crate::utils::output::OutputLevel::Normal,
                        );
                    }

                    let run_config = crate::utils::container::RunConfig {
                        container_image: container_image.clone(),
                        target: target.to_string(),
                        command: install_command,
                        verbose: self.verbose,
                        source_environment: false, // don't source environment
                        interactive: !self.force,  // interactive if not forced
                        repo_url: repo_url.clone(),
                        repo_release: repo_release.clone(),
                        container_args: merged_container_args.clone(),
                        dnf_args: self.dnf_args.clone(),
                        disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                        ..Default::default()
                    };

                    let success = container_helper.run_in_container(run_config).await?;

                    if success {
                        print_info(
                                &format!("Installed {} package(s) for external extension '{extension_name}'.", packages.len()),
                                crate::utils::output::OutputLevel::Normal,
                            );

                        // Query installed versions and update lock file
                        if !package_names.is_empty() {
                            let installed_versions = container_helper
                                .query_installed_packages(
                                    &sysroot,
                                    &package_names,
                                    container_image,
                                    target,
                                    repo_url,
                                    repo_release,
                                    merged_container_args,
                                )
                                .await?;

                            if !installed_versions.is_empty() {
                                lock_file.update_sysroot_versions(
                                    target,
                                    &sysroot,
                                    installed_versions,
                                );
                                if self.verbose {
                                    print_info(
                                        &format!("Updated lock file with external extension '{extension_name}' package versions."),
                                        crate::utils::output::OutputLevel::Normal,
                                    );
                                }
                                // Save lock file immediately after external extension install
                                let src_dir = PathBuf::from(&self.config_path)
                                    .parent()
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "Failed to get parent directory of config file"
                                        )
                                    })?
                                    .to_path_buf();
                                lock_file.save(&src_dir)?;
                            }
                        }
                    } else {
                        return Err(anyhow::anyhow!(
                            "Failed to install package dependencies for external extension '{extension_name}'"
                        ));
                    }
                }
            }
        }

        print_info(
            &format!("Successfully installed external extension '{extension_name}' from '{external_config_path}'."),
            crate::utils::output::OutputLevel::Normal,
        );

        // Write install stamp for external extension (unless --no-stamps)
        if !self.no_stamps {
            use crate::utils::stamps::{
                generate_write_stamp_script, Stamp, StampInputs, StampOutputs,
            };

            // Compute input hash from external config
            let resolved_external_config_path =
                config.resolve_path_relative_to_src_dir(base_config_path, external_config_path);
            let external_config_content =
                std::fs::read_to_string(&resolved_external_config_path).unwrap_or_default();
            let input_hash = crate::utils::stamps::compute_hash(&external_config_content);

            let inputs = StampInputs::new(input_hash);
            let outputs = StampOutputs::default();
            let stamp = Stamp::ext_install(extension_name, target, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            // Get fresh container config values for stamp writing
            let stamp_container_image = config
                .get_sdk_image()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let stamp_repo_url = config.get_sdk_repo_url();
            let stamp_repo_release = config.get_sdk_repo_release();
            let stamp_container_args =
                config.merge_sdk_container_args(self.container_args.as_ref());

            let run_config = crate::utils::container::RunConfig {
                container_image: stamp_container_image.clone(),
                target: target.to_string(),
                command: stamp_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: stamp_repo_url,
                repo_release: stamp_repo_release,
                container_args: stamp_container_args,
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

            if self.verbose {
                crate::utils::output::print_info(
                    &format!("Wrote stamp for external extension '{extension_name}'."),
                    crate::utils::output::OutputLevel::Normal,
                );
            }
        }

        Ok(())
    }

    /// Install a versioned extension using DNF to its own sysroot
    async fn install_versioned_extension(
        &self,
        config: &Config,
        extension_name: &str,
        version: &str,
        target: &str,
        lock_file: &mut LockFile,
    ) -> Result<()> {
        // Get container configuration
        let container_helper = SdkContainer::new().verbose(self.verbose);
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Create sysroot name for versioned extension (just use extension name)
        let sysroot_name = extension_name.to_string();

        // Check if sysroot already exists
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{sysroot_name} ]");
        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.clone(),
            target: target.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };
        let sysroot_exists = container_helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            // Create the sysroot for versioned extension
            let setup_command =
                format!("mkdir -p $AVOCADO_EXT_SYSROOTS/{sysroot_name}/var/lib/extension.d");
            let run_config = crate::utils::container::RunConfig {
                container_image: container_image.clone(),
                target: target.to_string(),
                command: setup_command,
                verbose: self.verbose,
                source_environment: false,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };
            let success = container_helper.run_in_container(run_config).await?;

            if !success {
                return Err(anyhow::anyhow!(
                    "Failed to create sysroot for versioned extension '{extension_name}-{version}'"
                ));
            }

            print_info(
                &format!("Created sysroot for versioned extension '{extension_name}' version '{version}'."),
                crate::utils::output::OutputLevel::Normal,
            );
        }

        // Install the versioned extension package using DNF
        // Use the sysroot_name for lock file key (this is the extension name)
        // Note: VersionedExtension uses different RPM_CONFIGDIR than local extensions
        let sysroot = SysrootType::VersionedExtension(sysroot_name.clone());
        let package_spec =
            build_package_spec_with_lock(lock_file, target, &sysroot, extension_name, version);

        let installroot = format!("$AVOCADO_EXT_SYSROOTS/{sysroot_name}");
        let yes = if self.force { "-y" } else { "" };
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        // Always disable weak dependencies for versioned extensions since they're pre-built
        // and need to be installed exactly as specified without pulling in recommends
        let install_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_NO_SCRIPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --setopt=persistdir={installroot}/var/lib/extension.d/ \
    --installroot={installroot} \
    --enablerepo=${{AVOCADO_TARGET}}-target-ext \
    --setopt=install_weak_deps=0 \
    {dnf_args_str} \
    install \
    {yes} \
    {package_spec}
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {install_command}"),
                crate::utils::output::OutputLevel::Normal,
            );
        }

        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.clone(),
            target: target.to_string(),
            command: install_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: !self.force,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to install versioned extension '{extension_name}' version '{version}' (package: {package_spec})"
            ));
        }

        // Query installed version and update lock file
        let installed_versions = container_helper
            .query_installed_packages(
                &sysroot,
                &[extension_name.to_string()],
                container_image,
                target,
                repo_url,
                repo_release,
                merged_container_args,
            )
            .await?;

        if !installed_versions.is_empty() {
            lock_file.update_sysroot_versions(target, &sysroot, installed_versions);
            if self.verbose {
                print_info(
                    &format!(
                        "Updated lock file with versioned extension '{extension_name}' version."
                    ),
                    crate::utils::output::OutputLevel::Normal,
                );
            }
            // Save lock file immediately after versioned extension install
            let src_dir = PathBuf::from(&self.config_path)
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Failed to get parent directory of config file"))?
                .to_path_buf();
            lock_file.save(&src_dir)?;
        }

        let version_msg = if version == "*" {
            "latest version".to_string()
        } else {
            format!("version '{version}'")
        };

        print_info(
            &format!(
                "Successfully installed versioned extension '{extension_name}' {version_msg}."
            ),
            crate::utils::output::OutputLevel::Normal,
        );

        Ok(())
    }

    /// Install SDK dependencies from an external extension's config
    async fn install_external_extension_sdk_deps(
        &self,
        config: &Config,
        base_config_path: &str,
        external_config_path: &str,
        target: &str,
        lock_file: &mut LockFile,
    ) -> Result<()> {
        // Resolve the external config path
        let resolved_external_config_path =
            config.resolve_path_relative_to_src_dir(base_config_path, external_config_path);

        // Load the external config
        let external_config_content = std::fs::read_to_string(&resolved_external_config_path)
            .with_context(|| {
                format!(
                    "Failed to read external config file: {}",
                    resolved_external_config_path.display()
                )
            })?;
        let mut external_config: serde_yaml::Value = serde_yaml::from_str(&external_config_content)
            .with_context(|| {
                format!(
                    "Failed to parse external config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Apply interpolation to the external config
        // This resolves templates like {{ config.distro.version }}
        crate::utils::interpolation::interpolate_config(&mut external_config, Some(target))
            .with_context(|| {
                format!(
                    "Failed to interpolate external config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Check if the external config has SDK dependencies
        let sdk_deps = external_config
            .get("sdk")
            .and_then(|sdk| sdk.get("dependencies"))
            .and_then(|deps| deps.as_mapping());

        let Some(sdk_deps_map) = sdk_deps else {
            if self.verbose {
                print_info(
                    &format!(
                        "No SDK dependencies found in external config '{external_config_path}'"
                    ),
                    OutputLevel::Normal,
                );
            }
            return Ok(());
        };

        // Build list of SDK packages to install (using lock file for version pinning)
        let mut sdk_packages = Vec::new();
        let mut sdk_package_names = Vec::new();
        for (pkg_name_val, version_spec) in sdk_deps_map {
            let pkg_name = match pkg_name_val.as_str() {
                Some(name) => name,
                None => continue,
            };

            let config_version = match version_spec {
                serde_yaml::Value::String(version) => version.clone(),
                serde_yaml::Value::Mapping(spec_map) => spec_map
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("*")
                    .to_string(),
                _ => "*".to_string(),
            };

            let package_spec = build_package_spec_with_lock(
                lock_file,
                target,
                &SysrootType::Sdk,
                pkg_name,
                &config_version,
            );
            sdk_packages.push(package_spec);
            sdk_package_names.push(pkg_name.to_string());
        }

        if sdk_packages.is_empty() {
            return Ok(());
        }

        if self.verbose {
            print_info(
                &format!(
                    "Installing {} SDK dependencies from external config '{external_config_path}': {}",
                    sdk_packages.len(),
                    sdk_packages.join(", ")
                ),
                OutputLevel::Normal,
            );
        }

        // Get container configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Build DNF install command for SDK dependencies
        // Use the same pattern as sdk/install.rs
        let yes = if self.force { "-y" } else { "" };
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let install_command = format!(
            r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    install \
    {} \
    {}
"#,
            dnf_args_str,
            yes,
            sdk_packages.join(" ")
        );

        if self.verbose {
            print_info(
                &format!("Running SDK install command: {install_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = crate::utils::container::RunConfig {
            container_image: container_image.clone(),
            target: target.to_string(),
            command: install_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: !self.force,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to install SDK dependencies from external config '{external_config_path}'"
            ));
        }

        // Query installed versions and update lock file
        if !sdk_package_names.is_empty() {
            let installed_versions = container_helper
                .query_installed_packages(
                    &SysrootType::Sdk,
                    &sdk_package_names,
                    container_image,
                    target,
                    repo_url,
                    repo_release,
                    merged_container_args,
                )
                .await?;

            if !installed_versions.is_empty() {
                lock_file.update_sysroot_versions(target, &SysrootType::Sdk, installed_versions);
                if self.verbose {
                    print_info(
                        &format!("Updated lock file with SDK dependencies from external config '{external_config_path}'."),
                        OutputLevel::Normal,
                    );
                }
                // Save lock file immediately after external extension SDK deps install
                let src_dir = PathBuf::from(base_config_path)
                    .parent()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Failed to get parent directory of config file")
                    })?
                    .to_path_buf();
                lock_file.save(&src_dir)?;
            }
        }

        print_info(
            &format!(
                "Installed {} SDK dependencies from external config '{external_config_path}'.",
                sdk_packages.len()
            ),
            OutputLevel::Normal,
        );

        Ok(())
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
        // Test that all ExtensionDependency variants can be created and compared
        let local = ExtensionDependency::Local("test-ext".to_string());
        let external = ExtensionDependency::External {
            name: "test-ext".to_string(),
            config_path: "config.toml".to_string(),
        };
        let versioned = ExtensionDependency::Versioned {
            name: "test-ext".to_string(),
            version: "1.0.0".to_string(),
        };

        // Test that they are different
        assert_ne!(local, external);
        assert_ne!(local, versioned);
        assert_ne!(external, versioned);

        // Test that they can be cloned and hashed (for HashSet usage)
        let mut set = std::collections::HashSet::new();
        set.insert(local.clone());
        set.insert(external.clone());
        set.insert(versioned.clone());
        assert_eq!(set.len(), 3);

        // Test versioned extension with wildcard version
        let versioned_wildcard = ExtensionDependency::Versioned {
            name: "test-ext".to_string(),
            version: "*".to_string(),
        };
        assert_ne!(versioned, versioned_wildcard);
    }
}
