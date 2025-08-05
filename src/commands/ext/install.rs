use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_debug, print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtInstallCommand {
    extension: Option<String>,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
}

impl ExtInstallCommand {
    pub fn new(
        extension: Option<String>,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            force,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load the configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if ext section exists
        let ext_section = match parsed.get("ext") {
            Some(ext) => ext,
            None => {
                if self.extension.is_some() {
                    print_error(
                        &format!(
                            "Extension '{}' not found in configuration.",
                            self.extension.as_ref().unwrap()
                        ),
                        OutputLevel::Normal,
                    );
                    return Ok(());
                } else {
                    print_info("No extensions found in configuration.", OutputLevel::Normal);
                    return Ok(());
                }
            }
        };

        // Determine which extensions to install
        let extensions_to_install = if let Some(extension_name) = &self.extension {
            // Single extension specified
            if !ext_section.as_table().unwrap().contains_key(extension_name) {
                print_error(
                    &format!("Extension '{extension_name}' not found in configuration."),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
            vec![extension_name.clone()]
        } else {
            // No extension specified - install all extensions
            match ext_section.as_table() {
                Some(table) => table.keys().cloned().collect(),
                None => vec![],
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
        let config_target = parsed
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
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());
        let target = resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })?;

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

    async fn install_single_extension(
        &self,
        config: &toml::Value,
        extension: &str,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
    ) -> Result<bool> {
        // Create the commands to check and set up the directory structure
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{extension} ]");
        let setup_command = format!(
            "mkdir -p ${{AVOCADO_EXT_SYSROOTS}}/{extension}/var/lib && cp -rf ${{AVOCADO_PREFIX}}/rootfs/var/lib/rpm ${{AVOCADO_EXT_SYSROOTS}}/{extension}/var/lib"
        );

        // First check if the sysroot already exists
        let sysroot_exists = container_helper
            .run_in_container(
                container_image,
                target,
                &check_command,
                self.verbose,
                false, // don't source environment
                false, // not interactive
            )
            .await?;

        if !sysroot_exists {
            // Create the sysroot
            let success = container_helper
                .run_in_container(
                    container_image,
                    target,
                    &setup_command,
                    self.verbose,
                    false, // don't source environment
                    false, // not interactive
                )
                .await?;

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
        let extension_config = &config["ext"][extension];
        let dependencies = extension_config.get("dependencies");

        if let Some(toml::Value::Table(deps_map)) = dependencies {
            // Build list of packages to install
            let mut packages = Vec::new();
            for (package_name, version_spec) in deps_map {
                // Skip compile dependencies (identified by dict value with 'compile' key)
                if let toml::Value::Table(spec_map) = version_spec {
                    if spec_map.contains_key("compile") {
                        continue;
                    }
                }

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

            if !packages.is_empty() {
                // Build DNF install command
                let yes = if self.force { "-y" } else { "" };
                let installroot = format!("${{AVOCADO_EXT_SYSROOTS}}/{extension}");
                let command = format!(
                    r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={} \
    install \
    {} \
    {}
"#,
                    installroot,
                    yes,
                    packages.join(" ")
                );

                if self.verbose {
                    print_info(&format!("Running command: {command}"), OutputLevel::Normal);
                }

                // Run the DNF install command
                let install_success = container_helper
                    .run_in_container(
                        container_image,
                        target,
                        &command,
                        self.verbose,
                        false,       // don't source environment
                        !self.force, // interactive if not forced
                    )
                    .await?;

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
