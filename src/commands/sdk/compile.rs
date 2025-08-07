//! SDK compile command implementation.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    target::resolve_target,
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
}

impl SdkCompileCommand {
    /// Create a new SdkCompileCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        sections: Vec<String>,
        target: Option<String>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            sections,
            target,
        }
    }

    /// Execute the sdk compile command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Get compile sections from config
        let compile_sections = self.get_compile_sections_from_config(&config);

        if compile_sections.is_empty() {
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

        // Resolve target with proper precedence
        let config_target = config.get_target();
        let target = resolve_target(self.target.as_deref(), config_target.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'."
                )
            })?;

        let mut overall_success = true;

        for section in &filtered_sections {
            print_info(
                &format!(
                    "Compiling section '{}' with script '{}'",
                    section.name, section.script
                ),
                OutputLevel::Normal,
            );

            let container_helper = SdkContainer::new().verbose(self.verbose);

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
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.sections, vec!["app"]);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }

    #[test]
    fn test_get_compile_sections_from_config() {
        let cmd = SdkCompileCommand::new("test.toml".to_string(), false, vec![], None);

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
            "test.toml".to_string(),
            false,
            vec!["nonexistent".to_string()],
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
        let cmd = SdkCompileCommand::new("test.toml".to_string(), false, vec![], None);

        // Test section with compile script
        let mut deps = std::collections::HashMap::new();
        deps.insert("gcc".to_string(), toml::Value::String("*".to_string()));

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
}
