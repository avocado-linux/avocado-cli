//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target,
};

/// Implementation of the 'sdk install' command.
pub struct SdkInstallCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

impl SdkInstallCommand {
    /// Create a new SdkInstallCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
        }
    }

    /// Execute the sdk install command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Read the config file content for extension parsing
        let config_content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("Failed to read config file {}", self.config_path))?;

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Resolve target with proper precedence
        let config_target = config.get_target();
        let target = resolve_target(self.target.as_deref(), config_target.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'."
                )
            })?;

        print_info("Installing SDK dependencies.", OutputLevel::Normal);

        // Get SDK dependencies
        let sdk_dependencies = config.get_sdk_dependencies();

        // Get extension SDK dependencies
        let extension_sdk_dependencies = config.get_extension_sdk_dependencies(&config_content)
            .with_context(|| "Failed to parse extension SDK dependencies")?;

        // Get compile section dependencies
        let compile_dependencies = config.get_compile_dependencies();

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Use the container helper to run the installation
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Install SDK dependencies (into SDK)
        let mut sdk_packages = Vec::new();

        // Add regular SDK dependencies
        if let Some(dependencies) = sdk_dependencies {
            sdk_packages.extend(self.build_package_list(dependencies));
        }

        // Add extension SDK dependencies to the package list
        for (ext_name, ext_deps) in &extension_sdk_dependencies {
            if self.verbose {
                print_info(
                    &format!("Adding SDK dependencies from extension '{}'", ext_name),
                    OutputLevel::Normal,
                );
            }
            let ext_packages = self.build_package_list(ext_deps);
            sdk_packages.extend(ext_packages);
        }

        if !sdk_packages.is_empty() {
            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            let command = format!(
                r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    {} \
    install \
    {} \
    {}
"#,
                dnf_args_str,
                yes,
                sdk_packages.join(" ")
            );

            // Use the container helper's run_in_container method
            let config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command,
                verbose: self.verbose,
                source_environment: true,
                interactive: !self.force,
                repo_url: repo_url.cloned(),
                repo_release: repo_release.cloned(),
                container_args: self.container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };
            let install_success = container_helper.run_in_container(config).await?;

            if install_success {
                print_success("Installed SDK dependencies.", OutputLevel::Normal);
            } else {
                return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
            }
        } else {
            print_success("No dependencies configured.", OutputLevel::Normal);
        }

        // Install compile section dependencies (into target-dev sysroot)
        if !compile_dependencies.is_empty() {
            print_info("Installing SDK compile dependencies.", OutputLevel::Normal);
            let total = compile_dependencies.len();

            for (index, (section_name, dependencies)) in compile_dependencies.iter().enumerate() {
                let compile_packages = self.build_package_list(dependencies);

                if !compile_packages.is_empty() {
                    let installroot = "${AVOCADO_SDK_PREFIX}/target-sysroot";
                    let yes = if self.force { "-y" } else { "" };
                    let dnf_args_str = if let Some(args) = &self.dnf_args {
                        format!(" {} ", args.join(" "))
                    } else {
                        String::new()
                    };
                    let command = format!(
                        r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    --installroot {} \
    $DNF_SDK_TARGET_REPO_CONF \
    {} \
    install \
    {} \
    {}
"#,
                        installroot,
                        dnf_args_str,
                        yes,
                        compile_packages.join(" ")
                    );

                    print_info(
                        &format!(
                            "Installing ({}/{}) compile dependencies for section '{}'",
                            index + 1,
                            total,
                            section_name
                        ),
                        OutputLevel::Normal,
                    );

                    // Use the container helper's run_in_container method with target-dev installroot
                    let config = RunConfig {
                        container_image: container_image.to_string(),
                        target: target.clone(),
                        command,
                        verbose: self.verbose,
                        source_environment: true,
                        interactive: !self.force,
                        repo_url: repo_url.cloned(),
                        repo_release: repo_release.cloned(),
                        container_args: self.container_args.clone(),
                        dnf_args: self.dnf_args.clone(),
                        ..Default::default()
                    };
                    let install_success = container_helper.run_in_container(config).await?;

                    if !install_success {
                        return Err(anyhow::anyhow!(
                            "Failed to install dependencies for compile section '{}'.",
                            section_name
                        ));
                    }
                } else {
                    print_info(
                        &format!(
                            "({}/{}) [{}] No dependencies configured.",
                            index + 1,
                            total,
                            section_name
                        ),
                        OutputLevel::Normal,
                    );
                }
            }

            print_success("Installed SDK compile dependencies.", OutputLevel::Normal);
        }

        Ok(())
    }

    /// Build a list of packages from dependencies HashMap
    fn build_package_list(&self, dependencies: &HashMap<String, toml::Value>) -> Vec<String> {
        let mut packages = Vec::new();

        for (package_name, version) in dependencies {
            match version {
                toml::Value::String(v) if v == "*" => {
                    packages.push(package_name.clone());
                }
                toml::Value::String(v) => {
                    packages.push(format!("{package_name}-{v}"));
                }
                toml::Value::Table(_) => {
                    // Handle dictionary version format like {'core2_64': '*'}
                    packages.push(package_name.clone());
                }
                _ => {
                    packages.push(package_name.clone());
                }
            }
        }

        packages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use toml::Value;

    #[test]
    fn test_build_package_list() {
        let cmd = SdkInstallCommand::new("test.toml".to_string(), false, false, None, None, None);

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));
        deps.insert("package3".to_string(), Value::Table(toml::map::Map::new()));

        let packages = cmd.build_package_list(&deps);

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"package1".to_string()));
        assert!(packages.contains(&"package2-1.0.0".to_string()));
        assert!(packages.contains(&"package3".to_string()));
    }

    #[test]
    fn test_new() {
        let cmd = SdkInstallCommand::new(
            "config.toml".to_string(),
            true,
            false,
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }
}
