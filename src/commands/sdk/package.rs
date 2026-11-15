//! SDK package command implementation.
//!
//! Takes cross-compiled output, stages it into a sysroot layout,
//! and creates architecture-specific RPMs with optional sub-package splitting.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::utils::{
    config::{Config, PackageConfig, SplitPackageConfig},
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    prerequisites::read_stamps_batch,
    stamps::StampRequirement,
    target::resolve_target_required,
};

/// RPM metadata collected from PackageConfig
#[derive(Debug)]
struct RpmMetadata {
    name: String,
    version: String,
    release: String,
    summary: String,
    description: String,
    license: String,
    arch: String,
    vendor: String,
    url: Option<String>,
    requires: Vec<String>,
}

/// Implementation of the 'sdk package' command.
pub struct SdkPackageCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Compile section to package
    pub section: String,
    /// Output directory on host for the built RPM(s)
    pub output_dir: Option<String>,
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
}

impl SdkPackageCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        section: String,
        output_dir: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            section,
            output_dir,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            sdk_arch: None,
        }
    }

    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Execute the sdk package command
    pub async fn execute(&self) -> Result<()> {
        let config = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let config = &config.config;

        // Validate SDK install stamp
        if !self.no_stamps {
            let container_image = config
                .get_sdk_image()
                .context("No SDK container image specified in configuration")?;
            let target = resolve_target_required(self.target.as_deref(), config)?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

            let requirements = vec![StampRequirement::sdk_install()];
            let validation = read_stamps_batch(
                &requirements,
                &container_helper,
                RunConfig {
                    container_image: container_image.to_string(),
                    target: target.clone(),
                    repo_url: config.get_sdk_repo_url(),
                    repo_release: config.get_sdk_repo_release(),
                    container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
                    dnf_args: self.dnf_args.clone(),
                    sdk_arch: self.sdk_arch.clone(),
                    ..Default::default()
                },
                None,
            )
            .await?
            .validate(&requirements, &[]);

            if !validation.is_satisfied() {
                validation
                    .into_error("Cannot run SDK package")
                    .with_search_root(crate::utils::stamps::StampSearchRoot::for_container(
                        &container_helper,
                        &target,
                    ))
                    .print_and_exit();
            }
        }

        // Look up the compile section
        let sdk = config
            .sdk
            .as_ref()
            .context("No 'sdk' section in configuration")?;
        let compile_map = sdk
            .compile
            .as_ref()
            .context("No 'sdk.compile' section in configuration")?;
        let compile_config = compile_map.get(&self.section).ok_or_else(|| {
            anyhow::anyhow!(
                "Compile section '{}' not found. Available sections: {}",
                self.section,
                compile_map.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;

        // Validate package block
        let pkg_config = compile_config.package.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Compile section '{}' has no 'package' block. Add a 'package' block with at least 'install' and 'version'.",
                self.section
            )
        })?;

        // Resolve target and architecture
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Extract RPM metadata
        let metadata = self.extract_rpm_metadata(pkg_config, &target)?;

        if self.verbose {
            print_info(
                &format!(
                    "Packaging section '{}' as {}-{}-{}.{}.rpm",
                    self.section, metadata.name, metadata.version, metadata.release, metadata.arch
                ),
                OutputLevel::Normal,
            );
        }

        // Build and collect RPMs
        let output_paths = self
            .create_rpm_packages_in_container(&metadata, pkg_config, config, &target)
            .await?;

        for path in &output_paths {
            print_success(
                &format!("Successfully created RPM: {}", path.display()),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Map a target triple or Avocado machine name to an RPM architecture string.
    ///
    /// Well-known compile triples (e.g. `aarch64-unknown-linux-gnu`) map to their
    /// canonical RPM arch. Everything else is normalized: lowercased with hyphens
    /// replaced by underscores, since RPM arch names cannot contain hyphens.
    pub fn target_to_rpm_arch(target: &str) -> String {
        if target.starts_with("aarch64-") || target == "aarch64" {
            "aarch64".to_string()
        } else if target.starts_with("x86_64-") || target == "x86_64" {
            "x86_64".to_string()
        } else if target.starts_with("armv7-") || target.starts_with("armv7hl") {
            "armv7hl".to_string()
        } else if target.starts_with("riscv64-") || target == "riscv64" {
            "riscv64".to_string()
        } else if target.starts_with("i686-") || target == "i686" {
            "i686".to_string()
        } else {
            // Normalize: lowercase and replace hyphens with underscores.
            // RPM arch names cannot contain hyphens, so e.g. "qemux86-64" → "qemux86_64".
            target.to_lowercase().replace('-', "_")
        }
    }

    /// Extract RPM metadata from PackageConfig.
    fn extract_rpm_metadata(&self, pkg: &PackageConfig, target: &str) -> Result<RpmMetadata> {
        // Validate version
        crate::utils::version::validate_semver(&pkg.version).with_context(|| {
            format!(
                "Section '{}' has invalid version '{}'. Must be semver (e.g. '1.0.0')",
                self.section, pkg.version
            )
        })?;

        let name = pkg.name.clone().unwrap_or_else(|| self.section.clone());

        let arch = pkg
            .arch
            .clone()
            .unwrap_or_else(|| Self::target_to_rpm_arch(target));

        let release = pkg.release.clone().unwrap_or_else(|| "1".to_string());

        let license = pkg
            .license
            .clone()
            .unwrap_or_else(|| "Unspecified".to_string());

        let vendor = pkg
            .vendor
            .clone()
            .unwrap_or_else(|| "Unspecified".to_string());

        let summary = pkg
            .summary
            .clone()
            .unwrap_or_else(|| Self::generate_summary(&name));

        let description = pkg
            .description
            .clone()
            .unwrap_or_else(|| Self::generate_description(&name));

        let requires = pkg.requires.clone().unwrap_or_default();

        Ok(RpmMetadata {
            name,
            version: pkg.version.clone(),
            release,
            summary,
            description,
            license,
            arch,
            vendor,
            url: pkg.url.clone(),
            requires,
        })
    }

    fn generate_summary(name: &str) -> String {
        let words: Vec<String> = name
            .split('-')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect();
        format!("{} compiled SDK package", words.join(" "))
    }

    fn generate_description(name: &str) -> String {
        format!("Compiled SDK package for {name}")
    }

    /// Create all RPM packages in the SDK container.
    async fn create_rpm_packages_in_container(
        &self,
        metadata: &RpmMetadata,
        pkg_config: &PackageConfig,
        config: &Config,
        target: &str,
    ) -> Result<Vec<PathBuf>> {
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&cwd).await?;

        // Build the RPM script
        let (rpm_build_script, rpm_filenames) =
            self.generate_rpm_build_script(metadata, pkg_config, target)?;

        if self.verbose {
            print_info(
                "Creating RPM package(s) in container...",
                OutputLevel::Normal,
            );
        }

        let container_helper = SdkContainer::new();
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: rpm_build_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: config.get_sdk_repo_url(),
            repo_release: config.get_sdk_repo_release(),
            container_args: merged_container_args,
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to create RPM package(s) in container"
            ));
        }

        // Copy RPM(s) to host if --out-dir specified
        let mut output_paths = Vec::new();
        for rpm_filename in &rpm_filenames {
            let container_rpm_path =
                format!("/opt/_avocado/{target}/output/packages/{rpm_filename}");

            if let Some(output_dir) = &self.output_dir {
                self.copy_rpm_to_host(
                    &container_helper.container_tool,
                    &volume_state.volume_name,
                    &container_rpm_path,
                    output_dir,
                    rpm_filename,
                    container_image,
                )
                .await?;

                output_paths.push(PathBuf::from(output_dir).join(rpm_filename));
            } else {
                output_paths.push(PathBuf::from(rpm_filename));
            }
        }

        Ok(output_paths)
    }

    /// Generate the RPM build shell script and return it along with the expected RPM filenames.
    fn generate_rpm_build_script(
        &self,
        metadata: &RpmMetadata,
        pkg_config: &PackageConfig,
        _target: &str,
    ) -> Result<(String, Vec<String>)> {
        let section = &self.section;
        let name = &metadata.name;
        // Every use of the version below is an RPM filename or the spec's
        // `Version:` field, so it's the RPM form throughout — `extract_rpm_metadata`
        // validates semver but `rpmspec` rejects a `-`, which aborted this command
        // on any pre-release before.
        let version = &crate::utils::version::to_rpm_version(&metadata.version)
            .with_context(|| format!("Section '{}' cannot be packaged", self.section))?;
        let release = &metadata.release;
        let arch = &metadata.arch;
        let summary = &metadata.summary;
        let description = &metadata.description;
        let license = &metadata.license;
        let vendor = &metadata.vendor;

        let url_line = metadata
            .url
            .as_ref()
            .map(|u| format!("URL: {u}"))
            .unwrap_or_default();

        let mut dependency_lines: Vec<String> = metadata
            .requires
            .iter()
            .map(|r| format!("Requires: {r}"))
            .collect();
        for provide in pkg_config.provides.iter().flatten() {
            if provide.chars().any(char::is_control) {
                anyhow::bail!(
                    "Section '{}': provides entry {:?} contains a newline or control character",
                    self.section,
                    provide
                );
            }
            if provide.trim().is_empty() {
                anyhow::bail!(
                    "Section '{}': provides entry is empty; remove it or name a capability",
                    self.section
                );
            }
            dependency_lines.push(format!("Provides: {}", escape_spec_percent(provide)));
        }
        let requires_lines = dependency_lines.join("\n");

        let epoch_line = pkg_config
            .epoch
            .map(|e| format!("\nEpoch: {e}"))
            .unwrap_or_default();

        let mut scriptlet_sections = String::new();
        for (tag, body) in [
            ("%pre", &pkg_config.pre_install),
            ("%post", &pkg_config.post_install),
            ("%preun", &pkg_config.pre_uninstall),
            ("%postun", &pkg_config.post_uninstall),
        ] {
            let Some(body) = body else { continue };
            // The spec travels in a quoted heredoc: `$` and backticks are inert,
            // but a line equal to the terminator would end it early.
            if body.contains('\0') || body.lines().any(|l| l == "SPEC_EOF") {
                anyhow::bail!(
                    "Section '{}': {} scriptlet contains a NUL byte or a line equal to the spec terminator SPEC_EOF",
                    self.section,
                    tag
                );
            }
            // rpmbuild expands macros on each line before it looks for section
            // headers, so a `%%files` line (our escaped `%files`) still opens a
            // section and cuts the scriptlet short. Doubling cannot prevent it.
            if let Some(line) = body.lines().find(|l| l.trim_start().starts_with('%')) {
                anyhow::bail!(
                    "Section '{}': {} scriptlet line {:?} starts with '%', which rpm reads as a spec section; start the line with another character",
                    self.section,
                    tag,
                    line
                );
            }
            scriptlet_sections.push_str(&format!("\n\n{tag}\n{}", escape_spec_percent(body)));
        }

        // Collect RPM filenames to expect
        let main_rpm = format!("{name}-{version}-{release}.{arch}.rpm");
        let mut rpm_filenames = vec![main_rpm.clone()];

        // Build sub-package spec sections and partition script
        let (split_spec_sections, partition_script) = if let Some(split) = &pkg_config.split {
            let spec = self.generate_split_spec_sections(split, name, version, arch);
            let script = self.generate_partition_script(split);
            // Add sub-package RPM filenames
            for subpkg_name in split.keys() {
                rpm_filenames.push(format!(
                    "{name}-{subpkg_name}-{version}-{release}.{arch}.rpm"
                ));
            }
            (spec, script)
        } else {
            (String::new(), String::new())
        };

        // Main %files section: if split defined, main gets unmatched files from $MAIN_DIR;
        // otherwise all files from $STAGING
        let main_files_section = if pkg_config.split.is_some() {
            // Files come from partitioned MAIN_DIR
            r#"%files
%defattr(-,root,root,-)
/*"#
            .to_string()
        } else if let Some(files) = &pkg_config.files {
            // Explicit file patterns
            let patterns = files.join("\n");
            format!("%files\n%defattr(-,root,root,-)\n{patterns}")
        } else {
            // All staged files
            r#"%files
%defattr(-,root,root,-)
/*"#
            .to_string()
        };

        let main_files_section = format!("{main_files_section}{scriptlet_sections}");

        let install_script = &pkg_config.install;
        let install_script_escaped = install_script.replace('\'', "'\\''");

        let script = format!(
            r#"
set -e

STAGING="$AVOCADO_SDK_PREFIX/staging/{section}"
BUILD_DIR="$AVOCADO_SDK_PREFIX/build/{section}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/packages"

mkdir -p "$STAGING" "$BUILD_DIR" "$OUTPUT_DIR"

# Run install script with DESTDIR and AVOCADO_BUILD_DIR
export DESTDIR="$STAGING"
export AVOCADO_BUILD_DIR="$BUILD_DIR"

if [ ! -f '{install_script_escaped}' ]; then
    echo "ERROR: Install script not found: {install_script_escaped}"
    exit 1
fi

echo "Running install script: {install_script_escaped}"
bash '{install_script_escaped}'

# Verify files were staged
FILE_COUNT=$(find "$STAGING" -type f | wc -l)
if [ "$FILE_COUNT" -eq 0 ]; then
    echo "ERROR: No files staged by install script"
    exit 1
fi
echo "Staged $FILE_COUNT file(s)"

{partition_script}

# Create RPM build tree
TMPDIR=$(mktemp -d)
mkdir -p "$TMPDIR/BUILD" "$TMPDIR/RPMS" "$TMPDIR/SOURCES" "$TMPDIR/SPECS" "$TMPDIR/SRPMS"

# Generate spec file
# Note: heredoc is single-quoted so no shell expansion inside.
# The staging path is passed via rpmbuild --define so it becomes an RPM macro.
cat > "$TMPDIR/SPECS/package.spec" << 'SPEC_EOF'
%define _buildhost reproducible
AutoReqProv: no

Name: {name}{epoch_line}
Version: {version}
Release: {release}
Summary: {summary}
License: {license}
Vendor: {vendor}
{url_line}
{requires_lines}

%description
{description}

{split_spec_sections}

%install
mkdir -p %{{buildroot}}
cp -a %{{staging_dir}}/. %{{buildroot}}/

{main_files_section}

%clean
%changelog
SPEC_EOF

# Run rpmbuild; pass staging_dir so %install can reference it as an RPM macro
rpmbuild --define "_topdir $TMPDIR" --define "staging_dir $STAGING" --define "_arch {arch}" --target {arch} -bb "$TMPDIR/SPECS/package.spec"

# Move RPMs to output
find "$TMPDIR/RPMS" -name '*.rpm' | while read rpm_path; do
    rpm_file=$(basename "$rpm_path")
    mv "$rpm_path" "$OUTPUT_DIR/$rpm_file"
    echo "RPM created: $OUTPUT_DIR/$rpm_file"
done

rm -rf "$TMPDIR"
"#,
        );

        Ok((script, rpm_filenames))
    }

    /// Generate spec sub-package sections for split packages.
    fn generate_split_spec_sections(
        &self,
        split: &HashMap<String, SplitPackageConfig>,
        parent_name: &str,
        version: &str,
        arch: &str,
    ) -> String {
        let mut sections = String::new();

        // Sort for deterministic output
        let mut subpkg_names: Vec<&String> = split.keys().collect();
        subpkg_names.sort();

        for subpkg_name in subpkg_names {
            let subpkg = &split[subpkg_name];
            let full_name = format!("{parent_name}-{subpkg_name}");

            let summary = subpkg
                .summary
                .clone()
                .unwrap_or_else(|| format!("{full_name} package"));

            let description = subpkg
                .description
                .clone()
                .unwrap_or_else(|| format!("Sub-package {full_name}"));

            let requires_lines: String = subpkg
                .requires
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|r| format!("Requires: {r}"))
                .collect::<Vec<_>>()
                .join("\n");

            // %files section for this sub-package uses the patterns from config
            let files_list: String = subpkg.files.join("\n");

            sections.push_str(&format!(
                r#"
%package -n {full_name}
Summary: {summary}
{requires_lines}

%description -n {full_name}
{description}

%files -n {full_name}
%defattr(-,root,root,-)
{files_list}

"#,
            ));

            let _ = (version, arch); // suppress unused warnings
        }

        sections
    }

    /// Generate file partitioning script for split packages.
    /// This runs in the container after install.sh to handle file partitioning.
    fn generate_partition_script(&self, split: &HashMap<String, SplitPackageConfig>) -> String {
        // For split packages, we still copy everything to buildroot and use
        // RPM %files sections to claim files. The %files patterns from config
        // are embedded directly in the spec. No shell-level partitioning needed.
        //
        // However, warn if any sub-packages are empty (best-effort, not blocking).
        let _ = split;
        String::new()
    }

    /// Copy an RPM from the container volume to the host.
    async fn copy_rpm_to_host(
        &self,
        container_tool: &str,
        volume_name: &str,
        container_rpm_path: &str,
        output_dir: &str,
        rpm_filename: &str,
        _container_image: &str,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Copying RPM to host: {output_dir}/{rpm_filename}"),
                OutputLevel::Normal,
            );
        }

        let temp_container_id = self
            .create_temp_container(container_tool, volume_name)
            .await?;

        let host_output_dir = if output_dir.starts_with('/') {
            PathBuf::from(output_dir)
        } else {
            std::env::current_dir()?.join(output_dir)
        };
        fs::create_dir_all(&host_output_dir)?;

        let docker_cp_source = format!("{temp_container_id}:{container_rpm_path}");
        let docker_cp_dest = host_output_dir.join(rpm_filename);

        if self.verbose {
            print_info(
                &format!(
                    "{container_tool} cp {} -> {}",
                    docker_cp_source,
                    docker_cp_dest.display()
                ),
                OutputLevel::Normal,
            );
        }

        let copy_output = tokio::process::Command::new(container_tool)
            .arg("cp")
            .arg(&docker_cp_source)
            .arg(&docker_cp_dest)
            .output()
            .await
            .context("Failed to execute docker cp")?;

        let _ = tokio::process::Command::new(container_tool)
            .arg("rm")
            .arg("-f")
            .arg(&temp_container_id)
            .output()
            .await;

        if !copy_output.status.success() {
            let stderr = String::from_utf8_lossy(&copy_output.stderr);
            return Err(anyhow::anyhow!("docker cp failed: {stderr}"));
        }

        Ok(())
    }

    /// Create a temporary container to access the volume for `<container_tool> cp`.
    async fn create_temp_container(
        &self,
        container_tool: &str,
        volume_name: &str,
    ) -> Result<String> {
        let output = tokio::process::Command::new(container_tool)
            .arg("create")
            .arg("-v")
            .arg(format!("{volume_name}:/opt/_avocado:ro"))
            .arg("alpine:latest")
            .arg("true")
            .output()
            .await
            .context("Failed to create temporary container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "Failed to create temporary container: {stderr}"
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Double every `%` so rpm reads the text literally instead of as a macro.
fn escape_spec_percent(text: &str) -> String {
    text.replace('%', "%%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cmd() -> SdkPackageCommand {
        SdkPackageCommand::new(
            "avocado.yaml".to_string(),
            false,
            "my-app".to_string(),
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn test_new() {
        let cmd = SdkPackageCommand::new(
            "test.yaml".to_string(),
            true,
            "hello".to_string(),
            Some("./rpms".to_string()),
            Some("aarch64-unknown-linux-gnu".to_string()),
            None,
            None,
        );
        assert_eq!(cmd.config_path, "test.yaml");
        assert!(cmd.verbose);
        assert_eq!(cmd.section, "hello");
        assert_eq!(cmd.output_dir, Some("./rpms".to_string()));
        assert!(!cmd.no_stamps);
    }

    #[test]
    fn test_with_no_stamps() {
        let cmd = make_cmd().with_no_stamps(true);
        assert!(cmd.no_stamps);
    }

    #[test]
    fn test_with_sdk_arch() {
        let cmd = make_cmd().with_sdk_arch(Some("aarch64".to_string()));
        assert_eq!(cmd.sdk_arch, Some("aarch64".to_string()));
    }

    #[test]
    fn test_target_to_rpm_arch() {
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("aarch64-unknown-linux-gnu"),
            "aarch64"
        );
        assert_eq!(SdkPackageCommand::target_to_rpm_arch("aarch64"), "aarch64");
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("x86_64-unknown-linux-gnu"),
            "x86_64"
        );
        assert_eq!(SdkPackageCommand::target_to_rpm_arch("x86_64"), "x86_64");
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("armv7-unknown-linux-gnueabihf"),
            "armv7hl"
        );
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("riscv64-unknown-linux-gnu"),
            "riscv64"
        );
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("i686-unknown-linux-gnu"),
            "i686"
        );
        // Avocado machine names: hyphens become underscores, no hardcoded mapping
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("qemux86-64"),
            "qemux86_64"
        );
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("qemuarm64"),
            "qemuarm64"
        );
        assert_eq!(SdkPackageCommand::target_to_rpm_arch("qemuarm"), "qemuarm");
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("qemuriscv64"),
            "qemuriscv64"
        );
        // Unknown targets: normalize (lowercase + hyphens → underscores)
        assert_eq!(
            SdkPackageCommand::target_to_rpm_arch("mips-unknown-linux-gnu"),
            "mips_unknown_linux_gnu"
        );
    }

    #[test]
    fn test_generate_summary() {
        assert_eq!(
            SdkPackageCommand::generate_summary("my-app"),
            "My App compiled SDK package"
        );
        assert_eq!(
            SdkPackageCommand::generate_summary("libfoo"),
            "Libfoo compiled SDK package"
        );
    }

    #[test]
    fn test_generate_description() {
        assert_eq!(
            SdkPackageCommand::generate_description("my-app"),
            "Compiled SDK package for my-app"
        );
    }

    #[test]
    fn test_extract_rpm_metadata_minimal() {
        let cmd = make_cmd();
        let pkg = PackageConfig {
            install: "install.sh".to_string(),
            version: "1.0.0".to_string(),
            name: None,
            release: None,
            license: None,
            summary: None,
            description: None,
            vendor: None,
            url: None,
            arch: None,
            requires: None,
            provides: None,
            epoch: None,
            pre_install: None,
            post_install: None,
            pre_uninstall: None,
            post_uninstall: None,
            files: None,
            split: None,
        };

        let meta = cmd
            .extract_rpm_metadata(&pkg, "aarch64-unknown-linux-gnu")
            .unwrap();

        assert_eq!(meta.name, "my-app"); // defaults to section name
        assert_eq!(meta.version, "1.0.0");
        assert_eq!(meta.release, "1");
        assert_eq!(meta.license, "Unspecified");
        assert_eq!(meta.vendor, "Unspecified");
        assert_eq!(meta.arch, "aarch64"); // derived from target
        assert_eq!(meta.url, None);
        assert!(meta.requires.is_empty());
    }

    #[test]
    fn test_extract_rpm_metadata_full() {
        let cmd = make_cmd();
        let pkg = PackageConfig {
            install: "install.sh".to_string(),
            version: "2.3.4".to_string(),
            name: Some("custom-name".to_string()),
            release: Some("2".to_string()),
            license: Some("Apache-2.0".to_string()),
            summary: Some("A custom summary".to_string()),
            description: Some("A longer description".to_string()),
            vendor: Some("Acme Corp".to_string()),
            url: Some("https://example.com".to_string()),
            arch: Some("noarch".to_string()),
            requires: Some(vec!["glibc >= 2.17".to_string()]),
            provides: None,
            epoch: None,
            pre_install: None,
            post_install: None,
            pre_uninstall: None,
            post_uninstall: None,
            files: None,
            split: None,
        };

        let meta = cmd
            .extract_rpm_metadata(&pkg, "aarch64-unknown-linux-gnu")
            .unwrap();

        assert_eq!(meta.name, "custom-name");
        assert_eq!(meta.version, "2.3.4");
        assert_eq!(meta.release, "2");
        assert_eq!(meta.license, "Apache-2.0");
        assert_eq!(meta.summary, "A custom summary");
        assert_eq!(meta.description, "A longer description");
        assert_eq!(meta.vendor, "Acme Corp");
        assert_eq!(meta.url, Some("https://example.com".to_string()));
        assert_eq!(meta.arch, "noarch"); // explicit override
        assert_eq!(meta.requires, vec!["glibc >= 2.17"]);
    }

    #[test]
    fn test_extract_rpm_metadata_missing_version_error() {
        let cmd = make_cmd();
        let pkg = PackageConfig {
            install: "install.sh".to_string(),
            version: "bad_version".to_string(), // invalid semver
            name: None,
            release: None,
            license: None,
            summary: None,
            description: None,
            vendor: None,
            url: None,
            arch: None,
            requires: None,
            provides: None,
            epoch: None,
            pre_install: None,
            post_install: None,
            pre_uninstall: None,
            post_uninstall: None,
            files: None,
            split: None,
        };

        let result = cmd.extract_rpm_metadata(&pkg, "x86_64-unknown-linux-gnu");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_deserialization_without_package() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let yaml = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:dev"
  compile:
    my-app:
      compile: build.sh
      packages:
        gcc: "*"
"#;
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{yaml}").unwrap();
        let config = Config::load(f.path()).unwrap();

        let compile = config.sdk.unwrap().compile.unwrap();
        let section = compile.get("my-app").unwrap();
        assert_eq!(section.compile, Some("build.sh".to_string()));
        assert!(section.package.is_none());
    }

    #[test]
    fn test_config_deserialization_with_package_minimal() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let yaml = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:dev"
  compile:
    my-app:
      compile: build.sh
      package:
        install: install.sh
        version: "1.0.0"
"#;
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{yaml}").unwrap();
        let config = Config::load(f.path()).unwrap();

        let compile = config.sdk.unwrap().compile.unwrap();
        let section = compile.get("my-app").unwrap();
        let pkg = section.package.as_ref().unwrap();
        assert_eq!(pkg.install, "install.sh");
        assert_eq!(pkg.version, "1.0.0");
        assert!(pkg.name.is_none());
        assert!(pkg.split.is_none());
    }

    #[test]
    fn test_config_deserialization_with_package_full() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let yaml = r#"
sdk:
  image: "docker.io/avocadolinux/sdk:dev"
  compile:
    my-app:
      compile: build.sh
      package:
        install: install.sh
        version: "1.2.3"
        name: my-custom-app
        release: "2"
        license: MIT
        summary: "My custom app"
        vendor: "Acme Corp"
        arch: aarch64
        requires:
          - "glibc >= 2.17"
        files:
          - /usr/bin/*
        split:
          dev:
            summary: "Dev files"
            files:
              - /usr/include/**
"#;
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{yaml}").unwrap();
        let config = Config::load(f.path()).unwrap();

        let compile = config.sdk.unwrap().compile.unwrap();
        let section = compile.get("my-app").unwrap();
        let pkg = section.package.as_ref().unwrap();

        assert_eq!(pkg.version, "1.2.3");
        assert_eq!(pkg.name, Some("my-custom-app".to_string()));
        assert_eq!(pkg.release, Some("2".to_string()));
        assert_eq!(pkg.license, Some("MIT".to_string()));
        assert_eq!(pkg.arch, Some("aarch64".to_string()));
        assert!(pkg
            .requires
            .as_ref()
            .unwrap()
            .contains(&"glibc >= 2.17".to_string()));
        assert!(pkg.split.is_some());

        let dev = pkg.split.as_ref().unwrap().get("dev").unwrap();
        assert_eq!(dev.files, vec!["/usr/include/**"]);
    }

    fn bare_pkg() -> PackageConfig {
        PackageConfig {
            install: "install.sh".to_string(),
            version: "1.0.0".to_string(),
            name: None,
            release: None,
            license: None,
            summary: Some("sum".to_string()),
            description: Some("desc".to_string()),
            vendor: None,
            url: None,
            arch: None,
            requires: None,
            provides: None,
            epoch: None,
            pre_install: None,
            post_install: None,
            pre_uninstall: None,
            post_uninstall: None,
            files: None,
            split: None,
        }
    }

    fn gen_script(pkg: &PackageConfig) -> Result<String> {
        let cmd = make_cmd();
        let meta = cmd.extract_rpm_metadata(pkg, "aarch64-unknown-linux-gnu")?;
        cmd.generate_rpm_build_script(&meta, pkg, "aarch64-unknown-linux-gnu")
            .map(|(s, _)| s)
    }

    /// The spec text between the heredoc markers.
    fn spec_of(script: &str) -> String {
        let start_marker = "<< 'SPEC_EOF'\n";
        let start = script.find(start_marker).unwrap() + start_marker.len();
        let end = script[start..].find("\nSPEC_EOF\n").unwrap() + start;
        script[start..end].to_string()
    }

    #[test]
    fn spec_without_new_keys_is_unchanged() {
        let spec = spec_of(&gen_script(&bare_pkg()).unwrap());
        let expected = "%define _buildhost reproducible\nAutoReqProv: no\n\nName: my-app\nVersion: 1.0.0\nRelease: 1\nSummary: sum\nLicense: Unspecified\nVendor: Unspecified\n\n\n\n%description\ndesc\n\n\n\n%install\nmkdir -p %{buildroot}\ncp -a %{staging_dir}/. %{buildroot}/\n\n%files\n%defattr(-,root,root,-)\n/*\n\n%clean\n%changelog";
        assert_eq!(spec, expected);
        assert!(!spec.contains("Epoch:"));
        assert!(!spec.contains("%post"));
        assert!(!spec.contains("Provides:"));
    }

    #[test]
    fn spec_emits_provides_and_epoch() {
        let mut pkg = bare_pkg();
        pkg.requires = Some(vec!["glibc".to_string()]);
        pkg.provides = Some(vec!["foo = 1.0".to_string(), "bar".to_string()]);
        pkg.epoch = Some(2);
        let spec = spec_of(&gen_script(&pkg).unwrap());
        assert!(spec.contains("\nEpoch: 2\n"), "{spec}");
        assert!(
            spec.contains("\nProvides: foo = 1.0\nProvides: bar\n"),
            "{spec}"
        );
        assert!(spec.contains("Requires: glibc\n"), "{spec}");
        assert!(spec.contains("AutoReqProv: no"));
        // Epoch sits in the header, before Version.
        assert!(spec.find("Epoch: 2").unwrap() < spec.find("Version:").unwrap());
    }

    #[test]
    fn scriptlet_percent_is_escaped() {
        let mut pkg = bare_pkg();
        pkg.post_install = Some("echo 100%s %{buildroot}".to_string());
        pkg.provides = Some(vec!["x = %{v}".to_string()]);
        let spec = spec_of(&gen_script(&pkg).unwrap());
        assert!(
            spec.contains("\n%post\necho 100%%s %%{buildroot}"),
            "{spec}"
        );
        assert!(spec.contains("Provides: x = %%{v}"), "{spec}");
        assert!(!spec.contains("100%s"));
    }

    #[test]
    fn scriptlets_emit_all_four_sections() {
        let mut pkg = bare_pkg();
        pkg.pre_install = Some("a".to_string());
        pkg.post_install = Some("b".to_string());
        pkg.pre_uninstall = Some("c".to_string());
        pkg.post_uninstall = Some("d".to_string());
        let spec = spec_of(&gen_script(&pkg).unwrap());
        for (sec, body) in [
            ("%pre", "a"),
            ("%post", "b"),
            ("%preun", "c"),
            ("%postun", "d"),
        ] {
            assert!(
                spec.contains(&format!("\n{sec}\n{body}\n")),
                "{sec}: {spec}"
            );
        }
        assert!(spec.find("%files").unwrap() < spec.find("\n%pre\n").unwrap());
        assert!(spec.find("\n%postun\n").unwrap() < spec.find("%clean").unwrap());
    }

    #[test]
    fn provides_with_newline_is_rejected() {
        for bad in ["foo\nRequires: evil", "foo\rbar", "foo\u{1b}bar", "foo\0"] {
            let mut pkg = bare_pkg();
            pkg.provides = Some(vec!["ok".to_string(), bad.to_string()]);
            let err = gen_script(&pkg).unwrap_err().to_string();
            assert!(err.contains("provides"), "{err}");
            assert!(err.contains(&format!("{bad:?}")), "{err}");
        }
    }

    #[test]
    fn scriptlet_survives_the_shell_heredoc() {
        // The heredoc is single-quoted, so $ and backticks reach rpm verbatim.
        let mut pkg = bare_pkg();
        pkg.post_install = Some("echo $HOME `id` $(whoami)".to_string());
        let script = gen_script(&pkg).unwrap();
        assert!(script.contains("<< 'SPEC_EOF'"));
        assert!(spec_of(&script).contains("echo $HOME `id` $(whoami)"));

        // A line equal to the terminator would end the heredoc early: rejected.
        let mut pkg = bare_pkg();
        pkg.post_install = Some("echo hi\nSPEC_EOF\ntouch /pwned".to_string());
        let err = gen_script(&pkg).unwrap_err().to_string();
        assert!(err.contains("SPEC_EOF"), "{err}");
    }

    #[test]
    fn scriptlet_line_starting_with_percent_is_rejected() {
        // rpmbuild expands `%%files` back to `%files` before it looks for section
        // headers, so doubling the `%` does not keep the line inside the scriptlet.
        for bad in [
            "echo a\n%files\n/usr/bin/x",
            "echo a\n%%build\necho b",
            "echo a\n   %package -n evil",
            "echo a\n\t%if 1",
        ] {
            let mut pkg = bare_pkg();
            pkg.post_install = Some(bad.to_string());
            let err = gen_script(&pkg).unwrap_err().to_string();
            assert!(err.contains("%post"), "{err}");
            assert!(err.contains("starts with '%'"), "{err}");
        }
        // A `%` inside a line stays legal and is escaped.
        let mut pkg = bare_pkg();
        pkg.post_install = Some("printf '%s\\n' x".to_string());
        assert!(spec_of(&gen_script(&pkg).unwrap()).contains("printf '%%s\\n' x"));
    }

    #[test]
    fn blank_provides_entry_is_rejected_by_name() {
        for bad in ["", "   "] {
            let mut pkg = bare_pkg();
            pkg.provides = Some(vec!["ok".to_string(), bad.to_string()]);
            let err = gen_script(&pkg).unwrap_err().to_string();
            assert!(err.contains("provides"), "{err}");
            assert!(err.contains("empty"), "{err}");
        }
    }
}
