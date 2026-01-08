// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

use anyhow::{Context, Result};

use std::fs;
use std::path::PathBuf;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};
// Note: Stamp imports removed - we no longer validate build stamps for packaging
// since we now package src_dir instead of built sysroot
use crate::utils::target::resolve_target_required;

/// Command to package an extension sysroot into an RPM
pub struct ExtPackageCommand {
    pub config_path: String,
    pub extension: String,
    pub target: Option<String>,
    pub output_dir: Option<String>,
    pub verbose: bool,
    pub container_args: Option<Vec<String>>,
    #[allow(dead_code)]
    pub dnf_args: Option<Vec<String>>,
    /// Note: no_stamps is kept for API compatibility but is not used for ext package
    /// since we now package src_dir directly without requiring build stamps.
    #[allow(dead_code)]
    pub no_stamps: bool,
    pub sdk_arch: Option<String>,
}

impl ExtPackageCommand {
    pub fn new(
        config_path: String,
        extension: String,
        target: Option<String>,
        output_dir: Option<String>,
        verbose: bool,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            extension,
            target,
            output_dir,
            verbose,
            container_args,
            dnf_args,
            no_stamps: false,
            sdk_arch: None,
        }
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

    pub async fn execute(&self) -> Result<()> {
        // Load composed configuration (includes remote extension configs)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;
        let config = &composed.config;
        let parsed = &composed.merged_value;

        // Resolve target
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // With the new src_dir packaging approach, we no longer require
        // ext_install and ext_build stamps. We're packaging the source directory,
        // not the built sysroot. The consumer will build the extension themselves.
        //
        // Issue a warning to remind users to test builds before packaging.
        print_warning(
            "Packaging extension source directory. It is recommended to run \
             'avocado ext build' before packaging to verify the extension builds correctly.",
            OutputLevel::Normal,
        );

        // Note: We no longer need to parse SDK dependencies since they're merged
        // from the extension's config when it's installed

        // Find extension using comprehensive lookup
        let extension_location = config
            .find_extension_in_dependency_tree(&self.config_path, &self.extension, &target)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Get the config path where this extension is actually defined
        let ext_config_path = match &extension_location {
            ExtensionLocation::Local { config_path, .. } => config_path.clone(),
            ExtensionLocation::External { config_path, .. } => config_path.clone(),
            ExtensionLocation::Remote { name, .. } => {
                // Remote extensions are installed to $AVOCADO_PREFIX/includes/<name>/
                let ext_install_path =
                    config.get_extension_install_path(&self.config_path, name, &target);
                ext_install_path
                    .join("avocado.yaml")
                    .to_string_lossy()
                    .to_string()
            }
        };

        if self.verbose {
            match &extension_location {
                ExtensionLocation::Local { name, config_path } => {
                    print_info(
                        &format!("Found local extension '{name}' in config '{config_path}'"),
                        OutputLevel::Normal,
                    );
                }
                ExtensionLocation::External { name, config_path } => {
                    print_info(
                        &format!("Found external extension '{name}' in config '{config_path}'"),
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

        // Get extension configuration from the composed/merged config
        // For remote extensions, this comes from the merged remote extension config (already read via container)
        // For local extensions, this uses get_merged_ext_config which reads from the file
        let ext_config = match &extension_location {
            ExtensionLocation::Remote { .. } => {
                // Use the already-merged config from `parsed` which contains remote extension configs
                // Then apply target-specific overrides manually
                let ext_section = parsed.get("ext").and_then(|ext| ext.get(&self.extension));
                if let Some(ext_val) = ext_section {
                    let base_ext = ext_val.clone();
                    // Check for target-specific override within this extension
                    let target_override = ext_val.get(&target).cloned();
                    if let Some(override_val) = target_override {
                        // Merge target override into base, filtering out other target sections
                        Some(config.merge_target_override(base_ext, override_val, &target))
                    } else {
                        Some(base_ext)
                    }
                } else {
                    None
                }
            }
            ExtensionLocation::Local { config_path, .. } => {
                // For local extensions, read from the file with proper target merging
                config.get_merged_ext_config(&self.extension, &target, config_path)?
            }
            #[allow(deprecated)]
            ExtensionLocation::External { config_path, .. } => {
                // For deprecated external configs, read from the file
                config.get_merged_ext_config(&self.extension, &target, config_path)?
            }
        }
        .ok_or_else(|| {
            anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
        })?;

        // Also get the raw (unmerged) extension config to find all target-specific overlays
        // For remote extensions, use the parsed config; for local, read from file
        let raw_ext_config = match &extension_location {
            ExtensionLocation::Remote { .. } => parsed
                .get("ext")
                .and_then(|ext| ext.get(&self.extension))
                .cloned(),
            _ => self.get_raw_extension_config(&ext_config_path)?,
        };

        // Extract RPM metadata with defaults
        let rpm_metadata = self.extract_rpm_metadata(&ext_config, &target)?;

        // Determine which files to package
        // Pass both merged config (for package_files) and raw config (for all target overlays)
        let package_files = self.get_package_files(&ext_config, raw_ext_config.as_ref());

        if self.verbose {
            print_info(
                &format!(
                    "Packaging extension '{}' v{}-{}",
                    self.extension, rpm_metadata.version, rpm_metadata.release
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Package files: {package_files:?}"),
                OutputLevel::Normal,
            );
        }

        // Create main RPM package in container
        // This packages the extension's src_dir (directory containing avocado.yaml)
        let output_path = self
            .create_rpm_package_in_container(
                &rpm_metadata,
                config,
                &target,
                &ext_config_path,
                &package_files,
            )
            .await?;

        print_success(
            &format!(
                "Successfully created RPM package: {}",
                output_path.display()
            ),
            OutputLevel::Normal,
        );

        // Note: SDK dependencies are now merged from the extension's config when installed,
        // so we no longer need to create a separate SDK package.

        Ok(())
    }

    /// Get the raw (unmerged) extension configuration from the config file.
    ///
    /// This is used to find all target-specific overlays that should be included
    /// in the package (since the package is noarch and needs all target overlays).
    fn get_raw_extension_config(&self, ext_config_path: &str) -> Result<Option<serde_yaml::Value>> {
        let content = fs::read_to_string(ext_config_path)
            .with_context(|| format!("Failed to read config file: {ext_config_path}"))?;

        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {ext_config_path}"))?;

        // Get the ext section
        let ext_section = parsed.get("ext");
        if ext_section.is_none() {
            return Ok(None);
        }

        // Get this specific extension's config
        Ok(ext_section
            .and_then(|ext| ext.get(&self.extension))
            .cloned())
    }

    /// Extract overlay directory from an overlay configuration value.
    fn extract_overlay_dir(overlay_value: &serde_yaml::Value) -> Option<String> {
        if let Some(overlay_dir) = overlay_value.as_str() {
            // Simple string format: overlay = "directory"
            Some(overlay_dir.to_string())
        } else if let Some(overlay_table) = overlay_value.as_mapping() {
            // Table format: overlay = { dir = "directory", ... }
            overlay_table
                .get("dir")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    }

    /// Determine which files to package based on the extension configuration.
    ///
    /// If `package_files` is specified in the extension config, use those patterns.
    /// Otherwise, default to:
    /// - The avocado config file (avocado.yaml, avocado.yml, or avocado.toml)
    /// - All overlay directories (base level and target-specific)
    ///
    /// # Arguments
    /// * `ext_config` - The merged extension config (for package_files check)
    /// * `raw_ext_config` - The raw unmerged extension config (to find all target-specific overlays)
    fn get_package_files(
        &self,
        ext_config: &serde_yaml::Value,
        raw_ext_config: Option<&serde_yaml::Value>,
    ) -> Vec<String> {
        // Check if package_files is explicitly defined
        if let Some(package_files) = ext_config.get("package_files") {
            if let Some(files_array) = package_files.as_sequence() {
                let files: Vec<String> = files_array
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if !files.is_empty() {
                    return files;
                }
            }
        }

        // Default behavior: avocado.yaml + all overlay directories
        let mut default_files = vec!["avocado.yaml".to_string()];
        let mut seen_overlays = std::collections::HashSet::new();

        // If we have the raw extension config, scan for all overlays
        if let Some(raw_config) = raw_ext_config {
            if let Some(mapping) = raw_config.as_mapping() {
                for (key, value) in mapping {
                    // Check if this is the base-level overlay
                    if key.as_str() == Some("overlay") {
                        if let Some(overlay_dir) = Self::extract_overlay_dir(value) {
                            if seen_overlays.insert(overlay_dir.clone()) {
                                default_files.push(overlay_dir);
                            }
                        }
                    }
                    // Check if this is a target-specific section with an overlay
                    else if let Some(target_config) = value.as_mapping() {
                        if let Some(overlay_value) = target_config.get("overlay") {
                            if let Some(overlay_dir) = Self::extract_overlay_dir(overlay_value) {
                                if seen_overlays.insert(overlay_dir.clone()) {
                                    default_files.push(overlay_dir);
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // Fallback: just check the merged config for overlay (current target only)
            if let Some(overlay) = ext_config.get("overlay") {
                if let Some(overlay_dir) = Self::extract_overlay_dir(overlay) {
                    default_files.push(overlay_dir);
                }
            }
        }

        default_files
    }

    /// Extract RPM metadata from extension configuration with defaults
    fn extract_rpm_metadata(
        &self,
        ext_config: &serde_yaml::Value,
        _target: &str, // Not used - extensions default to noarch
    ) -> Result<RpmMetadata> {
        // Version is required
        let version = ext_config
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Extension '{}' is missing required 'version' field for RPM packaging",
                    self.extension
                )
            })?
            .to_string();

        // Validate semver format
        Self::validate_semver(&version).with_context(|| {
            format!(
                "Extension '{}' has invalid version '{}'. Version must be in semantic versioning format (e.g., '1.0.0', '2.1.3')",
                self.extension, version
            )
        })?;

        // Generate defaults
        let name = self.extension.clone();
        let release = ext_config
            .get("release")
            .and_then(|v| v.as_str())
            .unwrap_or("r0")
            .to_string();

        let summary = ext_config
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.generate_summary_from_name(&name));

        let description = ext_config
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.generate_description_from_name(&name));

        let license = ext_config
            .get("license")
            .and_then(|v| v.as_str())
            .unwrap_or("Unspecified")
            .to_string();

        // Default to noarch for extension source packages since they contain
        // configs/code, not compiled binaries. Can be overridden in ext config.
        let arch = ext_config
            .get("arch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "noarch".to_string());

        let vendor = ext_config
            .get("vendor")
            .and_then(|v| v.as_str())
            .unwrap_or("Unspecified")
            .to_string();

        let url = ext_config
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let group = "system-extension".to_string();

        Ok(RpmMetadata {
            name,
            version,
            release,
            summary,
            description,
            license,
            arch,
            vendor,
            group,
            url,
        })
    }

    /// Generate summary from extension name
    fn generate_summary_from_name(&self, name: &str) -> String {
        // Convert kebab-case to title case
        let words: Vec<&str> = name.split('-').collect();
        let title_case: Vec<String> = words
            .iter()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            })
            .collect();

        format!("{} system extension", title_case.join(" "))
    }

    /// Generate description from extension name
    fn generate_description_from_name(&self, name: &str) -> String {
        format!("System extension package for {name}")
    }

    /// Validate semantic versioning format (X.Y.Z where X, Y, Z are non-negative integers)
    fn validate_semver(version: &str) -> Result<()> {
        let parts: Vec<&str> = version.split('.').collect();

        if parts.len() < 3 {
            return Err(anyhow::anyhow!(
                "Version must follow semantic versioning format with at least MAJOR.MINOR.PATCH components (e.g., '1.0.0', '2.1.3')"
            ));
        }

        // Validate the first 3 components (MAJOR.MINOR.PATCH)
        for (i, part) in parts.iter().take(3).enumerate() {
            // Handle pre-release and build metadata (e.g., "1.0.0-alpha" or "1.0.0+build")
            let component = part.split(&['-', '+'][..]).next().unwrap_or(part);

            component.parse::<u32>().with_context(|| {
                let component_name = match i {
                    0 => "MAJOR",
                    1 => "MINOR",
                    2 => "PATCH",
                    _ => "component",
                };
                format!(
                    "{component_name} version component '{component}' must be a non-negative integer in semantic versioning format"
                )
            })?;
        }

        Ok(())
    }

    /// Create the RPM package containing the extension's src_dir
    ///
    /// The package root (/) maps to the extension's src_dir contents.
    /// This allows the extension to be installed to $AVOCADO_PREFIX/includes/<ext_name>/
    /// and its config merged into the main config.
    ///
    /// # Arguments
    /// * `metadata` - RPM metadata for the package
    /// * `config` - The avocado configuration
    /// * `target` - The target architecture
    /// * `ext_config_path` - Path to the extension's config file
    /// * `package_files` - List of files/directories to package (supports glob patterns like * and **)
    async fn create_rpm_package_in_container(
        &self,
        metadata: &RpmMetadata,
        config: &Config,
        target: &str,
        ext_config_path: &str,
        package_files: &[String],
    ) -> Result<PathBuf> {
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the volume state
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&cwd).await?;

        // Determine the extension's src_dir (directory containing avocado.yaml)
        let ext_src_dir = std::path::Path::new(ext_config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_string_lossy()
            .to_string();

        // Convert to container path (relative paths become /opt/src/<path>)
        let container_src_dir = if ext_src_dir.starts_with('/') {
            ext_src_dir.clone()
        } else {
            format!("/opt/src/{ext_src_dir}")
        };

        // Create the RPM filename
        let rpm_filename = format!(
            "{}-{}-{}.{}.rpm",
            metadata.name, metadata.version, metadata.release, metadata.arch
        );

        // Convert package_files to a space-separated string for the shell script
        let package_files_str = package_files.join(" ");

        // Create RPM using rpmbuild in container
        // Package root (/) maps to the extension's src_dir contents
        let rpm_build_script = format!(
            r#"
set -e

# Extension source directory
EXT_SRC_DIR="{container_src_dir}"

# Package files patterns (may contain globs like * and **)
PACKAGE_FILES="{package_files_str}"

# Ensure output directory exists
mkdir -p $AVOCADO_PREFIX/output/extensions

# Check if extension source directory exists
if [ ! -d "$EXT_SRC_DIR" ]; then
    echo "Extension source directory not found: $EXT_SRC_DIR"
    exit 1
fi

# Check for avocado config file
if [ ! -f "$EXT_SRC_DIR/avocado.yaml" ] && [ ! -f "$EXT_SRC_DIR/avocado.yml" ] && [ ! -f "$EXT_SRC_DIR/avocado.toml" ]; then
    echo "No avocado.yaml/yml/toml found in $EXT_SRC_DIR"
    exit 1
fi

# Create temporary directory for RPM build
TMPDIR=$(mktemp -d)
STAGING_DIR="$TMPDIR/staging"
mkdir -p "$STAGING_DIR"
cd "$TMPDIR"

# Create directory structure for rpmbuild
mkdir -p BUILD RPMS SOURCES SPECS SRPMS

# Enable globstar for ** pattern support
shopt -s globstar nullglob

# Copy files matching patterns to staging directory
cd "$EXT_SRC_DIR"
FILE_COUNT=0
for pattern in $PACKAGE_FILES; do
    # Expand the glob pattern
    for file in $pattern; do
        if [ -e "$file" ]; then
            # Create parent directory in staging and copy
            parent_dir=$(dirname "$file")
            if [ "$parent_dir" != "." ]; then
                mkdir -p "$STAGING_DIR/$parent_dir"
            fi
            cp -rp "$file" "$STAGING_DIR/$file"
            if [ -f "$file" ]; then
                FILE_COUNT=$((FILE_COUNT + 1))
            elif [ -d "$file" ]; then
                dir_files=$(find "$file" -type f | wc -l)
                FILE_COUNT=$((FILE_COUNT + dir_files))
            fi
        fi
    done
done
cd "$TMPDIR"

echo "Creating RPM with $FILE_COUNT files from source directory..."

if [ "$FILE_COUNT" -eq 0 ]; then
    echo "No files matched the package_files patterns: $PACKAGE_FILES"
    exit 1
fi

# Create spec file
# Package root (/) maps to the extension's src_dir
cat > SPECS/package.spec << SPEC_EOF
%define _buildhost reproducible
AutoReqProv: no

Name: {name}
Version: {version}
Release: {release}
Summary: {summary}
License: {license}
Vendor: {vendor}
Group: {group}{url_line}

%description
{description}

%files
/*

%prep
# No prep needed

%build
# No build needed

%install
mkdir -p %{{buildroot}}
# Copy staged files to buildroot root
# This allows installation to \$AVOCADO_PREFIX/includes/<ext_name>/
cp -rp "$STAGING_DIR"/* %{{buildroot}}/

%clean
# Skip clean section - not needed for our use case

%changelog
SPEC_EOF

# Build the RPM with custom architecture target
rpmbuild --define "_topdir $TMPDIR" --define "_arch {arch}" --target {arch} -bb SPECS/package.spec

# Move RPM to output directory
mv RPMS/{arch}/*.rpm $AVOCADO_PREFIX/output/extensions/{rpm_filename} || {{
    mv RPMS/*/*.rpm $AVOCADO_PREFIX/output/extensions/{rpm_filename} 2>/dev/null || {{
        echo "Failed to find built RPM"
        exit 1
    }}
}}

echo "RPM created successfully: $AVOCADO_PREFIX/output/extensions/{rpm_filename}"

# Cleanup
rm -rf "$TMPDIR"
"#,
            name = metadata.name,
            version = metadata.version,
            release = metadata.release,
            summary = metadata.summary,
            license = metadata.license,
            vendor = metadata.vendor,
            group = metadata.group,
            url_line = if let Some(url) = &metadata.url {
                format!("\nURL: {url}")
            } else {
                String::new()
            },
            description = metadata.description,
            arch = metadata.arch,
            rpm_filename = rpm_filename,
            container_src_dir = container_src_dir,
            package_files_str = package_files_str,
        );

        // Run the RPM build in the container
        let container_helper = SdkContainer::new();
        let run_config = crate::utils::container::RunConfig {
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

        if self.verbose {
            print_info("Creating RPM package in container...", OutputLevel::Normal);
        }

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!("Failed to create RPM package in container"));
        }

        // RPM is now created in the container at $AVOCADO_PREFIX/output/extensions/{rpm_filename}
        let container_rpm_path = format!("/opt/_avocado/{target}/output/extensions/{rpm_filename}");

        // If --out is specified, copy the RPM to the host
        if let Some(output_dir) = &self.output_dir {
            self.copy_rpm_to_host(
                &volume_state.volume_name,
                &container_rpm_path,
                output_dir,
                &rpm_filename,
                container_image,
            )
            .await?;

            // Return the host path (canonicalized for clean display)
            let host_output_path = if output_dir.starts_with('/') {
                // Absolute path
                PathBuf::from(output_dir).join(&rpm_filename)
            } else {
                // Relative path from current directory
                std::env::current_dir()?
                    .join(output_dir)
                    .join(&rpm_filename)
            };

            // Canonicalize the path to resolve . and .. components for clean display
            let canonical_path = host_output_path.canonicalize().unwrap_or(host_output_path);
            Ok(canonical_path)
        } else {
            // Return the container path for informational purposes
            Ok(PathBuf::from(container_rpm_path))
        }
    }

    /// Copy the RPM from the container to the host using docker cp
    async fn copy_rpm_to_host(
        &self,
        volume_name: &str,
        container_rpm_path: &str,
        output_dir: &str,
        rpm_filename: &str,
        _container_image: &str,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Copying RPM to host: {output_dir}"),
                OutputLevel::Normal,
            );
        }

        // Create a temporary container to access the volume (following checkout pattern)
        let temp_container_id = self.create_temp_container(volume_name).await?;

        // Determine the output path on host
        let host_output_dir = if output_dir.starts_with('/') {
            // Absolute path
            PathBuf::from(output_dir)
        } else {
            // Relative path from current directory
            std::env::current_dir()?.join(output_dir)
        };

        // Create output directory on host
        fs::create_dir_all(&host_output_dir)?;

        let docker_cp_source = format!("{temp_container_id}:{container_rpm_path}");
        let docker_cp_dest = host_output_dir.join(rpm_filename);

        if self.verbose {
            print_info(
                &format!(
                    "Docker cp: {docker_cp_source} -> {}",
                    docker_cp_dest.display()
                ),
                OutputLevel::Normal,
            );
        }

        // Use tokio::process::Command directly like checkout does
        let copy_output = tokio::process::Command::new("docker")
            .arg("cp")
            .arg(&docker_cp_source)
            .arg(&docker_cp_dest)
            .output()
            .await
            .context("Failed to execute docker cp")?;

        // Clean up temporary container
        let _ = tokio::process::Command::new("docker")
            .arg("rm")
            .arg("-f")
            .arg(&temp_container_id)
            .output()
            .await;

        if !copy_output.status.success() {
            let stderr = String::from_utf8_lossy(&copy_output.stderr);
            return Err(anyhow::anyhow!("Docker cp failed: {stderr}"));
        }

        if self.verbose {
            print_info(
                &format!("RPM copied to: {}", docker_cp_dest.display()),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Create a temporary container to access the volume (following checkout pattern)
    async fn create_temp_container(&self, volume_name: &str) -> Result<String> {
        let output = tokio::process::Command::new("docker")
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

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }
}

/// RPM metadata structure
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
    group: String,
    url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_summary_from_name() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        assert_eq!(
            cmd.generate_summary_from_name("web-server"),
            "Web Server system extension"
        );
        assert_eq!(
            cmd.generate_summary_from_name("my-app"),
            "My App system extension"
        );
        assert_eq!(
            cmd.generate_summary_from_name("database-backend"),
            "Database Backend system extension"
        );
        assert_eq!(
            cmd.generate_summary_from_name("simple"),
            "Simple system extension"
        );
    }

    #[test]
    fn test_generate_description_from_name() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        assert_eq!(
            cmd.generate_description_from_name("web-server"),
            "System extension package for web-server"
        );
        assert_eq!(
            cmd.generate_description_from_name("my-app"),
            "System extension package for my-app"
        );
    }

    #[test]
    fn test_extract_rpm_metadata_minimal() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-extension".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        ext_config.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        let metadata = cmd
            .extract_rpm_metadata(&ext_config, "x86_64-unknown-linux-gnu")
            .unwrap();

        assert_eq!(metadata.name, "test-extension");
        assert_eq!(metadata.version, "1.0.0");
        assert_eq!(metadata.release, "r0");
        assert_eq!(metadata.summary, "Test Extension system extension");
        assert_eq!(
            metadata.description,
            "System extension package for test-extension"
        );
        assert_eq!(metadata.license, "Unspecified");
        assert_eq!(metadata.arch, "noarch"); // Extension source packages default to noarch
        assert_eq!(metadata.vendor, "Unspecified");
        assert_eq!(metadata.group, "system-extension");
        assert_eq!(metadata.url, None);
    }

