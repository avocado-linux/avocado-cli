use anyhow::{Context, Result};

use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_debug, print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct RuntimeInstallCommand {
    runtime: Option<String>,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeInstallCommand {
    pub fn new(
        runtime: Option<String>,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime,
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
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Merge container args from config and CLI (similar to SDK commands)
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Check if runtime section exists
        let runtime_section = match parsed.get("runtime") {
            Some(runtime) => runtime,
            None => {
                if self.runtime.is_some() {
                    print_error(
                        &format!(
                            "Runtime '{}' not found in configuration.",
                            self.runtime.as_ref().unwrap()
                        ),
                        OutputLevel::Normal,
                    );
                    return Ok(());
                } else {
                    print_info("No runtimes found in configuration.", OutputLevel::Normal);
                    return Ok(());
                }
            }
        };

        // Determine which runtimes to install dependencies for
        let runtimes_to_install = if let Some(runtime_name) = &self.runtime {
            // Single runtime specified
            if !runtime_section
                .as_mapping()
                .unwrap()
                .contains_key(runtime_name)
            {
                print_error(
                    &format!("Runtime '{runtime_name}' not found in configuration."),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
            vec![runtime_name.clone()]
        } else {
            // No runtime specified - install for all runtimes
            match runtime_section.as_mapping() {
                Some(table) => table
                    .keys()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect(),
                None => vec![],
            }
        };

        if runtimes_to_install.is_empty() {
            print_info(
                "No runtimes to install dependencies for.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        // Initialize container helper
        let container_helper = SdkContainer::new();

        // Install dependencies for each runtime
        for runtime_name in &runtimes_to_install {
            print_info(
                &format!("Installing dependencies for runtime '{runtime_name}'"),
                OutputLevel::Normal,
            );

            let success = self
                .install_single_runtime(
                    &parsed,
                    &config,
                    runtime_name,
                    &container_helper,
                    container_image,
                    repo_url.as_ref(),
                    repo_release.as_ref(),
                    &merged_container_args,
                )
                .await?;

            if !success {
                print_error(
                    &format!("Failed to install dependencies for runtime '{runtime_name}'"),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        }

        print_success(
            &format!(
                "Successfully installed dependencies for {} runtime(s)",
                runtimes_to_install.len()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_single_runtime(
        &self,
        config_toml: &serde_yaml::Value,
        config: &crate::utils::config::Config,
        runtime: &str,
        container_helper: &SdkContainer,
        container_image: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<bool> {
        // Get runtime configuration
        let runtime_config = config_toml["runtime"][runtime].clone();

        // Get target from runtime config
        let _config_target = runtime_config
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // Create the commands to check and set up the runtime installroot
        let installroot_path = format!("$AVOCADO_PREFIX/runtimes/{runtime}");
        let check_command = format!("[ -d {installroot_path} ]");
        let setup_command = format!(
            "mkdir -p {installroot_path}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm {installroot_path}/var/lib"
        );

        // First check if the installroot already exists
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
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
        let installroot_exists = container_helper.run_in_container(run_config).await?;

        if !installroot_exists {
            // Create the installroot
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
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
                    &format!("Created installroot for runtime '{runtime}'."),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!("Failed to create installroot for runtime '{runtime}'."),
                    OutputLevel::Normal,
                );
                return Ok(false);
            }
        }

        // Install dependencies if they exist (using merged config to include target-specific dependencies)
        let merged_runtime =
            config.get_merged_runtime_config(runtime, &target_arch, &self.config_path)?;
        let dependencies = merged_runtime
            .as_ref()
            .and_then(|merged| merged.get("dependencies"));

        if let Some(serde_yaml::Value::Mapping(deps_map)) = dependencies {
            // Build list of packages to install (excluding extension references)
            let mut packages = Vec::new();
            for (package_name_val, version_spec) in deps_map {
                // Convert package name from Value to String
                let package_name = match package_name_val.as_str() {
                    Some(name) => name,
                    None => continue, // Skip if package name is not a string
                };

                // Skip extension dependencies (identified by 'ext' key)
                // Note: Extension dependencies are handled by the main install command,
                // not by individual runtime install
                if let serde_yaml::Value::Mapping(spec_map) = version_spec {
                    if spec_map.contains_key(serde_yaml::Value::String("ext".to_string())) {
                        if self.verbose {
                            let dep_type = if spec_map
                                .contains_key(serde_yaml::Value::String("vsn".to_string()))
                            {
                                "versioned extension"
                            } else if spec_map
                                .contains_key(serde_yaml::Value::String("config".to_string()))
                            {
                                "external extension"
                            } else {
                                "local extension"
                            };
                            print_debug(
                                &format!("Skipping {dep_type} dependency '{package_name}' (handled by main install command)"),
                                OutputLevel::Normal,
                            );
                        }
                        continue;
                    }
                }

                let package_name_and_version = if version_spec.as_str().is_some() {
                    let version = version_spec.as_str().unwrap();
                    if version == "*" {
                        package_name.to_string()
                    } else {
                        format!("{package_name}-{version}")
                    }
                } else if let serde_yaml::Value::Mapping(spec_map) = version_spec {
                    if let Some(version) = spec_map.get("version") {
                        let version = version.as_str().unwrap_or("*");
                        if version == "*" {
                            package_name.to_string()
                        } else {
                            format!("{package_name}-{version}")
                        }
                    } else {
                        package_name.to_string()
                    }
                } else {
                    package_name.to_string()
                };

                packages.push(package_name_and_version);
            }

            if !packages.is_empty() {
                print_info(
                    &format!(
                        "Installing {} package(s) for runtime '{runtime}'",
                        packages.len()
                    ),
                    OutputLevel::Normal,
                );

                let yes = if self.force { "-y" } else { "" };
                let dnf_args_str = if let Some(args) = &self.dnf_args {
                    format!(" {} ", args.join(" "))
                } else {
                    String::new()
                };

                let dnf_command = format!(
                    r#"\
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_NO_SCRIPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={installroot_path} \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    install \
    {} \
    {}"#,
                    dnf_args_str,
                    yes,
                    packages.join(" ")
                );

                if self.verbose {
                    print_debug(
                        &format!("Installing packages: {}", packages.join(", ")),
                        OutputLevel::Normal,
                    );
                }

                let run_config = RunConfig {
                    container_image: container_image.to_string(),
                    target: target_arch.clone(),
                    command: dnf_command,
                    verbose: self.verbose,
                    source_environment: true, // need environment for DNF
                    interactive: !self.force,
                    repo_url: repo_url.cloned(),
                    repo_release: repo_release.cloned(),
                    container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                    ..Default::default()
                };
                let success = container_helper.run_in_container(run_config).await?;

                if !success {
                    print_error(
                        &format!("Failed to install packages for runtime '{runtime}'"),
                        OutputLevel::Normal,
                    );
                    return Ok(false);
                }

                print_success(
                    &format!("Successfully installed packages for runtime '{runtime}'"),
                    OutputLevel::Normal,
                );
            } else {
                print_info(
                    &format!("No packages to install for runtime '{runtime}'"),
                    OutputLevel::Normal,
                );
            }
        } else {
            print_info(
                &format!("No dependencies configured for runtime '{runtime}'"),
                OutputLevel::Normal,
            );
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config_file(temp_dir: &TempDir, content: &str) -> String {
        let config_path = temp_dir.path().join("avocado.yaml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_new() {
        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            "avocado.yaml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_all_runtimes() {
        let cmd = RuntimeInstallCommand::new(
            None,
            "avocado.yaml".to_string(),
            true,
            true,
            None,
            Some(vec!["--arg1".to_string()]),
            Some(vec!["--dnf-arg".to_string()]),
        );

        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert!(cmd.force);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, Some(vec!["--arg1".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--dnf-arg".to_string()]));
    }

    #[tokio::test]
    async fn test_execute_no_runtime_section() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Should handle missing runtime section gracefully
        let result = cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_runtime_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  image: "test-image"

runtime:
  other-runtime:
    target: "x86_64"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Should handle missing specific runtime gracefully
        let result = cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_no_sdk_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
      gcc: "11.0"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Should fail without SDK configuration
        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No SDK container image specified in configuration"));
    }

    #[tokio::test]
    async fn test_execute_no_container_image() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
sdk:
  # Missing image field

runtime:
  test-runtime:
    target: "x86_64"
    dependencies:
      gcc: "11.0"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Should fail without container image
        let result = cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No SDK container image specified"));
    }

    #[test]
    fn test_runtime_install_with_package_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "11.0"
python3 = "*"
curl = { version = "7.0" }
app-ext = { ext = "my-extension" }

[ext.my-extension]
version = "2.0.0"
types = ["sysext"]
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // This would test the actual installation logic, but since we can't run containers in tests,
        // we'll just verify the command was created correctly
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_runtime_install_all_runtimes() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.runtime1]
target = "x86_64"

[runtime.runtime1.dependencies]
gcc = "11.0"

[runtime.runtime2]
target = "aarch64"

[runtime.runtime2.dependencies]
python3 = "*"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            None, // Install for all runtimes
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // This would install dependencies for both runtime1 and runtime2
        assert_eq!(cmd.runtime, None);
    }

    #[test]
    fn test_runtime_install_no_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"
# No dependencies section
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            false,
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        // Should handle runtime with no dependencies gracefully
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
    }

    #[test]
    fn test_runtime_install_with_container_and_dnf_args() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "*"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("test-runtime".to_string()),
            config_path,
            true,
            true,
            Some("x86_64".to_string()),
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
        assert!(cmd.verbose);
        assert!(cmd.force);
    }

    #[test]
    fn test_runtime_install_with_target_specific_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.dev]
# Base dependencies
[runtime.dev.dependencies]
avocado-img-bootfiles = "*"
avocado-img-rootfs = "*"
avocado-img-initramfs = "*"

# Target-specific dependencies
[runtime.dev.jetson-orin-nano-devkit.dependencies]
avocado-img-tegraflash = "*"
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);

        let cmd = RuntimeInstallCommand::new(
            Some("dev".to_string()),
            config_path,
            false,
            false,
            Some("jetson-orin-nano-devkit".to_string()),
            None,
            None,
        );

        // Test that the command is created correctly
        assert_eq!(cmd.runtime, Some("dev".to_string()));
        assert_eq!(cmd.target, Some("jetson-orin-nano-devkit".to_string()));

        // Note: The actual dependency resolution is tested by the merged config functionality
        // which is already covered in the config module tests
    }
}
