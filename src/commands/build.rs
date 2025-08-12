//! Build command implementation that runs SDK compile, extension build, and runtime build.

use anyhow::{Context, Result};
use std::collections::HashSet;

use crate::commands::{ext::{ExtBuildCommand, ExtImageCommand}, runtime::RuntimeBuildCommand, sdk::SdkCompileCommand};
use crate::utils::{
    config::Config,
    output::{print_info, print_success, OutputLevel},
};

/// Implementation of the 'build' command that runs all build subcommands.
pub struct BuildCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Runtime name to build (if not provided, builds all runtimes)
    pub runtime: Option<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
}

impl BuildCommand {
    /// Create a new BuildCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        runtime: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            runtime,
            target,
            container_args,
            dnf_args,
        }
    }

    /// Execute the build command
    pub async fn execute(&self) -> Result<()> {
        // Load the configuration and parse raw TOML
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        print_info(
            "Starting comprehensive build process...",
            OutputLevel::Normal,
        );

        // Determine which runtimes to build
        let runtimes_to_build = self.get_runtimes_to_build(&parsed)?;

        if runtimes_to_build.is_empty() {
            print_info("No runtimes found to build.", OutputLevel::Normal);
            return Ok(());
        }

        // Step 1: Analyze dependencies to find extensions that need SDK compilation
        print_info(
            "Step 1/3: Analyzing dependencies and compiling SDK code",
            OutputLevel::Normal,
        );
        let required_extensions = self.find_required_extensions(&parsed, &runtimes_to_build)?;
        let sdk_sections = self.find_sdk_compile_sections(&config, &required_extensions)?;

        if !sdk_sections.is_empty() {
            if self.verbose {
                print_info(
                    &format!(
                        "Found {} SDK compile sections needed: {}",
                        sdk_sections.len(),
                        sdk_sections.join(", ")
                    ),
                    OutputLevel::Normal,
                );
            }

            let sdk_compile_cmd = SdkCompileCommand::new(
                self.config_path.clone(),
                self.verbose,
                sdk_sections,
                self.target.clone(),
                self.container_args.clone(),
                self.dnf_args.clone(),
            );
            sdk_compile_cmd
                .execute()
                .await
                .with_context(|| "Failed to compile SDK sections")?;
        } else {
            print_info("No SDK compilation needed.", OutputLevel::Normal);
        }

        // Step 2: Build extensions
        print_info("Step 2/4: Building extensions", OutputLevel::Normal);
        if !required_extensions.is_empty() {
            for extension in &required_extensions {
                if self.verbose {
                    print_info(
                        &format!("Building extension '{extension}'"),
                        OutputLevel::Normal,
                    );
                }

                let ext_build_cmd = ExtBuildCommand::new(
                    extension.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                );
                ext_build_cmd
                    .execute()
                    .await
                    .with_context(|| format!("Failed to build extension '{extension}'"))?;
            }
        } else {
            print_info("No extensions to build.", OutputLevel::Normal);
        }

        // Step 3: Create extension images
        print_info("Step 3/4: Creating extension images", OutputLevel::Normal);
        if !required_extensions.is_empty() {
            for extension in &required_extensions {
                if self.verbose {
                    print_info(
                        &format!("Creating image for extension '{extension}'"),
                        OutputLevel::Normal,
                    );
                }

                let ext_image_cmd = ExtImageCommand::new(
                    extension.clone(),
                    self.config_path.clone(),
                    self.verbose,
                    self.target.clone(),
                    self.container_args.clone(),
                    self.dnf_args.clone(),
                );
                ext_image_cmd
                    .execute()
                    .await
                    .with_context(|| format!("Failed to create image for extension '{extension}'"))?;
            }
        } else {
            print_info("No extension images to create.", OutputLevel::Normal);
        }

        // Step 4: Build runtimes
        if let Some(ref runtime_name) = self.runtime {
            print_info(
                &format!("Step 4/4: Building runtime '{runtime_name}'"),
                OutputLevel::Normal,
            );
        } else {
            print_info("Step 4/4: Building all runtimes", OutputLevel::Normal);
        }

        for runtime_name in &runtimes_to_build {
            if self.verbose {
                print_info(
                    &format!("Building runtime '{runtime_name}'"),
                    OutputLevel::Normal,
                );
            }

            let runtime_build_cmd = RuntimeBuildCommand::new(
                runtime_name.clone(),
                self.config_path.clone(),
                self.verbose,
                self.target.clone(),
                self.container_args.clone(),
                self.dnf_args.clone(),
            );
            runtime_build_cmd
                .execute()
                .await
                .with_context(|| format!("Failed to build runtime '{runtime_name}'"))?;
        }

        print_success("All components built successfully!", OutputLevel::Normal);
        Ok(())
    }

    /// Determine which runtimes to build based on the --runtime parameter
    fn get_runtimes_to_build(&self, parsed: &toml::Value) -> Result<Vec<String>> {
        let runtime_section = parsed
            .get("runtime")
            .and_then(|r| r.as_table())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        if let Some(ref runtime_name) = self.runtime {
            // Single runtime specified
            if !runtime_section.contains_key(runtime_name) {
                return Err(anyhow::anyhow!(
                    "Runtime '{}' not found in configuration",
                    runtime_name
                ));
            }
            Ok(vec![runtime_name.clone()])
        } else {
            // Build all runtimes
            Ok(runtime_section.keys().cloned().collect())
        }
    }

    /// Find all extensions required by the specified runtimes
    fn find_required_extensions(
        &self,
        parsed: &toml::Value,
        runtimes: &[String],
    ) -> Result<Vec<String>> {
        let mut required_extensions = HashSet::new();

        let runtime_section = parsed.get("runtime").and_then(|r| r.as_table()).unwrap();

        for runtime_name in runtimes {
            if let Some(runtime_config) = runtime_section.get(runtime_name) {
                if let Some(dependencies) = runtime_config
                    .get("dependencies")
                    .and_then(|d| d.as_table())
                {
                    for (_dep_name, dep_spec) in dependencies {
                        if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                            required_extensions.insert(ext_name.to_string());
                        }
                    }
                }
            }
        }

        let mut extensions: Vec<String> = required_extensions.into_iter().collect();
        extensions.sort();
        Ok(extensions)
    }

    /// Find SDK compile sections needed for the required extensions
    fn find_sdk_compile_sections(
        &self,
        config: &Config,
        required_extensions: &[String],
    ) -> Result<Vec<String>> {
        let mut needed_sections = HashSet::new();

        // Get extension SDK dependencies
        let content = std::fs::read_to_string(&self.config_path)?;
        let extension_sdk_dependencies = config
            .get_extension_sdk_dependencies(&content)
            .with_context(|| "Failed to parse extension SDK dependencies")?;

        // For each required extension, check if it has SDK dependencies that need compilation
        for extension in required_extensions {
            if let Some(_ext_deps) = extension_sdk_dependencies.get(extension) {
                // If the extension has SDK dependencies, we might need to compile them
                // For now, we'll check if there are compile sections defined and add them all
                // In a more sophisticated implementation, we'd analyze which specific sections
                // are needed based on the extension's SDK dependencies
                if self.verbose {
                    print_info(
                        &format!("Extension '{extension}' has SDK dependencies"),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Get compile sections from config - we'll compile all of them if any extensions need SDK code
        if !required_extensions.is_empty() {
            let compile_dependencies = config.get_compile_dependencies();
            for section_name in compile_dependencies.keys() {
                needed_sections.insert(section_name.clone());
            }
        }

        let mut sections: Vec<String> = needed_sections.into_iter().collect();
        sections.sort();
        Ok(sections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = BuildCommand::new(
            "avocado.toml".to_string(),
            true,
            Some("my-runtime".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(cmd.verbose);
        assert_eq!(cmd.runtime, Some("my-runtime".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_all_runtimes() {
        let cmd = BuildCommand::new("config.toml".to_string(), false, None, None, None, None);

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }

    #[test]
    fn test_new_with_runtime() {
        let cmd = BuildCommand::new(
            "avocado.toml".to_string(),
            false,
            Some("test-runtime".to_string()),
            None,
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
