use anyhow::{Context, Result};

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_debug, print_error, print_info, print_success, OutputLevel};
use crate::utils::stamps::{
    compute_ext_input_hash, generate_write_stamp_script, Stamp, StampOutputs,
};
use crate::utils::target::resolve_target_required;

pub struct ExtInstallCommand {
    extension: Option<String>,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
}

impl ExtInstallCommand {
    pub fn new(
        extension: Option<String>,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            force,
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
        // Load the composed configuration (merges external configs, applies interpolation)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;

        let config = &composed.config;
        let parsed = &composed.merged_value;

        // Merge container args from config and CLI (similar to SDK commands)
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Determine which extensions to install (with their locations)
        let extensions_to_install: Vec<(String, ExtensionLocation)> = if let Some(extension_name) =
            &self.extension
        {
            // Single extension specified - use comprehensive lookup
            match config.find_extension_in_dependency_tree(
                &self.config_path,
                extension_name,
                &target,
            )? {
                Some(location) => {
                    if self.verbose {
                        match &location {
                            ExtensionLocation::Local { name, config_path } => {
                                print_info(
                                    &format!(
                                        "Found local extension '{name}' in config '{config_path}'"
                                    ),
                                    OutputLevel::Normal,
                                );
                            }
                            ExtensionLocation::External { name, config_path } => {
                                print_info(
                                        &format!(
                                            "Found external extension '{name}' in config '{config_path}'"
                                        ),
                                        OutputLevel::Normal,
                                    );
                            }
                        }
                    }
                    vec![(extension_name.clone(), location)]
                }
                None => {
                    print_error(
                        &format!("Extension '{extension_name}' not found in configuration."),
                        OutputLevel::Normal,
                    );
                    return Ok(());
                }
            }
        } else {
            // No extension specified - install all local extensions
            match parsed.get("ext") {
                Some(ext_section) => match ext_section.as_mapping() {
                    Some(table) => table
                        .keys()
                        .filter_map(|k| {
                            k.as_str().map(|s| {
                                (
                                    s.to_string(),
                                    ExtensionLocation::Local {
                                        name: s.to_string(),
                                        config_path: self.config_path.clone(),
                                    },
                                )
                            })
                        })
                        .collect(),
                    None => vec![],
                },
                None => {
                    print_info("No extensions found in configuration.", OutputLevel::Normal);
                    return Ok(());
                }
            }
        };

        if extensions_to_install.is_empty() {
            print_info("No extensions found in configuration.", OutputLevel::Normal);
            return Ok(());
        }

        let ext_names: Vec<&str> = extensions_to_install
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        print_info(
            &format!(
                "Installing {} extension(s): {}.",
                extensions_to_install.len(),
                ext_names.join(", ")
            ),
            OutputLevel::Normal,
        );

        // Get the SDK image from interpolated config
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
        })?;

        // Use resolved target (from CLI/env) if available, otherwise fall back to config
        let _config_target = parsed
            .get("runtime")
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
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Use the container helper to run the setup commands
        let container_helper = SdkContainer::new();
        let total = extensions_to_install.len();

        // Install each extension
        for (index, (ext_name, ext_location)) in extensions_to_install.iter().enumerate() {
            if self.verbose {
                print_debug(
                    &format!("Installing ({}/{}) {}.", index + 1, total, ext_name),
                    OutputLevel::Normal,
                );
            }

            // Get the config path where this extension is actually defined
            let ext_config_path = match ext_location {
                ExtensionLocation::Local { config_path, .. } => config_path.clone(),
                ExtensionLocation::External { config_path, .. } => {
                    // Resolve relative path against main config directory
                    let main_config_dir = std::path::Path::new(&self.config_path)
                        .parent()
                        .unwrap_or(std::path::Path::new("."));
                    main_config_dir
                        .join(config_path)
                        .to_string_lossy()
                        .to_string()
                }
            };

            if !self
                .install_single_extension(
                    config,
                    ext_name,
                    &ext_config_path,
                    &container_helper,
                    container_image,
                    &target,
                    repo_url.as_ref(),
                    repo_release.as_ref(),
                    &merged_container_args,
                    config.get_sdk_disable_weak_dependencies(),
                )
                .await?
            {
                return Err(anyhow::anyhow!("Failed to install extension '{ext_name}'"));
            }

            // Write extension install stamp (unless --no-stamps)
            if !self.no_stamps {
                let inputs = compute_ext_input_hash(parsed, ext_name)?;
                let outputs = StampOutputs::default();
                let stamp = Stamp::ext_install(ext_name, &target, inputs, outputs);
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
                    ..Default::default()
                };

                container_helper.run_in_container(run_config).await?;

                if self.verbose {
                    print_info(
                        &format!("Wrote install stamp for extension '{ext_name}'."),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        if !extensions_to_install.is_empty() {
            print_success(
                &format!("Installed {} extension(s).", extensions_to_install.len()),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_single_extension(
        &self,
        config: &Config,
        extension: &str,
        ext_config_path: &str,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
        disable_weak_dependencies: bool,
    ) -> Result<bool> {
        // Create the commands to check and set up the directory structure
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{extension} ]");
        let setup_command = format!(
            "mkdir -p $AVOCADO_EXT_SYSROOTS/{extension}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm $AVOCADO_EXT_SYSROOTS/{extension}/var/lib"
        );

        // First check if the sysroot already exists
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let sysroot_exists = container_helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            // Create the sysroot
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: setup_command,
                verbose: self.verbose,
                source_environment: false, // don't source environment
                interactive: false,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };
            let success = container_helper.run_in_container(run_config).await?;

            if success {
                print_success(
                    &format!("Created sysroot for extension '{extension}'."),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!("Failed to create sysroot for extension '{extension}'."),
                    OutputLevel::Normal,
                );
                return Ok(false);
            }
        }

        // Get merged extension configuration from the correct config file
        // This properly handles both local and external extensions
        let ext_config = config.get_merged_ext_config(extension, target, ext_config_path)?;

        // Install dependencies if they exist
        let dependencies = ext_config.as_ref().and_then(|ec| ec.get("dependencies"));

        if let Some(serde_yaml::Value::Mapping(deps_map)) = dependencies {
            // Build list of packages to install and handle extension dependencies
            let mut packages = Vec::new();
            let mut extension_dependencies = Vec::new();

            for (package_name_val, version_spec) in deps_map {
                // Convert package name from Value to String
                let package_name = match package_name_val.as_str() {
                    Some(name) => name,
                    None => continue, // Skip if package name is not a string
                };

                // Handle different dependency types based on value format
                match version_spec {
                    // Simple string version: "package: version" or "package: '*'"
                    // These are always package repository dependencies
                    serde_yaml::Value::String(version) => {
                        if version == "*" {
                            packages.push(package_name.to_string());
                        } else {
                            packages.push(format!("{package_name}-{version}"));
                        }
                    }
                    // Object/mapping value: need to check what type of dependency
                    serde_yaml::Value::Mapping(spec_map) => {
                        // Skip compile dependencies - these are SDK-compiled, not from repo
                        // Format: { compile: "section-name", install: "script.sh" }
                        if spec_map.get("compile").is_some() {
                            if self.verbose {
                                print_debug(
                                    &format!("Skipping compile dependency '{package_name}' (SDK-compiled, not from repo)"),
                                    OutputLevel::Normal,
                                );
                            }
                            continue;
                        }

                        // Check for extension dependency
                        // Format: { ext: "extension-name" } or { ext: "name", config: "path" } or { ext: "name", vsn: "version" }
                        if let Some(ext_name) = spec_map.get("ext").and_then(|v| v.as_str()) {
                            // Check if this is a versioned extension (has vsn field)
                            if let Some(version) = spec_map.get("vsn").and_then(|v| v.as_str()) {
                                extension_dependencies
                                    .push((ext_name.to_string(), Some(version.to_string())));
                                if self.verbose {
                                    print_info(
                                        &format!("Found versioned extension dependency: {ext_name} version {version}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            }
                            // Check if this is an external extension (has config field)
                            else if let Some(config_path) =
                                spec_map.get("config").and_then(|v| v.as_str())
                            {
                                extension_dependencies.push((ext_name.to_string(), None));
                                if self.verbose {
                                    print_info(
                                        &format!("Found external extension dependency: {ext_name} from config {config_path}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            } else {
                                // Local extension
                                extension_dependencies.push((ext_name.to_string(), None));
                                if self.verbose {
                                    print_info(
                                        &format!("Found local extension dependency: {ext_name}"),
                                        OutputLevel::Normal,
                                    );
                                }
                            }
                            continue; // Skip adding to packages list
                        }

                        // Check for explicit version in object format
                        // Format: { version: "1.0.0" }
                        if let Some(serde_yaml::Value::String(version)) = spec_map.get("version") {
                            if version == "*" {
                                packages.push(package_name.to_string());
                            } else {
                                packages.push(format!("{package_name}-{version}"));
                            }
                        }
                        // If it's a mapping without compile, ext, or version keys, skip it
                        // (unknown format)
                    }
                    _ => {}
                }
            }

            // Handle extension dependencies first
            if !extension_dependencies.is_empty() {
                if self.verbose {
                    print_info(
                        &format!("Extension '{extension}' has {} extension dependencies that need to be installed first", extension_dependencies.len()),
                        OutputLevel::Normal,
                    );
                }

                // Note: Extension dependencies should be handled by the main install command
                // or by recursive calls to ExtInstallCommand for each dependency.
                // For now, we'll log them but not install them directly to avoid circular dependencies.
                for (ext_name, version) in &extension_dependencies {
                    if let Some(ver) = version {
                        print_info(
                            &format!("Extension dependency: {ext_name} (version {ver}) - should be installed via main install command"),
                            OutputLevel::Normal,
                        );
                    } else {
                        print_info(
                            &format!("Extension dependency: {ext_name} - should be installed via main install command"),
                            OutputLevel::Normal,
                        );
                    }
                }
            }

            if !packages.is_empty() {
                // Build DNF install command
                let yes = if self.force { "-y" } else { "" };
                let installroot = format!("$AVOCADO_EXT_SYSROOTS/{extension}");
                let dnf_args_str = if let Some(args) = &self.dnf_args {
                    format!(" {} ", args.join(" "))
                } else {
                    String::new()
                };
                let command = format!(
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
                    print_info(&format!("Running command: {command}"), OutputLevel::Normal);
                }

                // Run the DNF install command
                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target.to_string(),
                    command,
                    verbose: self.verbose,
                    source_environment: false, // don't source environment
                    interactive: !self.force,  // interactive if not forced
                    repo_url: repo_url.cloned(),
                    repo_release: repo_release.cloned(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    disable_weak_dependencies,
                    ..Default::default()
                };
                let install_success = container_helper.run_in_container(run_config).await?;

                if !install_success {
                    print_error(
                        &format!("Failed to install dependencies for extension '{extension}'."),
                        OutputLevel::Normal,
                    );
                    return Ok(false);
                }
            } else if self.verbose {
                print_debug(
                    &format!("No valid dependencies found for extension '{extension}'."),
                    OutputLevel::Normal,
                );
            }
        } else if self.verbose {
            print_debug(
                &format!("No dependencies defined for extension '{extension}'."),
                OutputLevel::Normal,
            );
        }

        Ok(true)
    }
}
