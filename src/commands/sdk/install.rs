//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::utils::{
    config::Config,
    container::SdkContainer,
    output::{print_info, print_success},
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
}

impl SdkInstallCommand {
    /// Create a new SdkInstallCommand instance
    pub fn new(config_path: String, verbose: bool, force: bool, target: Option<String>) -> Self {
        Self {
            config_path,
            verbose,
            force,
            target,
        }
    }

    /// Execute the sdk install command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

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

        print_info("Installing SDK dependencies.");

        // Get SDK dependencies
        let sdk_dependencies = config.get_sdk_dependencies();

        // Get compile section dependencies
        let compile_dependencies = config.get_compile_dependencies();

        // Use the container helper to run the installation
        let container_helper = SdkContainer::new().verbose(self.verbose);

        // Install SDK dependencies (into SDK)
        if let Some(dependencies) = sdk_dependencies {
            let sdk_packages = self.build_package_list(dependencies);

            if !sdk_packages.is_empty() {
                let yes = if self.force { "-y" } else { "" };

                let command = format!(
                    r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    install \
    {} \
    {}
"#,
                    yes,
                    sdk_packages.join(" ")
                );

                // Use the container helper's run_in_container method
                let install_success = container_helper
                    .run_in_container(
                        container_image,
                        &target,
                        &command,
                        self.verbose,
                        true,
                        !self.force,
                    )
                    .await?;

                if install_success {
                    print_success("Installed SDK dependencies.");
                } else {
                    return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
                }
            }
        } else {
            print_success("No dependencies configured.");
        }

        // Install compile section dependencies (into target-dev sysroot)
        if !compile_dependencies.is_empty() {
            print_info("Installing SDK compile dependencies.");
            let total = compile_dependencies.len();

            for (index, (section_name, dependencies)) in compile_dependencies.iter().enumerate() {
                let compile_packages = self.build_package_list(dependencies);

                if !compile_packages.is_empty() {
                    let installroot = "${AVOCADO_SDK_PREFIX}/target-sysroot";
                    let yes = if self.force { "-y" } else { "" };
                    let command = format!(
                        r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    --installroot {} \
    $DNF_SDK_TARGET_REPO_CONF \
    install \
    {} \
    {}
"#,
                        installroot,
                        yes,
                        compile_packages.join(" ")
                    );

                    print_info(&format!(
                        "Installing ({}/{}) {}.",
                        index + 1,
                        total,
                        section_name
                    ));

                    // Use the container helper's run_in_container method with target-dev installroot
                    let install_success = container_helper
                        .run_in_container(
                            container_image,
                            &target,
                            &command,
                            self.verbose,
                            true,
                            !self.force,
                        )
                        .await?;

                    if !install_success {
                        return Err(anyhow::anyhow!(
                            "Failed to install dependencies for compile section '{}'.",
                            section_name
                        ));
                    }
                } else {
                    print_info(&format!(
                        "({}/{}) [sdk.compile.{}.dependencies] no dependencies.",
                        index + 1,
                        total,
                        section_name
                    ));
                }
            }

            print_success("Installed SDK compile dependencies.");
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
                    packages.push(format!("{}-{}", package_name, v));
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
        let cmd = SdkInstallCommand::new("test.toml".to_string(), false, false, None);

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
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }
}
