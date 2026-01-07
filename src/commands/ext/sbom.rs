//! Extension SBOM (Software Bill of Materials) generation command.
//!
//! Generates an SPDX-formatted SBOM for a specific extension sysroot.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::lockfile::SysrootType;
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::sbom::{build_rpm_query_all_command, parse_rpm_query_output, SbomPackage, SpdxBuilder};
use crate::utils::target::resolve_target_required;

pub struct ExtSbomCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    out: Option<String>,
    container_args: Option<Vec<String>>,
    #[allow(dead_code)] // Kept for API consistency with other commands
    dnf_args: Option<Vec<String>>,
}

impl ExtSbomCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        out: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
            out,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Resolve target architecture
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Validate extension exists in config
        self.validate_extension_exists(&parsed)?;

        // Get SDK configuration
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Initialize container helper
        let container_helper = SdkContainer::new().verbose(self.verbose);

        if self.verbose {
            print_info(
                &format!("Generating SBOM for extension '{}'", self.extension),
                OutputLevel::Normal,
            );
        }

        // Query packages from extension sysroot
        let packages = self
            .query_extension_packages(
                &container_helper,
                container_image,
                &target,
                repo_url.as_ref(),
                repo_release.as_ref(),
                &merged_container_args,
            )
            .await?;

        if packages.is_empty() {
            print_error(
                &format!(
                    "No packages found in extension '{}' sysroot. Has the extension been installed?",
                    self.extension
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Build SPDX document
        let mut builder = SpdxBuilder::new(
            &format!("avocado-extension-{}-sbom", self.extension),
            "https://avocado.dev/sbom/extension",
        );
        builder.add_packages(packages);

        let spdx_json = builder.to_json()?;

        // Output to file or stdout
        if let Some(out_path) = &self.out {
            let src_dir = config
                .get_resolved_src_dir(&self.config_path)
                .unwrap_or_else(|| {
                    PathBuf::from(&self.config_path)
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .to_path_buf()
                });
            let full_path = src_dir.join(out_path);

            // Create parent directories if needed
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }

            fs::write(&full_path, &spdx_json)?;
            print_success(
                &format!("SBOM written to: {}", full_path.display()),
                OutputLevel::Normal,
            );
        } else {
            // Output to stdout
            println!("{}", spdx_json);
        }

        Ok(())
    }

    fn validate_extension_exists(&self, parsed: &serde_yaml::Value) -> Result<()> {
        // Check if extension is defined in the ext section
        let ext_section = parsed.get("ext").and_then(|v| v.as_mapping());

        let extension_exists = ext_section
            .map(|table| table.contains_key(&self.extension))
            .unwrap_or(false);

        if !extension_exists {
            let available: Vec<String> = ext_section
                .map(|table| {
                    table
                        .keys()
                        .filter_map(|k| k.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            anyhow::bail!(
                "Extension '{}' not found in configuration. Available extensions: {}",
                self.extension,
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            );
        }

        Ok(())
    }

    async fn query_extension_packages(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_args: &Option<Vec<String>>,
    ) -> Result<Vec<SbomPackage>> {
        // Get RPM config for the extension sysroot
        let sysroot = SysrootType::Extension(self.extension.clone());
        let rpm_config = sysroot.get_rpm_query_config();

        // Build query command for all packages
        let query_command = build_rpm_query_all_command(rpm_config.root_path.as_deref());

        if self.verbose {
            print_info(
                &format!("Querying packages: {}", query_command),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: query_command,
            verbose: self.verbose,
            source_environment: false,
            use_entrypoint: true,
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: container_args.clone(),
            ..Default::default()
        };

        let output = container_helper
            .run_in_container_with_output(run_config)
            .await?;

        let source = format!("extension:{}", self.extension);
        match output {
            Some(output) => Ok(parse_rpm_query_output(&output, Some(&source))),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = ExtSbomCommand::new(
            "my-ext".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.extension, "my-ext");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
    }
}

