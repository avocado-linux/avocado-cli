use anyhow::Result;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_debug, print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtInstallCommand {
    extension: Option<String>,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
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
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load the configuration and parse raw TOML
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Merge container args from config and CLI (similar to SDK commands)
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Determine which extensions to install
        let extensions_to_install = if let Some(extension_name) = &self.extension {
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
                                    &format!("Found external extension '{name}' in config '{config_path}'"),
                                    OutputLevel::Normal,
                                );
                            }
                        }
                    }
                    vec![extension_name.clone()]
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
                Some(ext_section) => match ext_section.as_table() {
                    Some(table) => table.keys().cloned().collect(),
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

        print_info(
            &format!(
                "Installing {} extension(s): {}.",
                extensions_to_install.len(),
                extensions_to_install.join(", ")
            ),
            OutputLevel::Normal,
        );

        // Get the SDK image and target from configuration
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })?;

        // Use resolved target (from CLI/env) if available, otherwise fall back to config
        let _config_target = parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_table())
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
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Use the container helper to run the setup commands
        let container_helper = SdkContainer::new();
        let total = extensions_to_install.len();

        // Install each extension
        for (index, ext_name) in extensions_to_install.iter().enumerate() {
            if self.verbose {
                print_debug(
                    &format!("Installing ({}/{}) {}.", index + 1, total, ext_name),
                    OutputLevel::Normal,
                );
            }

            if !self
                .install_single_extension(
                    &parsed,
                    ext_name,
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
                return Err(anyhow::anyhow!(
                    "Failed to install extension '{}'",
                    ext_name
                ));
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
        config: &toml::Value,
        extension: &str,
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

        // Install dependencies if they exist
        // Check if extension exists in local config (versioned extensions may not be local)
        let dependencies = config
            .get("ext")
            .and_then(|ext| ext.as_table())
            .and_then(|ext_table| ext_table.get(extension))
            .and_then(|extension_config| extension_config.get("dependencies"));

        if let Some(toml::Value::Table(deps_map)) = dependencies {
            // Build list of packages to install and handle extension dependencies
            let mut packages = Vec::new();
            let mut extension_dependencies = Vec::new();

            for (package_name, version_spec) in deps_map {
                // Handle extension dependencies
                if let toml::Value::Table(spec_map) = version_spec {
                    // Skip compile dependencies (identified by dict value with 'compile' key)
                    if spec_map.contains_key("compile") {
                        continue;
                    }

                    // Check for extension dependency
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
                }

                // Handle regular package dependencies
                match version_spec {
                    toml::Value::String(version) => {
                        if version == "*" {
                            packages.push(package_name.clone());
                        } else {
                            packages.push(format!("{package_name}-{version}"));
                        }
                    }
                    toml::Value::Table(spec_map) => {
                        if let Some(toml::Value::String(version)) = spec_map.get("version") {
                            if version == "*" {
                                packages.push(package_name.clone());
                            } else {
                                packages.push(format!("{package_name}-{version}"));
                            }
                        }
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
