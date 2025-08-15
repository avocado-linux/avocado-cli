use anyhow::Result;

use crate::utils::config::Config;
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
                    repo_url,
                    repo_release,
                    &merged_container_args,
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
                let installroot = format!("$AVOCADO_EXT_SYSROOTS/{extension}");
                let dnf_args_str = if let Some(args) = &self.dnf_args {
                    format!(" {} ", args.join(" "))
                } else {
                    String::new()
                };
                let command = format!(
                    r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={} \
    {} \
    install \
    {} \
    {}
"#,
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
