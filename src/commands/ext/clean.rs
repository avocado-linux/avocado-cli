use anyhow::{Context, Result};
use std::sync::Arc;

use super::find_ext_in_mapping;
use crate::utils::config::{ComposedConfig, Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::stamps::{
    generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement,
};
use crate::utils::target::resolve_target_required;

pub struct ExtCleanCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl ExtCleanCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        let target = resolve_target_required(self.target.as_deref(), config)?;
        let extension_location = self.find_extension_in_dependency_tree(config, &target)?;
        let container_image = self.get_container_image(config)?;

        // Get extension configuration from the composed/merged config
        let ext_config = self.get_extension_config(config, parsed, &extension_location, &target)?;

        // Determine the extension source path for clean scripts
        // For remote extensions, scripts are in $AVOCADO_PREFIX/includes/<ext-name>/
        // For local extensions, scripts are in /opt/src (the mounted src_dir)
        let ext_script_workdir = match &extension_location {
            ExtensionLocation::Remote { name, .. } => {
                Some(format!("$AVOCADO_PREFIX/includes/{name}"))
            }
            ExtensionLocation::Local { .. } => None,
        };

        // Execute clean scripts for compile dependencies BEFORE cleaning the extension
        // This allows clean scripts to access build artifacts if needed
        self.execute_compile_clean_scripts(
            config,
            &ext_config,
            &container_image,
            &target,
            ext_script_workdir.as_deref(),
        )
        .await?;

        self.clean_extension(&container_image, &target).await
    }

    /// Get extension configuration from the composed/merged config
    fn get_extension_config(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        extension_location: &ExtensionLocation,
        target: &str,
    ) -> Result<serde_yaml::Value> {
        match extension_location {
            ExtensionLocation::Remote { .. } => {
                // Use the already-merged config from `parsed` which contains remote extension configs
                // Use find_ext_in_mapping to handle template keys like "avocado-bsp-{{ avocado.target }}"
                let ext_section = find_ext_in_mapping(parsed, &self.extension, target);
                if let Some(ext_val) = ext_section {
                    let base_ext = ext_val.clone();
                    // Check for target-specific override within this extension
                    let target_override = ext_val.get(target).cloned();
                    if let Some(override_val) = target_override {
                        Ok(config.merge_target_override(base_ext, override_val, target))
                    } else {
                        Ok(base_ext)
                    }
                } else {
                    Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
                }
            }
            ExtensionLocation::Local { config_path, .. } => config
                .get_merged_ext_config(&self.extension, target, config_path)?
                .ok_or_else(|| {
                    anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
                }),
        }
    }

    /// Execute clean scripts for compile dependencies
    async fn execute_compile_clean_scripts(
        &self,
        config: &Config,
        ext_config: &serde_yaml::Value,
        container_image: &str,
        target: &str,
        ext_script_workdir: Option<&str>,
    ) -> Result<()> {
        // Get dependencies from extension configuration
        let dependencies = ext_config.get("packages").and_then(|v| v.as_mapping());

        let Some(deps_table) = dependencies else {
            return Ok(());
        };

        // Find compile dependencies that may have clean scripts
        let mut compile_sections_to_clean = Vec::new();

        for (dep_name_val, dep_spec) in deps_table {
            if let Some(dep_name) = dep_name_val.as_str() {
                if let serde_yaml::Value::Mapping(spec_map) = dep_spec {
                    // Check for compile dependency: { compile = "section-name", ... }
                    if let Some(serde_yaml::Value::String(compile_section)) =
                        spec_map.get("compile")
                    {
                        compile_sections_to_clean
                            .push((dep_name.to_string(), compile_section.clone()));
                    }
                }
            }
        }

        if compile_sections_to_clean.is_empty() {
            return Ok(());
        }

        // Get clean scripts from SDK compile sections
        let clean_scripts = self.get_clean_scripts_for_sections(config, &compile_sections_to_clean);

        if clean_scripts.is_empty() {
            if self.verbose {
                print_info(
                    "No clean scripts defined for compile dependencies",
                    OutputLevel::Normal,
                );
            }
            return Ok(());
        }

        // Get SDK configuration for container setup
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Initialize SDK container helper
        let container_helper = SdkContainer::from_config(&self.config_path, config)?;

        // Validate SDK is installed before running clean scripts
        let requirements = vec![StampRequirement::sdk_install()];
        let batch_script = generate_batch_read_stamps_script(&requirements);
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
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
            validation
                .into_error("Cannot run clean scripts for compile dependencies")
                .print_and_exit();
        }

        print_info(
            &format!(
                "Executing {} clean script(s) for compile dependencies",
                clean_scripts.len()
            ),
            OutputLevel::Normal,
        );

        // Execute each clean script
        for (section_name, clean_script) in clean_scripts {
            print_info(
                &format!(
                    "Running clean script for compile section '{section_name}': {clean_script}"
                ),
                OutputLevel::Normal,
            );

            // Build clean command with optional workdir prefix
            // For remote extensions, scripts are in $AVOCADO_PREFIX/includes/<ext>/ instead of /opt/src
            let clean_command = if let Some(workdir) = ext_script_workdir {
                format!(
                    r#"cd "{workdir}" && if [ -f '{clean_script}' ]; then echo 'Running clean script: {clean_script}'; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{clean_script}'; else echo 'Clean script {clean_script} not found, skipping.'; fi"#
                )
            } else {
                format!(
                    r#"if [ -f '{clean_script}' ]; then echo 'Running clean script: {clean_script}'; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{clean_script}'; else echo 'Clean script {clean_script} not found, skipping.'; fi"#
                )
            };

            if self.verbose {
                print_info(
                    &format!("Running command: {clean_command}"),
                    OutputLevel::Normal,
                );
            }

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: clean_command,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let success = container_helper.run_in_container(run_config).await?;

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
                return Err(anyhow::anyhow!(
                    "Clean script failed for section '{section_name}'"
                ));
            }
        }

        Ok(())
    }

    /// Get clean scripts for the specified compile sections
    fn get_clean_scripts_for_sections(
        &self,
        config: &Config,
        compile_sections: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut clean_scripts = Vec::new();

        if let Some(sdk) = &config.sdk {
            if let Some(compile) = &sdk.compile {
                for (_dep_name, section_name) in compile_sections {
                    if let Some(section_config) = compile.get(section_name) {
                        if let Some(clean_script) = &section_config.clean {
                            clean_scripts.push((section_name.clone(), clean_script.clone()));
                        }
                    }
                }
            }
        }

        clean_scripts
    }

    fn find_extension_in_dependency_tree(
        &self,
        config: &Config,
        target: &str,
    ) -> Result<ExtensionLocation> {
        match config.find_extension_in_dependency_tree(
            &self.config_path,
            &self.extension,
            target,
        )? {
            Some(location) => {
                if self.verbose {
                    match &location {
                        ExtensionLocation::Local { name, config_path } => {
                            print_info(
                                &format!(
                                    "Found local extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        ExtensionLocation::Remote { name, source } => {
                            print_info(
                                &format!("Found remote extension '{name}' with source: {source:?}"),
                                OutputLevel::Normal,
                            );
                        }
                    }
                }
                Ok(location)
            }
            None => {
                print_error(
                    &format!("Extension '{}' not found in configuration.", self.extension),
                    OutputLevel::Normal,
                );
                Err(anyhow::anyhow!("Extension not found"))
            }
        }
    }

    fn get_container_image(&self, config: &Config) -> Result<String> {
        config
            .get_sdk_image()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    async fn clean_extension(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning extension '{}'...", self.extension),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();

        // Clean sysroot, output files, and stamps
        let clean_command = format!(
            r#"
# Clean extension sysroot
rm -rf "$AVOCADO_EXT_SYSROOTS/{ext}"

# Clean extension output files (built .raw images)
rm -f "$AVOCADO_PREFIX/output/extensions/{ext}"-*.raw

# Clean extension stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/ext/{ext}"
"#,
            ext = self.extension
        );

        if self.verbose {
            print_info(
                &format!("Running command: {clean_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            container_args: crate::utils::config::Config::process_container_args(
                self.container_args.as_ref(),
            ),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(run_config).await?;

        if success {
            print_success(
                &format!("Successfully cleaned extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            print_error(
                &format!("Failed to clean extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!("Clean command failed"))
        }
    }

    /// Generate the clean command script for testing
    #[cfg(test)]
    fn generate_clean_script(&self) -> String {
        format!(
            r#"
# Clean extension sysroot
rm -rf "$AVOCADO_EXT_SYSROOTS/{ext}"

# Clean extension output files (built .raw images)
rm -f "$AVOCADO_PREFIX/output/extensions/{ext}"-*.raw

# Clean extension stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/ext/{ext}"
"#,
            ext = self.extension
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.extension, "test-ext");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = ExtCleanCommand::new(
            "my-extension".to_string(),
            "config.yaml".to_string(),
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.extension, "my-extension");
        assert!(cmd.verbose);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_clean_script_cleans_sysroot() {
        let cmd = ExtCleanCommand::new(
            "gpu-driver".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean extension sysroot
        assert!(script.contains(r#"rm -rf "$AVOCADO_EXT_SYSROOTS/gpu-driver""#));
    }

    #[test]
    fn test_clean_script_cleans_output_files() {
        let cmd = ExtCleanCommand::new(
            "network-driver".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean built extension images
        assert!(
            script.contains(r#"rm -f "$AVOCADO_PREFIX/output/extensions/network-driver"-*.raw"#)
        );
    }

    #[test]
    fn test_clean_script_cleans_stamps() {
        let cmd = ExtCleanCommand::new(
            "app-bundle".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean extension stamps (install and build)
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/.stamps/ext/app-bundle""#));
    }

    #[test]
    fn test_clean_script_includes_all_cleanup_targets() {
        let cmd = ExtCleanCommand::new(
            "my-ext".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Verify all three cleanup targets are present
        assert!(
            script.contains("AVOCADO_EXT_SYSROOTS"),
            "Should clean sysroot"
        );
        assert!(
            script.contains("output/extensions"),
            "Should clean output files"
        );
        assert!(script.contains(".stamps/ext"), "Should clean stamps");
    }

    #[test]
    fn test_get_clean_scripts_for_sections_with_clean_script() {
        use std::io::Write;
        use tempfile::NamedTempFile;

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

        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            temp_file.path().to_string_lossy().to_string(),
            false,
            None,
            None,
            None,
        );

        // Test with compile sections - one has clean script, one doesn't
        let compile_sections = vec![
            ("dep1".to_string(), "my-library".to_string()),
            ("dep2".to_string(), "other-library".to_string()),
        ];

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config, &compile_sections);

        // Only my-library has a clean script
        assert_eq!(clean_scripts.len(), 1);
        assert_eq!(clean_scripts[0].0, "my-library");
        assert_eq!(clean_scripts[0].1, "clean.sh");
    }

    #[test]
    fn test_get_clean_scripts_for_sections_no_clean_scripts() {
        use std::io::Write;
        use tempfile::NamedTempFile;

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

        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            temp_file.path().to_string_lossy().to_string(),
            false,
            None,
            None,
            None,
        );

        let compile_sections = vec![("dep1".to_string(), "my-library".to_string())];

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config, &compile_sections);

        // No clean script defined
        assert!(clean_scripts.is_empty());
    }

    #[test]
    fn test_get_clean_scripts_for_nonexistent_section() {
        use std::io::Write;
        use tempfile::NamedTempFile;

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

        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            temp_file.path().to_string_lossy().to_string(),
            false,
            None,
            None,
            None,
        );

        // Reference a section that doesn't exist
        let compile_sections = vec![("dep1".to_string(), "nonexistent-library".to_string())];

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config, &compile_sections);

        // No clean script found for nonexistent section
        assert!(clean_scripts.is_empty());
    }

    #[test]
    fn test_get_clean_scripts_multiple_sections_with_clean() {
        use std::io::Write;
        use tempfile::NamedTempFile;

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

        let cmd = ExtCleanCommand::new(
            "test-ext".to_string(),
            temp_file.path().to_string_lossy().to_string(),
            false,
            None,
            None,
            None,
        );

        let compile_sections = vec![
            ("dep-a".to_string(), "lib-a".to_string()),
            ("dep-b".to_string(), "lib-b".to_string()),
            ("dep-c".to_string(), "lib-c".to_string()),
        ];

        let clean_scripts = cmd.get_clean_scripts_for_sections(&config, &compile_sections);

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
