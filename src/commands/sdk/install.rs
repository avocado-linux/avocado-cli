//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    stamps::{compute_sdk_input_hash, generate_write_stamp_script, Stamp, StampOutputs},
    target::validate_and_log_target,
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
    /// Disable stamp validation and writing
    pub no_stamps: bool,
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
            no_stamps: false,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Execute the sdk install command
    pub async fn execute(&self) -> Result<()> {
        // Early target validation - load basic config first
        let basic_config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let target = validate_and_log_target(self.target.as_deref(), &basic_config)?;

        // Load the composed configuration (merges external configs, applies interpolation)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;

        let config = &composed.config;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Serialize the merged config back to string for extension parsing methods
        let config_content = serde_yaml::to_string(&composed.merged_value)
            .with_context(|| "Failed to serialize composed config")?;

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        print_info("Installing SDK dependencies.", OutputLevel::Normal);

        // Get SDK dependencies from the composed config (already has external deps merged)
        let sdk_dependencies = config
            .get_sdk_dependencies_for_target(&self.config_path, &target)
            .with_context(|| "Failed to get SDK dependencies with target interpolation")?;

        // Get extension SDK dependencies (from the composed, interpolated config)
        let extension_sdk_dependencies = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                &config_content,
                Some(&self.config_path),
                Some(&target),
            )
            .with_context(|| "Failed to parse extension SDK dependencies")?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Use the container helper to run the installation
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Install SDK dependencies (into SDK)
        let mut sdk_packages = Vec::new();

        // Add regular SDK dependencies
        if let Some(ref dependencies) = sdk_dependencies {
            sdk_packages.extend(self.build_package_list(dependencies));
        }

        // Add extension SDK dependencies to the package list
        for (ext_name, ext_deps) in &extension_sdk_dependencies {
            if self.verbose {
                print_info(
                    &format!("Adding SDK dependencies from extension '{ext_name}'"),
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

            // Use the container helper's run_in_container method
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command,
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
            let install_success = container_helper.run_in_container(run_config).await?;

            if install_success {
                print_success("Installed SDK dependencies.", OutputLevel::Normal);
            } else {
                return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
            }
        } else {
            print_success("No dependencies configured.", OutputLevel::Normal);
        }

        // Install rootfs sysroot with version from distro.version
        print_info("Installing rootfs sysroot.", OutputLevel::Normal);

        let rootfs_pkg = if let Some(version) = config.get_distro_version() {
            format!("avocado-pkg-rootfs-{}", version)
        } else {
            "avocado-pkg-rootfs".to_string()
        };

        let yes = if self.force { "-y" } else { "" };
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let rootfs_command = format!(
            r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot ${{AVOCADO_PREFIX}}/rootfs \
    $DNF_SDK_TARGET_REPO_CONF \
    {} \
    install \
    {} \
    {}
"#,
            dnf_args_str, yes, rootfs_pkg
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: rootfs_command,
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

        let rootfs_success = container_helper.run_in_container(run_config).await?;

        if rootfs_success {
            print_success("Installed rootfs sysroot.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to install rootfs sysroot."));
        }

        // Install target-sysroot if there are any sdk.compile dependencies
        // This aggregates all dependencies from all compile sections (main config + external extensions)
        let compile_dependencies = config.get_compile_dependencies();
        if !compile_dependencies.is_empty() {
            // Aggregate all compile dependencies into a single list
            let mut all_compile_packages: Vec<String> = Vec::new();
            for dependencies in compile_dependencies.values() {
                let packages = self.build_package_list(dependencies);
                all_compile_packages.extend(packages);
            }

            // Deduplicate packages
            all_compile_packages.sort();
            all_compile_packages.dedup();

            print_info(
                &format!(
                    "Installing target-sysroot with {} compile dependencies.",
                    all_compile_packages.len()
                ),
                OutputLevel::Normal,
            );

            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            // Build the target-sysroot package spec with version from distro.version
            let target_sysroot_pkg = if let Some(version) = config.get_distro_version() {
                format!("avocado-sdk-target-sysroot-{}", version)
            } else {
                "avocado-sdk-target-sysroot".to_string()
            };

            // Install the target-sysroot with packagegroup-core-standalone-sdk-target plus compile deps
            let command = format!(
                r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot ${{AVOCADO_SDK_PREFIX}}/target-sysroot \
    --setopt=install_weak_deps=0 \
    --nodocs \
    $DNF_SDK_TARGET_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    install \
    {} \
    {} \
    {}
"#,
                dnf_args_str,
                yes,
                target_sysroot_pkg,
                all_compile_packages.join(" ")
            );

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command,
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

            let install_success = container_helper.run_in_container(run_config).await?;

            if install_success {
                print_success(
                    "Installed target-sysroot with compile dependencies.",
                    OutputLevel::Normal,
                );
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to install target-sysroot with compile dependencies."
                ));
            }
        }

        // Write SDK install stamp (unless --no-stamps)
        if !self.no_stamps {
            let inputs = compute_sdk_input_hash(&composed.merged_value)?;
            let outputs = StampOutputs::default();
            let stamp = Stamp::sdk_install(&target, inputs, outputs);
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
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

            if self.verbose {
                print_info("Wrote SDK install stamp.", OutputLevel::Normal);
            }
        }

        Ok(())
    }

    /// Build a list of packages from dependencies HashMap
    fn build_package_list(&self, dependencies: &HashMap<String, serde_yaml::Value>) -> Vec<String> {
        let mut packages = Vec::new();

        for (package_name, version) in dependencies {
            match version {
                serde_yaml::Value::String(v) if v == "*" => {
                    packages.push(package_name.clone());
                }
                serde_yaml::Value::String(v) => {
                    packages.push(format!("{package_name}-{v}"));
                }
                serde_yaml::Value::Mapping(_) => {
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
    use serde_yaml::Value;
    use std::collections::HashMap;

    #[test]
    fn test_build_package_list() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));
        deps.insert(
            "package3".to_string(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );

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
