//! SDK clean command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    stamps::{generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement},
    target::resolve_target_required,
};

/// Context for running clean operations in containers
struct CleanContext<'a> {
    container_helper: &'a SdkContainer,
    container_image: &'a str,
    target: &'a str,
    repo_url: Option<String>,
    repo_release: Option<String>,
    merged_container_args: Option<Vec<String>>,
}

/// Implementation of the 'sdk clean' command.
pub struct SdkCleanCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Specific compile sections to clean
    pub sections: Vec<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
}

impl SdkCleanCommand {
    /// Create a new SdkCleanCommand instance
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
            sdk_arch: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Execute the sdk clean command
    pub async fn execute(&self) -> Result<()> {
        // Load composed configuration to get sdk.compile sections
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let config = &composed.config;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Create container helper
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // If sections are specified, run clean scripts for those sections
        if !self.sections.is_empty() {
            // Validate SDK is installed before running clean scripts
            let requirements = vec![StampRequirement::sdk_install()];
            let batch_script = generate_batch_read_stamps_script(&requirements);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            let validation =
                validate_stamps_batch(&requirements, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error = validation.into_error("Cannot run SDK clean scripts");
                return Err(error.into());
            }

            let ctx = CleanContext {
                container_helper: &container_helper,
                container_image,
                target: &target,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                merged_container_args: merged_container_args.clone(),
            };
            return self.clean_sections(config, &ctx).await;
        }

        // Default behavior: Remove the entire SDK directory
        if self.verbose {
            print_info(
                "Removing SDK directory: $AVOCADO_SDK_PREFIX",
                OutputLevel::Normal,
            );
        }

        let remove_command = "rm -rf $AVOCADO_SDK_PREFIX";
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: remove_command.to_string(),
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url,
            repo_release,
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(run_config).await?;

        if success {
            print_success("Successfully removed SDK directory.", OutputLevel::Normal);
        } else {
            print_error("Failed to remove SDK directory.", OutputLevel::Normal);
            return Err(anyhow::anyhow!("Failed to remove SDK directory"));
        }

        Ok(())
    }

