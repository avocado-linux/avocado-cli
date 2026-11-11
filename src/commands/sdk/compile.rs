//! SDK compile command implementation.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::utils::{
    config::{ComposedConfig, Config},
    container::{docker_env_flag, RunConfig, SdkContainer, TuiContext},
    output::{print_error, print_info, print_success, OutputLevel},
    stamps::{
        compute_sdk_compile_input_hash, generate_batch_read_stamps_script,
        generate_output_listing_probe, generate_write_stamp_script, get_local_arch,
        hash_root_is_mounted, locked_packages_hash, output_listing_from_batch,
        own_stamp_is_current, remove_own_stamp_line, sdk_compile_inputs_are_complete,
        stamp_identity_from_batch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
        StampRequirement,
    },
    target::resolve_target_required,
};

/// Compile section configuration
#[derive(Debug, Clone)]
pub struct CompileSection {
    pub name: String,
    pub script: String,
}

/// What `execute` does with one compile section.
#[derive(Debug, PartialEq)]
enum SectionPlan {
    /// The section stamp is current and the build directory is there.
    Skip,
    /// Run the script. With inputs, write the section stamp when it succeeds.
    Run(Option<StampInputs>),
}

/// What a section skip is decided from, besides the section and its stamp.
/// Set only under `ext build` with stamps on and a lock file.
struct SkipContext<'a> {
    merged: &'a serde_yaml::Value,
    project_root: &'a std::path::Path,
    /// The batch stamp read: SDK, compile-deps and section stamps, and the
    /// section build directories that exist.
    batch: &'a str,
    lock_pins: &'a std::collections::BTreeMap<String, String>,
    container_args: Option<&'a [String]>,
    /// The repo values the container gets, after env and `distro` overrides.
    repo_url: Option<&'a str>,
    repo_release: Option<&'a str>,
    /// Whether the container mounts the tree the hash reads.
    hash_root_mounted: bool,
}