    #[test]
    fn test_extract_rpm_metadata_full() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "web-server".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = ext_config.as_mapping_mut().unwrap();

        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("2.1.3".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("release".to_string()),
            serde_yaml::Value::String("2".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("summary".to_string()),
            serde_yaml::Value::String("Custom web server".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("description".to_string()),
            serde_yaml::Value::String("A custom web server extension".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("license".to_string()),
            serde_yaml::Value::String("MIT".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("arch".to_string()),
            serde_yaml::Value::String("noarch".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("vendor".to_string()),
            serde_yaml::Value::String("Acme Corp".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("url".to_string()),
            serde_yaml::Value::String("https://example.com".to_string()),
        );

        let metadata = cmd
            .extract_rpm_metadata(&ext_config, "aarch64-unknown-linux-gnu")
            .unwrap();

        assert_eq!(metadata.name, "web-server");
        assert_eq!(metadata.version, "2.1.3");
        assert_eq!(metadata.release, "2");
        assert_eq!(metadata.summary, "Custom web server");
        assert_eq!(metadata.description, "A custom web server extension");
        assert_eq!(metadata.license, "MIT");
        assert_eq!(metadata.arch, "noarch"); // Explicit arch overrides generated
        assert_eq!(metadata.vendor, "Acme Corp");
        assert_eq!(metadata.group, "system-extension");
        assert_eq!(metadata.url, Some("https://example.com".to_string()));
    }

    #[test]
    fn test_extract_rpm_metadata_missing_version() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-extension".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let result = cmd.extract_rpm_metadata(&ext_config, "x86_64-unknown-linux-gnu");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing required 'version' field"));
    }

    #[test]
    fn test_arch_defaults_to_noarch_for_all_targets() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        ext_config.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        // Extension source packages should default to noarch regardless of target
        // since they contain configs/code, not compiled binaries
        let targets = vec![
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "riscv64-unknown-linux-gnu",
            "i686-unknown-linux-gnu",
            "armv7-unknown-linux-gnueabihf",
            "raspberrypi4",
        ];

        for target in targets {
            let metadata = cmd.extract_rpm_metadata(&ext_config, target).unwrap();
            assert_eq!(
                metadata.arch, "noarch",
                "Extension should default to noarch for target: {target}"
            );
        }
    }

    // ========================================================================
    // Note: Stamp Dependency Tests Removed
    // ========================================================================
    // The stamp validation tests have been removed because ext package now
    // packages the extension's src_dir directly instead of the built sysroot.
    // This means we no longer require ext_install and ext_build stamps before
    // packaging - the consumer will build the extension themselves.
    //
    // The old behavior required:
    // - SDK install stamp
    // - Extension install stamp
    // - Extension build stamp
    //
    // The new behavior only requires the extension's avocado.yaml to exist
    // in its src_dir.

    #[test]
    fn test_package_with_no_stamps_flag() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Default should have stamps enabled (though not used for src_dir packaging)
        assert!(!cmd.no_stamps);

        // Test with_no_stamps builder
        let cmd = cmd.with_no_stamps(true);
        assert!(cmd.no_stamps);
    }