    /// Clean specific compile sections by running their clean scripts
    async fn clean_sections(&self, config: &Config, ctx: &CleanContext<'_>) -> Result<()> {
        // Get clean scripts for the requested sections
        let clean_scripts = self.get_clean_scripts_for_sections(config)?;

        if clean_scripts.is_empty() {
            print_info(
                "No clean scripts defined for the specified sections.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        let section_list = clean_scripts
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        print_info(
            &format!(
                "Executing clean scripts for {} section(s): {section_list}",
                clean_scripts.len()
            ),
            OutputLevel::Normal,
        );

        let mut overall_success = true;

        for (section_name, clean_script) in &clean_scripts {
            print_info(
                &format!("Running clean script for section '{section_name}': {clean_script}"),
                OutputLevel::Normal,
            );

            // Build clean command - scripts are relative to src_dir (/opt/src in container)
            let clean_command = format!(
                r#"if [ -f '{clean_script}' ]; then echo 'Running clean script: {clean_script}'; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{clean_script}'; else echo 'Clean script {clean_script} not found, skipping.'; fi"#
            );

            if self.verbose {
                print_info(
                    &format!("Running command: {clean_command}"),
                    OutputLevel::Normal,
                );
            }

            let run_config = RunConfig {
                container_image: ctx.container_image.to_string(),
                target: ctx.target.to_string(),
                command: clean_command,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: ctx.repo_url.clone(),
                repo_release: ctx.repo_release.clone(),
                container_args: ctx.merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let success = ctx.container_helper.run_in_container(run_config).await?;

            if success {
                print_success(
                    &format!("Completed clean script for section '{section_name}'."),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!("Failed to run clean script for section '{section_name}'."),
                    OutputLevel::Normal,
                );
                overall_success = false;
            }
        }

        if overall_success {
            print_success(
                &format!(
                    "All {} clean script(s) completed successfully!",
                    clean_scripts.len()
                ),
                OutputLevel::Normal,
            );
        }

        if !overall_success {
            return Err(anyhow::anyhow!("One or more clean scripts failed."));
        }

        Ok(())
    }

    /// Get clean scripts for the specified sections
    fn get_clean_scripts_for_sections(&self, config: &Config) -> Result<Vec<(String, String)>> {
        let mut clean_scripts = Vec::new();
        let mut missing_sections = Vec::new();
        let mut sections_without_clean = Vec::new();

        if let Some(sdk) = &config.sdk {
            if let Some(compile) = &sdk.compile {
                for section_name in &self.sections {
                    if let Some(section_config) = compile.get(section_name) {
                        if let Some(clean_script) = &section_config.clean {
                            clean_scripts.push((section_name.clone(), clean_script.clone()));
                        } else {
                            sections_without_clean.push(section_name.clone());
                        }
                    } else {
                        missing_sections.push(section_name.clone());
                    }
                }
            } else {
                // No compile sections at all
                missing_sections = self.sections.clone();
            }
        } else {
            // No SDK section at all
            missing_sections = self.sections.clone();
        }

        // Report missing sections as errors
        if !missing_sections.is_empty() {
            return Err(anyhow::anyhow!(
                "The following compile sections were not found: {}",
                missing_sections.join(", ")
            ));
        }

        // Report sections without clean scripts as info
        if !sections_without_clean.is_empty() && self.verbose {
            print_info(
                &format!(
                    "The following sections have no clean script defined: {}",
                    sections_without_clean.join(", ")
                ),
                OutputLevel::Normal,
            );
        }

        Ok(clean_scripts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_new() {
        let cmd = SdkCleanCommand::new(
            "config.toml".to_string(),
            true,
            vec!["section1".to_string()],
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.sections, vec!["section1"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_new_minimal() {
        let cmd = SdkCleanCommand::new("config.toml".to_string(), false, vec![], None, None, None);

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert!(cmd.sections.is_empty());
        assert_eq!(cmd.target, None);
    }

    #[test]
    fn test_get_clean_scripts_for_sections_with_clean_script() {
        let config_content = r#"
sdk:
  image: "test-image"
  compile:
    my-library:
      compile: "build.sh"
      clean: "clean.sh"
      packages:
        gcc: "*"
    other-library:
      compile: "build-other.sh"
      packages:
        make: "*"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let cmd = SdkCleanCommand::new(
            temp_file.path().to_string_lossy().to_string(),
            false,
            vec!["my-library".to_string()],
            None,
            None,
            None,
        );

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config).unwrap();

        assert_eq!(clean_scripts.len(), 1);
        assert_eq!(clean_scripts[0].0, "my-library");
        assert_eq!(clean_scripts[0].1, "clean.sh");
    }

    #[test]
    fn test_get_clean_scripts_for_sections_no_clean_script() {
        let config_content = r#"
sdk:
  image: "test-image"
  compile:
    my-library:
      compile: "build.sh"
      packages:
        gcc: "*"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let cmd = SdkCleanCommand::new(
            temp_file.path().to_string_lossy().to_string(),
            false,
            vec!["my-library".to_string()],
            None,
            None,
            None,
        );

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config).unwrap();

        // Section exists but has no clean script
        assert!(clean_scripts.is_empty());
    }

    #[test]
    fn test_get_clean_scripts_for_nonexistent_section() {
        let config_content = r#"
sdk:
  image: "test-image"
  compile:
    my-library:
      compile: "build.sh"
      clean: "clean.sh"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let cmd = SdkCleanCommand::new(
            temp_file.path().to_string_lossy().to_string(),
            false,
            vec!["nonexistent-library".to_string()],
            None,
            None,
            None,
        );

        // Should return an error for nonexistent section
        let result = cmd.get_clean_scripts_for_sections(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("nonexistent-library"));
    }

    #[test]
    fn test_get_clean_scripts_multiple_sections() {
        let config_content = r#"
sdk:
  image: "test-image"
  compile:
    lib-a:
      compile: "build-a.sh"
      clean: "clean-a.sh"
    lib-b:
      compile: "build-b.sh"
      clean: "clean-b.sh"
    lib-c:
      compile: "build-c.sh"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let cmd = SdkCleanCommand::new(
            temp_file.path().to_string_lossy().to_string(),
            false,
            vec![
                "lib-a".to_string(),
                "lib-b".to_string(),
                "lib-c".to_string(),
            ],
            None,
            None,
            None,
        );

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config).unwrap();

        // lib-a and lib-b have clean scripts, lib-c doesn't
        assert_eq!(clean_scripts.len(), 2);

        let section_names: Vec<&str> = clean_scripts
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(section_names.contains(&"lib-a"));
        assert!(section_names.contains(&"lib-b"));
    }
}
