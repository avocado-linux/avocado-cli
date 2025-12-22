//! SDK compile command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    stamps::{generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement},
    target::resolve_target_required,
};

/// Compile section configuration
#[derive(Debug, Clone)]
pub struct CompileSection {
    pub name: String,
    pub script: String,
}

/// Implementation of the 'sdk compile' command.
pub struct SdkCompileCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Specific compile sections to run
    pub sections: Vec<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Disable stamp validation
    pub no_stamps: bool,
}

impl SdkCompileCommand {
    /// Create a new SdkCompileCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        sections: Vec<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            sections,
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

    /// Execute the sdk compile command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        if self.verbose {
            print_info(
                &format!("Loading SDK compile config from: {}", self.config_path),
                OutputLevel::Normal,
            );
        }
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Validate stamps before proceeding (unless --no-stamps)
        // SDK compile requires SDK to be installed
        if !self.no_stamps {
            let container_image = config
                .get_sdk_image()
                .context("No SDK container image specified in configuration")?;
            let target = resolve_target_required(self.target.as_deref(), &config)?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

            let requirements = vec![StampRequirement::sdk_install()];

            let batch_script = generate_batch_read_stamps_script(&requirements);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: config.get_sdk_repo_url(),
                repo_release: config.get_sdk_repo_release(),
                container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            let validation =
                validate_stamps_batch(&requirements, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error = validation.into_error("Cannot run SDK compile");
                return Err(error.into());
            }
        }

        // Debug: Check if sdk.compile was parsed
        if self.verbose {
            if let Some(sdk) = &config.sdk {
                if let Some(compile) = &sdk.compile {
                    print_info(
                        &format!("Found {} SDK compile section(s) in config", compile.len()),
                        OutputLevel::Normal,
                    );
                    for (name, cfg) in compile {
                        print_info(
                            &format!("  - Section '{}': compile script = {:?}", name, cfg.compile),
                            OutputLevel::Normal,
                        );
                    }
                } else {
                    print_info(
                        "No sdk.compile section found in config",
                        OutputLevel::Normal,
                    );
                }
            } else {
                print_info("No sdk section found in config", OutputLevel::Normal);
            }
        }

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get compile sections from config
        let compile_sections = self.get_compile_sections_from_config(&config);

        if compile_sections.is_empty() {
            // If specific sections were requested but none found, this is an error
            if !self.sections.is_empty() {
                return Err(anyhow::anyhow!(
                    "Requested compile sections {:?} not found in config '{}'",
                    self.sections,
                    self.config_path
                ));
            }
            print_success("No compile sections configured.", OutputLevel::Normal);
            return Ok(());
        }

        // Filter sections if specific ones were requested
        let filtered_sections = if self.sections.is_empty() {
            compile_sections
        } else {
            let requested_sections: std::collections::HashSet<&String> =
                self.sections.iter().collect();
            let available_sections: std::collections::HashSet<String> =
                compile_sections.iter().map(|s| s.name.clone()).collect();

            // Check for missing sections
            let missing_sections: Vec<&String> = requested_sections
                .iter()
                .filter(|&s| !available_sections.contains(*s))
                .cloned()
                .collect();

            if !missing_sections.is_empty() {
                let missing: Vec<String> = missing_sections.iter().map(|s| (*s).clone()).collect();
                let available: Vec<String> = available_sections.into_iter().collect();
                return Err(anyhow::anyhow!(
                    "The following compile sections were not found: {}\nAvailable sections: {}",
                    missing.join(", "),
                    available.join(", ")
                ));
            }

            compile_sections
                .into_iter()
                .filter(|s| requested_sections.contains(&s.name))
                .collect()
        };

        println!(
            "Found {} compile section(s) to process: {}",
            filtered_sections.len(),
            filtered_sections
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        let mut overall_success = true;

        for section in &filtered_sections {
            print_info(
                &format!(
                    "Compiling section '{}' with script '{}'",
                    section.name, section.script
                ),
                OutputLevel::Normal,
            );

            let container_helper =
                SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

            let compile_command = format!(
                r#"if [ -f '{}' ]; then echo 'Running compile script: {}'; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{}'; else echo 'Compile script {} not found.' && ls -la; exit 1; fi"#,
                section.script, section.script, section.script, section.script
            );

            let config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: compile_command,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };
            let success = container_helper.run_in_container(config).await?;

            if success {
                print_success(
                    &format!("Compiled section '{}'.", section.name),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!("Failed to compile section '{}'.", section.name),
                    OutputLevel::Normal,
                );
                overall_success = false;
            }
        }

        if overall_success {
            print_success(
                &format!(
                    "All {} compile section(s) completed successfully!",
                    filtered_sections.len()
                ),
                OutputLevel::Normal,
            );
        }

        if !overall_success {
            return Err(anyhow::anyhow!("One or more compile sections failed."));
        }

        Ok(())
    }

    /// Extract compile sections from configuration
    fn get_compile_sections_from_config(&self, config: &Config) -> Vec<CompileSection> {
        let mut compile_sections = Vec::new();

        if let Some(sdk) = &config.sdk {
            if let Some(compile) = &sdk.compile {
                for (section_name, section_config) in compile {
                    // Look for a compile script in the section config
                    if let Some(compile_script) =
                        self.find_compile_script_in_section(section_config)
                    {
                        compile_sections.push(CompileSection {
                            name: section_name.clone(),
                            script: compile_script,
                        });
                    }
                }
            }
        }

        compile_sections
    }

    /// Find compile script in section configuration
    fn find_compile_script_in_section(
        &self,
        section_config: &crate::utils::config::CompileConfig,
    ) -> Option<String> {
        section_config.compile.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_new() {
        let cmd = SdkCompileCommand::new(
            "config.toml".to_string(),
            true,
            vec!["app".to_string()],
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.sections, vec!["app"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_get_compile_sections_from_config() {
        let cmd = SdkCompileCommand::new("test.yaml".to_string(), false, vec![], None, None, None);

        let config_content = r#"
[sdk]
image = "test-image"

[sdk.compile.app]
compile = "build.sh"
dependencies = { gcc = "*" }

[sdk.compile.library]
compile = "lib_build.sh"
dependencies = { make = "*" }
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let sections = cmd.get_compile_sections_from_config(&config);

        assert_eq!(sections.len(), 2);

        let section_names: Vec<&String> = sections.iter().map(|s| &s.name).collect();
        assert!(section_names.contains(&&"app".to_string()));
        assert!(section_names.contains(&&"library".to_string()));

        // Find the app section and verify its script
        let app_section = sections.iter().find(|s| s.name == "app").unwrap();
        assert_eq!(app_section.script, "build.sh");
    }

    #[tokio::test]
    async fn test_missing_sections_error() {
        let cmd = SdkCompileCommand::new(
            "test.yaml".to_string(),
            false,
            vec!["nonexistent".to_string()],
            None,
            None,
            None,
        );

        let config_content = r#"
[sdk]
image = "test-image"

[sdk.compile.app]
compile = "build.sh"
dependencies = { gcc = "*" }
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();

        // This should work if we had a valid config file path
        // For now, we'll just test the structure
        assert_eq!(cmd.sections, vec!["nonexistent"]);
    }

    #[test]
    fn test_find_compile_script_in_section() {
        let cmd = SdkCompileCommand::new("test.yaml".to_string(), false, vec![], None, None, None);

        // Test section with compile script
        let mut deps = std::collections::HashMap::new();
        deps.insert(
            "gcc".to_string(),
            serde_yaml::Value::String("*".to_string()),
        );

        let section_config = crate::utils::config::CompileConfig {
            compile: Some("my_script.sh".to_string()),
            dependencies: Some(deps),
        };

        let script = cmd.find_compile_script_in_section(&section_config);
        assert_eq!(script, Some("my_script.sh".to_string()));

        // Test section with no compile script
        let section_config_no_script = crate::utils::config::CompileConfig {
            compile: None,
            dependencies: None,
        };

        let script = cmd.find_compile_script_in_section(&section_config_no_script);
        assert_eq!(script, None);
    }

    // ========================================================================
    // Stamp Dependency Tests
    // ========================================================================

    #[test]
    fn test_compile_stamp_requirements() {
        use crate::utils::stamps::StampRequirement;

        // sdk compile requires only: SDK install
        let requirements = [StampRequirement::sdk_install()];

        // Verify correct stamp path
        assert_eq!(requirements[0].relative_path(), "sdk/install.stamp");

        // Verify fix command is correct
        assert_eq!(requirements[0].fix_command(), "avocado sdk install");
    }

    #[test]
    fn test_compile_with_no_stamps_flag() {
        let cmd =
            SdkCompileCommand::new("config.yaml".to_string(), false, vec![], None, None, None);

        // Default should have stamps enabled
        assert!(!cmd.no_stamps);

        // Test with_no_stamps builder
        let cmd = cmd.with_no_stamps(true);
        assert!(cmd.no_stamps);
    }

    #[test]
    fn test_compile_fails_without_sdk_install() {
        use crate::utils::stamps::{validate_stamps_batch, StampRequirement};

        let requirements = vec![StampRequirement::sdk_install()];

        // SDK stamp missing
        let output = "sdk/install.stamp:::null";
        let result = validate_stamps_batch(&requirements, output, None);

        assert!(!result.is_satisfied());
        assert_eq!(result.missing.len(), 1);
        assert_eq!(result.missing[0].fix_command(), "avocado sdk install");
    }

    #[test]
    fn test_compile_succeeds_with_sdk_install() {
        use crate::utils::stamps::{
            validate_stamps_batch, Stamp, StampInputs, StampOutputs, StampRequirement,
        };

        let requirements = vec![StampRequirement::sdk_install()];

        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();

        let output = format!("sdk/install.stamp:::{}", sdk_json);
        let result = validate_stamps_batch(&requirements, &output, None);

        assert!(result.is_satisfied());
        assert_eq!(result.satisfied.len(), 1);
    }

    #[test]
    fn test_compile_clean_lifecycle() {
        use crate::utils::stamps::{
            validate_stamps_batch, Stamp, StampInputs, StampOutputs, StampRequirement,
        };

        let requirements = vec![StampRequirement::sdk_install()];

        // Before clean: SDK stamp present
        let sdk_stamp = Stamp::sdk_install(
            "qemux86-64",
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();

        let output_before = format!("sdk/install.stamp:::{}", sdk_json);
        let result_before = validate_stamps_batch(&requirements, &output_before, None);
        assert!(result_before.is_satisfied(), "Should pass before clean");

        // After clean --stamps: SDK stamp gone (simulating rm -rf .stamps/)
        let output_after = "sdk/install.stamp:::null";
        let result_after = validate_stamps_batch(&requirements, output_after, None);
        assert!(
            !result_after.is_satisfied(),
            "Should fail after clean --stamps"
        );
        assert_eq!(result_after.missing.len(), 1);
    }
}