/// Whether a container arg passes host state into the build that its text
/// does not show: an env flag with a bare name (`-e CFLAGS`, docker then
/// passes the host's current value), or `--env-file` (docker reads the file
/// when the container starts). Hashing the text would miss a change to either.
/// A config arg can hold several words (`-e CFLAGS`), so each is split first.
fn passes_host_state(container_args: &[String]) -> bool {
    let words: Vec<String> = container_args
        .iter()
        .flat_map(|a| a.split_whitespace().map(str::to_string))
        .collect();
    let mut i = 0;
    while i < words.len() {
        if words[i].starts_with("--env-file") {
            return true;
        }
        if let Some((taken, operand)) = docker_env_flag(&words, i) {
            if !operand.contains('=') {
                return true;
            }
            i += taken;
        } else {
            i += 1;
        }
    }
    false
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
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Working directory for compile scripts (container path).
    /// If set, scripts are executed from this directory instead of /opt/src.
    /// Used for remote extensions where scripts are in $AVOCADO_PREFIX/includes/<ext>/
    pub workdir: Option<String>,
    /// Under `ext build`: every extension that names the section, the
    /// calling extension first, each node resolved for the target as `ext
    /// build` resolves it. Only then may a section be skipped: its stamp folds
    /// their `package_files`. A direct `sdk compile` always runs.
    pub extensions: Option<Vec<(String, serde_yaml::Value)>>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
    /// TUI context for output capture when running under a parent orchestrator
    tui_context: Option<TuiContext>,
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
            sdk_arch: None,
            workdir: None,
            extensions: None,
            composed_config: None,
            tui_context: None,
        }
    }

    /// Set the working directory for compile scripts (container path)
    pub fn with_workdir(mut self, workdir: Option<String>) -> Self {
        self.workdir = workdir;
        self
    }

    /// Set the extensions that name the section, the caller first, with their
    /// resolved config nodes (see [`Self::extensions`])
    pub fn with_extensions(mut self, extensions: Vec<(String, serde_yaml::Value)>) -> Self {
        self.extensions = Some(extensions);
        self
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
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

    /// Set TUI context for output capture under a parent orchestrator
    pub fn with_tui_context(mut self, ctx: TuiContext) -> Self {
        self.tui_context = Some(ctx);
        self
    }

    /// Execute the sdk compile command
    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        if self.verbose {
            print_info(
                &format!("Loading SDK compile config from: {}", self.config_path),
                OutputLevel::Normal,
            );
        }
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };
        let config = &composed.config;

        // Validate stamps before proceeding (unless --no-stamps)
        // SDK compile requires SDK to be installed
        let mut stamp_batch: Option<String> = None;
        let mut mounted_src_dir: Option<std::path::PathBuf> = None;
        if !self.no_stamps {
            let container_image = config
                .get_sdk_image()
                .context("No SDK container image specified in configuration")?;
            let target = resolve_target_required(self.target.as_deref(), config)?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

            let requirements = vec![StampRequirement::sdk_install()];

            // Under `ext build`, the same round-trip reads what the section skip
            // needs: the section stamps, the compile-deps stamp, and which
            // section build directories exist.
            let mut batch_script = generate_batch_read_stamps_script(&requirements);
            if self.extensions.is_some() {
                let mut own: Vec<StampRequirement> = self
                    .sections
                    .iter()
                    .map(|s| StampRequirement::sdk_compile(s))
                    .collect();
                own.push(StampRequirement::compile_deps_install());
                batch_script = format!(
                    "{batch_script}\n{}\n{}",
                    generate_batch_read_stamps_script(&own),
                    generate_output_listing_probe("\"$AVOCADO_SDK_PREFIX/build\"/*"),
                );
            }
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
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            let validation =
                validate_stamps_batch(&requirements, output.as_deref().unwrap_or(""), &[]);

            if !validation.is_satisfied() {
                validation
                    .into_error("Cannot run SDK compile")
                    .with_search_root(crate::utils::stamps::StampSearchRoot::for_container(
                        &container_helper,
                        &target,
                    ))
                    .print_and_exit();
            }
            stamp_batch = output;
            mounted_src_dir = Some(container_helper.mounted_src_dir().to_path_buf());
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
        let compile_sections = self.get_compile_sections_from_config(config)?;

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

        print_info(
            &format!(
                "Found {} compile section(s) to process: {}",
                filtered_sections.len(),
                filtered_sections
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            OutputLevel::Normal,
        );

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Resolve target with proper precedence
        let target = resolve_target_required(self.target.as_deref(), config)?;

        let mut overall_success = true;
        let project_root = config.project_root(&self.config_path);
        let lock_pins = match (&self.extensions, &stamp_batch) {
            (Some(_), Some(_)) => self.lock_pins(config, &target),
            _ => None,
        };
        let skip_context = match (&stamp_batch, &lock_pins, &mounted_src_dir) {
            (Some(batch), Some(lock_pins), Some(mounted)) => Some(SkipContext {
                merged: &composed.merged_value,
                project_root: &project_root,
                batch,
                lock_pins,
                container_args: merged_container_args.as_deref(),
                repo_url: repo_url.as_deref(),
                repo_release: repo_release.as_deref(),
                hash_root_mounted: hash_root_is_mounted(&project_root, mounted),
            }),
            _ => None,
        };

        for section in &filtered_sections {
            let plan = self.plan_section(section, skip_context.as_ref());
            if plan == SectionPlan::Skip {
                print_success(
                    &format!("Compile section '{}' is up to date.", section.name),
                    OutputLevel::Normal,
                );
                continue;
            }

            print_info(
                &format!(
                    "Compiling section '{}' with script '{}'",
                    section.name, section.script
                ),
                OutputLevel::Normal,
            );

            let container_helper =
                SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

            // Build compile command with optional workdir prefix
            // For remote extensions, scripts are in $AVOCADO_PREFIX/includes/<ext>/ instead of /opt/src
            // Note: Use double quotes for workdir so $AVOCADO_PREFIX gets expanded by the shell
            let section_name = &section.name;
            let compile_command = if let Some(ref workdir) = self.workdir {
                format!(
                    r#"cd "{workdir}" && if [ -f '{}' ]; then echo 'Running compile script: {}'; mkdir -p $AVOCADO_SDK_PREFIX/build/{section_name} && AVOCADO_BUILD_DIR=$AVOCADO_SDK_PREFIX/build/{section_name} AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{}'; else echo 'Compile script {} not found.' && ls -la; exit 1; fi"#,
                    section.script, section.script, section.script, section.script
                )
            } else {
                format!(
                    r#"if [ -f '{}' ]; then echo 'Running compile script: {}'; mkdir -p $AVOCADO_SDK_PREFIX/build/{section_name} && AVOCADO_BUILD_DIR=$AVOCADO_SDK_PREFIX/build/{section_name} AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{}'; else echo 'Compile script {} not found.' && ls -la; exit 1; fi"#,
                    section.script, section.script, section.script, section.script
                )
            };

            // The old stamp goes before the script runs, so a failed or
            // interrupted run never leaves one behind. The new one is written
            // only after the script exits 0.
            let own_req = StampRequirement::sdk_compile(section_name);
            let mut compile_command = format!(
                "{}{compile_command} || exit 1\n",
                remove_own_stamp_line(&own_req)
            );
            if let SectionPlan::Run(Some(inputs)) = plan {
                let stamp = Stamp::sdk_compile(
                    section_name,
                    get_local_arch(),
                    inputs,
                    StampOutputs::default(),
                );
                compile_command.push_str(&generate_write_stamp_script(&stamp)?);
            }

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
                sdk_arch: self.sdk_arch.clone(),
                tui_context: self.tui_context.clone(),
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

    /// The lock pins a compiled section is built against: the SDK (the
    /// toolchain) and the target sysroot (the libraries it links). `None` when
    /// there is no lock file or it cannot be read, and then no section is
    /// skipped. `LockFile::load` returns an empty lock for a missing file, so
    /// the file is checked first.
    fn lock_pins(
        &self,
        config: &Config,
        target: &str,
    ) -> Option<std::collections::BTreeMap<String, String>> {
        use crate::utils::lockfile::{LockFile, SysrootType};
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                std::path::Path::new(&self.config_path)
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf()
            });
        if !LockFile::get_path(&src_dir).exists() && !LockFile::legacy_path(&src_dir).exists() {
            return None;
        }
        let lock = LockFile::load(&src_dir).ok()?;
        let mut pins = std::collections::BTreeMap::new();
        pins.insert(
            "sdk_pins".to_string(),
            locked_packages_hash(
                &lock,
                target,
                &SysrootType::Sdk(get_local_arch().to_string()),
            ),
        );
        pins.insert(
            "target_sysroot_pins".to_string(),
            locked_packages_hash(&lock, target, &SysrootType::TargetSysroot),
        );
        Some(pins)
    }

    /// Decide whether one section runs, and whether its run writes a stamp.
    ///
    /// A section is skipped only under `ext build` with stamps on and a lock
    /// file (`ctx` is then set), when its inputs are complete
    /// (`sdk_compile_inputs_are_complete`), the build reads the tree the hash
    /// reads, no container arg passes in host state the hash cannot see, its
    /// stamp is current, and its build directory exists. Everything else runs,
    /// as every section did before there was a stamp.
    fn plan_section(&self, section: &CompileSection, ctx: Option<&SkipContext<'_>>) -> SectionPlan {
        let (Some(extensions), Some(ctx)) = (self.extensions.as_deref(), ctx) else {
            return SectionPlan::Run(None);
        };
        let name = section.name.as_str();
        if !sdk_compile_inputs_are_complete(extensions, &section.script, ctx.project_root) {
            if let Some((undeclared, _)) = extensions
                .iter()
                .find(|(_, ext)| ext.get("package_files").is_none())
            {
                print_info(
                    &format!(
                        "Compile section '{name}' runs on every build. Add package_files to \
                         extension '{undeclared}' to skip the section when its sources do \
                         not change."
                    ),
                    OutputLevel::Normal,
                );
            }
            return SectionPlan::Run(None);
        }
        if !ctx.hash_root_mounted || passes_host_state(ctx.container_args.unwrap_or_default()) {
            return SectionPlan::Run(None);
        }

        let mut environment = ctx.lock_pins.clone();
        environment.insert(
            "sdk_install".to_string(),
            stamp_identity_from_batch(ctx.batch, &StampRequirement::sdk_install()),
        );
        environment.insert(
            "compile_deps".to_string(),
            stamp_identity_from_batch(ctx.batch, &StampRequirement::compile_deps_install()),
        );
        environment.insert(
            "container_args".to_string(),
            ctx.container_args.unwrap_or_default().join("\n"),
        );
        environment.insert(
            "repo_url".to_string(),
            ctx.repo_url.unwrap_or_default().to_string(),
        );
        environment.insert(
            "repo_release".to_string(),
            ctx.repo_release.unwrap_or_default().to_string(),
        );
        environment.insert(
            "sdk_arch".to_string(),
            self.sdk_arch.clone().unwrap_or_default(),
        );
        environment.insert(
            "workdir".to_string(),
            self.workdir.clone().unwrap_or_default(),
        );
        // A hash that cannot be computed is a reason to run, not to fail: the
        // section ran without a stamp before, and the build reports the same
        // file error from its own input hash.
        let Ok(inputs) = compute_sdk_compile_input_hash(
            ctx.merged,
            name,
            &section.script,
            extensions,
            ctx.project_root,
            &environment,
        ) else {
            return SectionPlan::Run(None);
        };

        let build_dir_present = output_listing_from_batch(ctx.batch)
            .iter()
            .any(|d| d == name);
        if build_dir_present
            && own_stamp_is_current(ctx.batch, &StampRequirement::sdk_compile(name), &inputs)
        {
            return SectionPlan::Skip;
        }
        SectionPlan::Run(Some(inputs))
    }

    /// Extract compile sections from configuration.
    ///
    /// A section without a `compile:` script (clean-only, install-only) is
    /// skipped, unless it was requested by name: then it is an error, because
    /// "not found" would be wrong for a section that does exist.
    fn get_compile_sections_from_config(&self, config: &Config) -> Result<Vec<CompileSection>> {
        let mut compile_sections = Vec::new();

        if let Some(sdk) = &config.sdk {
            if let Some(compile) = &sdk.compile {
                for (section_name, section_config) in compile {
                    // Look for a compile script in the section config
                    match self.find_compile_script_in_section(section_config) {
                        Some(compile_script) => compile_sections.push(CompileSection {
                            name: section_name.clone(),
                            script: compile_script,
                        }),
                        None if self.sections.contains(section_name) => {
                            return Err(anyhow::anyhow!(
                                "sdk.compile.{section_name} has no compile script. \
                                 Add a compile: script to the section \
                                 (an install-only section is not supported yet)."
                            ));
                        }
                        None => {}
                    }
                }
            }
        }

        Ok(compile_sections)
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
sdk:
  image: "test-image"
  compile:
    app:
      compile: "build.sh"
      packages:
        gcc: "*"
    library:
      compile: "lib_build.sh"
      packages:
        make: "*"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let sections = cmd.get_compile_sections_from_config(&config).unwrap();

        assert_eq!(sections.len(), 2);

        let section_names: Vec<&String> = sections.iter().map(|s| &s.name).collect();
        assert!(section_names.contains(&&"app".to_string()));
        assert!(section_names.contains(&&"library".to_string()));

        // Find the app section and verify its script
        let app_section = sections.iter().find(|s| s.name == "app").unwrap();
        assert_eq!(app_section.script, "build.sh");
    }

    fn composed(config_content: &str) -> Arc<ComposedConfig> {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        Arc::new(ComposedConfig {
            config: Config::load(temp_file.path()).unwrap(),
            merged_value: serde_yaml::Value::Null,
            config_path: temp_file.path().display().to_string(),
            extension_sources: Default::default(),
            ext_version_sources: Default::default(),
        })
    }

    const INSTALL_ONLY: &str = r#"
sdk:
  image: "test-image"
  compile:
    app:
      install: "x.sh"
"#;

    #[test]
    fn requested_section_without_compile_script_is_an_error() {
        let cmd = SdkCompileCommand::new(
            "test.yaml".to_string(),
            false,
            vec!["app".to_string()],
            None,
            None,
            None,
        );
        let err = cmd
            .get_compile_sections_from_config(&composed(INSTALL_ONLY).config)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "sdk.compile.app has no compile script. Add a compile: script to the section \
             (an install-only section is not supported yet)."
        );
    }

    #[test]
    fn unrequested_section_without_compile_script_is_skipped() {
        let cmd = SdkCompileCommand::new("test.yaml".to_string(), false, vec![], None, None, None);
        let sections = cmd
            .get_compile_sections_from_config(&composed(INSTALL_ONLY).config)
            .unwrap();
        assert!(sections.is_empty());
    }

    #[tokio::test]
    async fn absent_section_is_still_not_found() {
        let cmd = SdkCompileCommand::new(
            "test.yaml".to_string(),
            false,
            vec!["nosuch".to_string()],
            None,
            None,
            None,
        )
        .with_no_stamps(true)
        .with_composed_config(composed(INSTALL_ONLY));
        let err = cmd.execute().await.unwrap_err().to_string();
        assert!(err.contains("not found in config"), "{err}");
        assert!(!err.contains("no compile script"), "{err}");
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
            clean: None,
            packages: Some(deps),
            package: None,
        };

        let script = cmd.find_compile_script_in_section(&section_config);
        assert_eq!(script, Some("my_script.sh".to_string()));

        // Test section with no compile script
        let section_config_no_script = crate::utils::config::CompileConfig {
            compile: None,
            clean: None,
            packages: None,
            package: None,
        };

        let script = cmd.find_compile_script_in_section(&section_config_no_script);
        assert_eq!(script, None);
    }

    // ========================================================================
    // Stamp Dependency Tests
    // ========================================================================

    #[test]
    fn test_compile_stamp_requirements() {
        use crate::utils::stamps::{get_local_arch, StampRequirement};

        // sdk compile requires only: SDK install
        let requirements = [StampRequirement::sdk_install()];

        // Verify correct stamp path (now includes local architecture)
        assert_eq!(
            requirements[0].relative_path(),
            format!("sdk/{}/install.stamp", get_local_arch())
        );

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
        use crate::utils::stamps::{get_local_arch, validate_stamps_batch, StampRequirement};

        let requirements = vec![StampRequirement::sdk_install()];

        // SDK stamp missing
        let output = format!("sdk/{}/install.stamp:::null", get_local_arch());
        let result = validate_stamps_batch(&requirements, &output, &[]);

        assert!(!result.is_satisfied());
        assert_eq!(result.missing.len(), 1);
        assert_eq!(result.missing[0].fix_command(), "avocado sdk install");
    }

    #[test]
    fn test_compile_succeeds_with_sdk_install() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
            StampRequirement,
        };

        let requirements = vec![StampRequirement::sdk_install()];

        // SDK stamp now uses host architecture
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();

        let output = format!("sdk/{}/install.stamp:::{}", get_local_arch(), sdk_json);
        let result = validate_stamps_batch(&requirements, &output, &[]);

        assert!(result.is_satisfied());
        assert_eq!(result.satisfied.len(), 1);
    }

    #[test]
    fn test_compile_clean_lifecycle() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
            StampRequirement,
        };

        let requirements = vec![StampRequirement::sdk_install()];

        // Before clean: SDK stamp present
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();

        let output_before = format!("sdk/{}/install.stamp:::{}", get_local_arch(), sdk_json);
        let result_before = validate_stamps_batch(&requirements, &output_before, &[]);
        assert!(result_before.is_satisfied(), "Should pass before clean");

        // After clean --stamps: SDK stamp gone (simulating rm -rf .stamps/)
        let output_after = format!("sdk/{}/install.stamp:::null", get_local_arch());
        let result_after = validate_stamps_batch(&requirements, &output_after, &[]);
        assert!(
            !result_after.is_satisfied(),
            "Should fail after clean --stamps"
        );
        assert_eq!(result_after.missing.len(), 1);
    }

    // ========================================================================
    // Compile section skip (B-32)
    // ========================================================================

    const SKIP_YAML: &str = r#"