    #[test]
    fn test_get_package_files_default_no_overlay() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Config without package_files or overlay - should default to just avocado.yaml
        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        ext_config.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        let files = cmd.get_package_files(&ext_config, None);
        assert_eq!(files, vec!["avocado.yaml".to_string()]);
    }

    #[test]
    fn test_get_package_files_default_with_overlay_string() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Config with overlay as string - should include avocado.yaml and overlay dir
        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = ext_config.as_mapping_mut().unwrap();
        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("overlay".to_string()),
            serde_yaml::Value::String("my-overlay".to_string()),
        );

        // Use the same config as raw config to test overlay extraction
        let files = cmd.get_package_files(&ext_config, Some(&ext_config));
        assert_eq!(
            files,
            vec!["avocado.yaml".to_string(), "my-overlay".to_string()]
        );
    }

    #[test]
    fn test_get_package_files_default_with_overlay_table() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Config with overlay as table { dir = "..." } - should include avocado.yaml and overlay dir
        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = ext_config.as_mapping_mut().unwrap();
        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        let mut overlay_table = serde_yaml::Mapping::new();
        overlay_table.insert(
            serde_yaml::Value::String("dir".to_string()),
            serde_yaml::Value::String("overlays/prod".to_string()),
        );
        overlay_table.insert(
            serde_yaml::Value::String("mode".to_string()),
            serde_yaml::Value::String("opaque".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("overlay".to_string()),
            serde_yaml::Value::Mapping(overlay_table),
        );

        // Use the same config as raw config to test overlay extraction
        let files = cmd.get_package_files(&ext_config, Some(&ext_config));
        assert_eq!(
            files,
            vec!["avocado.yaml".to_string(), "overlays/prod".to_string()]
        );
    }

    #[test]
    fn test_get_package_files_explicit_list() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Config with explicit package_files list
        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = ext_config.as_mapping_mut().unwrap();
        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        let package_files = vec![
            serde_yaml::Value::String("avocado.yaml".to_string()),
            serde_yaml::Value::String("config/**".to_string()),
            serde_yaml::Value::String("scripts/*.sh".to_string()),
            serde_yaml::Value::String("README.md".to_string()),
        ];
        config_map.insert(
            serde_yaml::Value::String("package_files".to_string()),
            serde_yaml::Value::Sequence(package_files),
        );

        // Also add overlay - should be ignored when package_files is set
        config_map.insert(
            serde_yaml::Value::String("overlay".to_string()),
            serde_yaml::Value::String("my-overlay".to_string()),
        );

        let files = cmd.get_package_files(&ext_config, Some(&ext_config));
        assert_eq!(
            files,
            vec![
                "avocado.yaml".to_string(),
                "config/**".to_string(),
                "scripts/*.sh".to_string(),
                "README.md".to_string(),
            ]
        );
    }

    #[test]
    fn test_get_package_files_empty_list_uses_default() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Config with empty package_files list - should fall back to default
        let mut ext_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = ext_config.as_mapping_mut().unwrap();
        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("package_files".to_string()),
            serde_yaml::Value::Sequence(vec![]),
        );

        let files = cmd.get_package_files(&ext_config, None);
        assert_eq!(files, vec!["avocado.yaml".to_string()]);
    }

    #[test]
    fn test_get_package_files_with_target_specific_overlays() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        );

        // Create a raw config that simulates target-specific overlays
        // like: ext.test-ext.reterminal.overlay and ext.test-ext.reterminal-dm.overlay
        let mut raw_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let config_map = raw_config.as_mapping_mut().unwrap();

        config_map.insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        );

        // Target: reterminal with overlay
        let mut reterminal_config = serde_yaml::Mapping::new();
        reterminal_config.insert(
            serde_yaml::Value::String("overlay".to_string()),
            serde_yaml::Value::String("overlays/reterminal".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("reterminal".to_string()),
            serde_yaml::Value::Mapping(reterminal_config),
        );

        // Target: reterminal-dm with overlay
        let mut reterminal_dm_config = serde_yaml::Mapping::new();
        reterminal_dm_config.insert(
            serde_yaml::Value::String("overlay".to_string()),
            serde_yaml::Value::String("overlays/reterminal-dm".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("reterminal-dm".to_string()),
            serde_yaml::Value::Mapping(reterminal_dm_config),
        );

        // Target: icam-540 without overlay (should not add anything)
        let mut icam_config = serde_yaml::Mapping::new();
        icam_config.insert(
            serde_yaml::Value::String("some_other_setting".to_string()),
            serde_yaml::Value::String("value".to_string()),
        );
        config_map.insert(
            serde_yaml::Value::String("icam-540".to_string()),
            serde_yaml::Value::Mapping(icam_config),
        );

        // Merged config (for a specific target, but package_files not set)
        let merged_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let files = cmd.get_package_files(&merged_config, Some(&raw_config));

        // Should include avocado.yaml and both target-specific overlays
        assert!(files.contains(&"avocado.yaml".to_string()));
        assert!(files.contains(&"overlays/reterminal".to_string()));
        assert!(files.contains(&"overlays/reterminal-dm".to_string()));
        assert_eq!(files.len(), 3);
    }
}
