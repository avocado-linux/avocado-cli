//! Runtime SBOM (Software Bill of Materials) generation command.
//!
//! Generates an SPDX-formatted SBOM for a runtime, including all packages
//! from the runtime sysroot and all included extension sysroots.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::utils::config::{Config, RuntimeExtDep};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::lockfile::SysrootType;
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::sbom::{
    build_rpm_query_all_command, parse_rpm_query_output, SbomPackage, SpdxBuilder,
};
use crate::utils::target::resolve_target_required;

pub struct RuntimeSbomCommand {
    runtime: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    out: Option<String>,
    container_args: Option<Vec<String>>,
    #[allow(dead_code)] // Kept for API consistency with other commands
    dnf_args: Option<Vec<String>>,
}

impl RuntimeSbomCommand {
    pub fn new(
        runtime: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        out: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime,
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

        // Validate runtime exists
        self.validate_runtime_exists(&parsed)?;

        // Get SDK configuration
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Initialize container helper
        let container_helper = SdkContainer::new().verbose(self.verbose);

        print_info(
            &format!("Generating SBOM for runtime '{}'", self.runtime),
            OutputLevel::Normal,
        );

        // Collect all packages
        let mut all_packages: Vec<SbomPackage> = Vec::new();

        // 1. Query packages from runtime sysroot
        if self.verbose {
            print_info("Querying runtime sysroot packages...", OutputLevel::Normal);
        }

        let runtime_packages = self
            .query_sysroot_packages(
                &container_helper,
                container_image,
                &target,
                repo_url.as_ref(),
                repo_release.as_ref(),
                &merged_container_args,
                &SysrootType::Runtime(self.runtime.clone()),
                "runtime",
            )
            .await?;

        if self.verbose {
            print_info(
                &format!("Found {} packages in runtime sysroot", runtime_packages.len()),
                OutputLevel::Normal,
            );
        }
        all_packages.extend(runtime_packages);

        // 2. Get extension dependencies for this runtime
        let ext_deps = config.get_runtime_extension_dependencies_detailed(
            &self.runtime,
            &target,
            &self.config_path,
        )?;

        // 3. Query packages from each extension sysroot
        for ext_dep in &ext_deps {
            let ext_name = ext_dep.name();
            if self.verbose {
                print_info(
                    &format!("Querying extension '{}' sysroot packages...", ext_name),
                    OutputLevel::Normal,
                );
            }

            let sysroot = match ext_dep {
                RuntimeExtDep::Local(_) | RuntimeExtDep::External { .. } => {
                    SysrootType::Extension(ext_name.to_string())
                }
                RuntimeExtDep::Versioned { name, .. } => {
                    SysrootType::VersionedExtension(name.clone())
                }
            };

            let source = format!("extension:{}", ext_name);
            let ext_packages = self
                .query_sysroot_packages(
                    &container_helper,
                    container_image,
                    &target,
                    repo_url.as_ref(),
                    repo_release.as_ref(),
                    &merged_container_args,
                    &sysroot,
                    &source,
                )
                .await?;

            if self.verbose {
                print_info(
                    &format!(
                        "Found {} packages in extension '{}' sysroot",
                        ext_packages.len(),
                        ext_name
                    ),
                    OutputLevel::Normal,
                );
            }
            all_packages.extend(ext_packages);
        }

        if all_packages.is_empty() {
            print_error(
                &format!(
                    "No packages found in runtime '{}' or its extensions. Has the runtime been installed?",
                    self.runtime
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Build SPDX document with deduplication
        let mut builder = SpdxBuilder::new(
            &format!("avocado-runtime-{}-sbom", self.runtime),
            "https://avocado.dev/sbom/runtime",
        );
        builder.add_packages(all_packages);
        builder.deduplicate();

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

        let pkg_count = builder.build().packages.len();
        print_success(
            &format!(
                "Generated SBOM with {} unique packages for runtime '{}' (including {} extension(s))",
                pkg_count,
                self.runtime,
                ext_deps.len()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn validate_runtime_exists(&self, parsed: &serde_yaml::Value) -> Result<()> {
        let runtime_section = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        runtime_section.get(&self.runtime).with_context(|| {
            format!(
                "Runtime '{}' not found in configuration",
                self.runtime
            )
        })?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn query_sysroot_packages(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        container_args: &Option<Vec<String>>,
        sysroot: &SysrootType,
        source: &str,
    ) -> Result<Vec<SbomPackage>> {
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

        match output {
            Some(output) => Ok(parse_rpm_query_output(&output, Some(source))),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeSbomCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
            None,
        );

        assert_eq!(cmd.runtime, "dev");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
    }
}