sdk:
  image: sdk:2024
  compile:
    app-compile:
      compile: compile.sh
extensions:
  my-ext:
    package_files: [src]
    packages:
      app:
        compile: app-compile
        install: install.sh
"#;

    fn skip_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("compile.sh"), "make\n").unwrap();
        std::fs::write(dir.path().join("install.sh"), "install app $DEST\n").unwrap();
        std::fs::write(dir.path().join("src/main.c"), "int main() {}\n").unwrap();
        dir
    }

    /// The compile step as `ext build` sets it up: the extensions that name
    /// the section, the caller first, each taken from `merged`.
    fn ext_build_compile(merged: &serde_yaml::Value, names: &[&str]) -> SdkCompileCommand {
        SdkCompileCommand::new(
            "avocado.yaml".to_string(),
            false,
            vec!["app-compile".to_string()],
            None,
            None,
            None,
        )
        .with_extensions(
            names
                .iter()
                .map(|n| (n.to_string(), merged["extensions"][*n].clone()))
                .collect(),
        )
    }

    fn app_section() -> CompileSection {
        CompileSection {
            name: "app-compile".to_string(),
            script: "compile.sh".to_string(),
        }
    }

    fn pins(sdk: &str) -> std::collections::BTreeMap<String, String> {
        [
            ("sdk_pins".to_string(), sdk.to_string()),
            ("target_sysroot_pins".to_string(), "t".to_string()),
        ]
        .into()
    }

    /// A batch read as `execute` gets it back: the SDK install stamp, the
    /// section stamp (if any), and the build directories that exist.
    fn batch_with(section_stamp: Option<&Stamp>, build_dirs: &str) -> String {
        use crate::utils::stamps::OUTPUT_LISTING_KEY;
        let sdk = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("sdk".to_string()),
            StampOutputs::default(),
        );
        let own = match section_stamp {
            Some(stamp) => serde_json::to_string(stamp).unwrap(),
            None => "null".to_string(),
        };
        format!(
            "{}:::{}\n{}:::{own}\n{}:::null\n{OUTPUT_LISTING_KEY}:::{build_dirs}\n",
            StampRequirement::sdk_install().relative_path(),
            serde_json::to_string(&sdk).unwrap(),
            StampRequirement::sdk_compile("app-compile").relative_path(),
            StampRequirement::compile_deps_install().relative_path(),
        )
    }

    /// A skip context with defaults: no container args, the default repo, and
    /// the hash root mounted.
    fn ctx<'a>(
        merged: &'a serde_yaml::Value,
        root: &'a std::path::Path,
        batch: &'a str,
        lock_pins: &'a std::collections::BTreeMap<String, String>,
    ) -> SkipContext<'a> {
        SkipContext {
            merged,
            project_root: root,
            batch,
            lock_pins,
            container_args: None,
            repo_url: None,
            repo_release: None,
            hash_root_mounted: true,
        }
    }

    /// The stamp a successful run writes, so the next run can be compared.
    fn stamp_written_by(plan: SectionPlan) -> Stamp {
        let SectionPlan::Run(Some(inputs)) = plan else {
            panic!("expected a run that writes a stamp, got {plan:?}");
        };
        Stamp::sdk_compile(
            "app-compile",
            get_local_arch(),
            inputs,
            StampOutputs::default(),
        )
    }

    /// Runs the section once with no stamp and returns the batch the next
    /// build reads: that stamp, and the build directory present.
    fn after_first_build(
        cmd: &SdkCompileCommand,
        merged: &serde_yaml::Value,
        root: &std::path::Path,
    ) -> String {
        let empty = batch_with(None, "");
        let p = pins("s");
        let stamp = stamp_written_by(
            cmd.plan_section(&app_section(), Some(&ctx(merged, root, &empty, &p))),
        );
        batch_with(Some(&stamp), "app-compile")
    }

    #[test]
    fn unchanged_section_is_skipped_after_an_install_script_edit() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let batch = after_first_build(&cmd, &merged, root);
        let p = pins("s");

        // The reported case: only install.sh changes. The section is skipped.
        std::fs::write(root.join("install.sh"), "install -m755 app $DEST\n").unwrap();
        assert_eq!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Skip
        );

        // A source edit under package_files runs it again.
        std::fs::write(root.join("src/main.c"), "int main() { return 1; }\n").unwrap();
        assert!(matches!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(Some(_))
        ));
    }

    #[test]
    fn section_runs_when_its_build_directory_is_gone() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let batch = after_first_build(&cmd, &merged, root)
            .replace(":::app-compile\n", ":::other-section\n");
        let p = pins("s");
        assert!(matches!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(Some(_))
        ));
    }

    /// Container args, the resolved repo and the lock pins are all inputs.
    #[test]
    fn section_runs_when_the_build_environment_changes() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let batch = after_first_build(&cmd, &merged, root);
        let p = pins("s");
        let plan = |c: SkipContext<'_>| cmd.plan_section(&app_section(), Some(&c));

        assert_eq!(plan(ctx(&merged, root, &batch, &p)), SectionPlan::Skip);

        let args = vec!["--env=OPT=1".to_string()];
        let mut c = ctx(&merged, root, &batch, &p);
        c.container_args = Some(&args);
        assert!(
            matches!(plan(c), SectionPlan::Run(Some(_))),
            "container args"
        );

        // The repo the container gets, after env and distro overrides.
        let mut c = ctx(&merged, root, &batch, &p);
        c.repo_url = Some("https://mirror.example.com");
        assert!(matches!(plan(c), SectionPlan::Run(Some(_))), "repo url");
        let mut c = ctx(&merged, root, &batch, &p);
        c.repo_release = Some("2026");
        assert!(matches!(plan(c), SectionPlan::Run(Some(_))), "repo release");

        let repinned = pins("s2");
        assert!(
            matches!(
                plan(ctx(&merged, root, &batch, &repinned)),
                SectionPlan::Run(Some(_))
            ),
            "lock pins"
        );

        // No context (no lock file, or --no-stamps): always runs.
        assert_eq!(
            cmd.plan_section(&app_section(), None),
            SectionPlan::Run(None)
        );
    }

    /// Docker passes host state the arg text does not show: the host's value
    /// for a bare env name, and the contents of an env file.
    #[test]
    fn section_never_skips_with_host_env_passthrough() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let batch = after_first_build(&cmd, &merged, root);
        let p = pins("s");

        for args in [
            vec!["--env=CFLAGS".to_string()],
            vec!["-e".to_string(), "CFLAGS".to_string()],
            vec!["-e CFLAGS".to_string()],
            vec!["-eCFLAGS".to_string()],
            vec!["--env-file=build.env".to_string()],
            vec!["--env-file".to_string(), "build.env".to_string()],
        ] {
            let mut c = ctx(&merged, root, &batch, &p);
            c.container_args = Some(&args);
            assert_eq!(
                cmd.plan_section(&app_section(), Some(&c)),
                SectionPlan::Run(None),
                "{args:?}"
            );
        }

        // A value in the text is hashed, and the provision args every
        // reference carries pass no host state.
        for args in [
            vec!["--env=CFLAGS=-O2".to_string()],
            vec![
                "--privileged".to_string(),
                "--network=host".to_string(),
                "-v /dev:/dev".to_string(),
                "-v /sys:/sys".to_string(),
            ],
        ] {
            assert!(!passes_host_state(&args), "{args:?}");
        }
    }

    /// Without `src_dir`, the container mounts the working directory. When
    /// that is not the tree the hash reads, nothing is skipped.
    #[test]
    fn section_never_skips_when_the_container_mounts_another_tree() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let batch = after_first_build(&cmd, &merged, root);
        let p = pins("s");
        let mut c = ctx(&merged, root, &batch, &p);
        c.hash_root_mounted = false;
        assert_eq!(
            cmd.plan_section(&app_section(), Some(&c)),
            SectionPlan::Run(None)
        );
    }

    /// The stamp is keyed by the section, so it hashes the sources of every
    /// extension that names it. B lists only its own files; A lists the
    /// library source. A library edit followed by a build of B must compile.
    #[test]
    fn shared_section_hashes_every_naming_extension() {
        let dir = skip_project();
        let root = dir.path();
        std::fs::create_dir_all(root.join("libfoo-src")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("libfoo-src/foo.c"), "int foo;\n").unwrap();
        std::fs::write(root.join("b/install.sh"), "install b\n").unwrap();
        let yaml = r#"
sdk:
  image: sdk:2024
  compile:
    app-compile:
      compile: compile.sh
extensions:
  ext-a:
    package_files: [libfoo-src, src]
    packages:
      libfoo:
        compile: app-compile
        install: install.sh
  ext-b:
    package_files: [b]
    packages:
      libfoo:
        compile: app-compile
        install: b/install.sh
"#;
        let merged: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let cmd = ext_build_compile(&merged, &["ext-b", "ext-a"]);
        let batch = after_first_build(&cmd, &merged, root);
        let p = pins("s");

        std::fs::write(root.join("libfoo-src/foo.c"), "int foo = 1;\n").unwrap();
        std::fs::write(root.join("b/install.sh"), "install -m755 b\n").unwrap();
        assert!(matches!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(Some(_))
        ));

        // One naming extension without a source list: no skip for anyone.
        let undeclared = yaml.replace("    package_files: [libfoo-src, src]\n", "");
        let merged: serde_yaml::Value = serde_yaml::from_str(&undeclared).unwrap();
        let cmd = ext_build_compile(&merged, &["ext-b", "ext-a"]);
        let batch = batch_with(None, "app-compile");
        assert_eq!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(None)
        );
    }

    /// A source edited through a symlinked `package_files` entry reruns the
    /// section: the digest hashes the link, so the skip is refused.
    #[cfg(unix)]
    #[test]
    fn source_edited_through_a_symlinked_package_files_entry_reruns() {
        let dir = skip_project();
        let root = dir.path();
        std::fs::create_dir_all(root.join("actual-src")).unwrap();
        std::fs::write(root.join("actual-src/main.c"), "int main() {}\n").unwrap();
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::os::unix::fs::symlink("actual-src", root.join("src")).unwrap();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let cmd = ext_build_compile(&merged, &["my-ext"]);
        let p = pins("s");
        let batch = batch_with(None, "app-compile");
        assert_eq!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(None)
        );
        std::fs::write(root.join("src/main.c"), "int main() { return 1; }\n").unwrap();
        assert_eq!(
            cmd.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(None)
        );
    }

    /// Without `package_files`, no stamp is written and the section always
    /// runs, as before.
    #[test]
    fn section_without_declared_sources_always_runs() {
        let dir = skip_project();
        let root = dir.path();
        let yaml = SKIP_YAML.replace("    package_files: [src]\n", "");
        let merged: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let batch = batch_with(None, "app-compile");
        let p = pins("s");
        assert_eq!(
            ext_build_compile(&merged, &["my-ext"])
                .plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(None)
        );
    }

    /// A direct `sdk compile` names no extensions, so it always runs and
    /// writes no stamp.
    #[test]
    fn direct_compile_always_runs() {
        let dir = skip_project();
        let root = dir.path();
        let merged: serde_yaml::Value = serde_yaml::from_str(SKIP_YAML).unwrap();
        let batch = after_first_build(&ext_build_compile(&merged, &["my-ext"]), &merged, root);
        let p = pins("s");
        let direct = SdkCompileCommand::new(
            "avocado.yaml".to_string(),
            false,
            vec!["app-compile".to_string()],
            None,
            None,
            None,
        );
        assert_eq!(
            direct.plan_section(&app_section(), Some(&ctx(&merged, root, &batch, &p))),
            SectionPlan::Run(None)
        );
    }

    /// No lock file means no pins to compare, so no section is skipped.
    /// `LockFile::load` returns an empty lock for a missing file, which would
    /// otherwise hash like a real, empty pin set.
    #[test]
    fn lock_pins_need_a_lock_file() {
        let dir = skip_project();
        let config_path = dir.path().join("avocado.yaml");
        std::fs::write(&config_path, SKIP_YAML).unwrap();
        let config = Config::load(&config_path).unwrap();
        let cmd = SdkCompileCommand::new(
            config_path.display().to_string(),
            false,
            vec![],
            None,
            None,
            None,
        );
        assert!(cmd.lock_pins(&config, "qemuarm64").is_none());

        crate::utils::lockfile::LockFile::new()
            .save(dir.path())
            .unwrap();
        let pins = cmd
            .lock_pins(&config, "qemuarm64")
            .expect("lock file exists");
        assert!(pins.contains_key("sdk_pins"));
        assert!(pins.contains_key("target_sysroot_pins"));
    }
}
