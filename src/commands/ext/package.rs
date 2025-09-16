use anyhow::{Context, Result};

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_info, print_success, OutputLevel};
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
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Resolve target using unified target resolution logic
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Find extension using comprehensive lookup
        let extension_location = config
            .find_extension_in_dependency_tree(&self.config_path, &self.extension, &target)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

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
            }
        }

        // Get extension configuration (for now, we still need to get it from local config for package logic)
        let ext_config = parsed
            .get("ext")
            .and_then(|ext| ext.get(&self.extension))
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in local configuration. External extension packaging not yet supported.", self.extension)
            })?;

        // Extract RPM metadata with defaults
        let rpm_metadata = self.extract_rpm_metadata(ext_config, &target)?;

        if self.verbose {
            print_info(
                &format!(
                    "Packaging extension '{}' v{}-{}",
                    self.extension, rpm_metadata.version, rpm_metadata.release
                ),
                OutputLevel::Normal,
            );
        }

        // Create main RPM package in container
        let output_path = self
            .create_rpm_package_in_container(&rpm_metadata, &config, &parsed, &target)
            .await?;

        print_success(
            &format!(
                "Successfully created RPM package: {}",
                output_path.display()
            ),
            OutputLevel::Normal,
        );

        // Check if extension has SDK dependencies and create SDK package if needed
        let sdk_dependencies = self.get_extension_sdk_dependencies(&config, &content, &target)?;
        if !sdk_dependencies.is_empty() {
            if self.verbose {
                print_info(
                    &format!(
                        "Extension '{}' has SDK dependencies, creating SDK package...",
                        self.extension
                    ),
                    OutputLevel::Normal,
                );
            }

            let sdk_output_path = self
                .create_sdk_rpm_package_in_container(
                    &rpm_metadata,
                    &config,
                    &parsed,
                    &sdk_dependencies,
                    &target,
                )
                .await?;

            print_success(
                &format!(
                    "Successfully created SDK RPM package: {}",
                    sdk_output_path.display()
                ),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Extract RPM metadata from extension configuration with defaults
    fn extract_rpm_metadata(&self, ext_config: &toml::Value, target: &str) -> Result<RpmMetadata> {
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

        let arch = ext_config
            .get("arch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.generate_arch_from_target(target));

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

    /// Generate architecture from target by replacing dashes with underscores
    fn generate_arch_from_target(&self, target: &str) -> String {
        format!("avocado_{}", target.replace('-', "_"))
    }

    /// Create the RPM package inside the container at $AVOCADO_PREFIX/output/extensions
    async fn create_rpm_package_in_container(
        &self,
        metadata: &RpmMetadata,
        config: &Config,
        parsed: &toml::Value,
        target: &str,
    ) -> Result<PathBuf> {
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the volume state
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&cwd).await?;

        // Create the RPM filename
        let rpm_filename = format!(
            "{}-{}-{}.{}.rpm",
            metadata.name, metadata.version, metadata.release, metadata.arch
        );

        // Create RPM using rpmbuild in container
        let rpm_build_script = format!(
            r#"
# Ensure output directory exists
mkdir -p $AVOCADO_PREFIX/output/extensions

# Check if extension sysroot exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/{}" ]; then
    echo "Extension sysroot not found: $AVOCADO_EXT_SYSROOTS/{}"
    exit 1
fi

# Count files
FILE_COUNT=$(find "$AVOCADO_EXT_SYSROOTS/{}" -type f | wc -l)
echo "Creating RPM with $FILE_COUNT files..."

if [ "$FILE_COUNT" -eq 0 ]; then
    echo "No files found in sysroot"
    exit 1
fi

# Create temporary directory for RPM build
TMPDIR=$(mktemp -d)
cd "$TMPDIR"

# Create directory structure for rpmbuild
mkdir -p BUILD RPMS SOURCES SPECS SRPMS

# Create spec file
cat > SPECS/package.spec << 'SPEC_EOF'
%define _buildhost reproducible
%global _use_internal_dependency_generator 0

Name: {}
Version: {}
Release: {}
Summary: {}
License: {}
Vendor: {}
Group: {}{}

%description
{}

%files
/*

%prep
# No prep needed

%build
# No build needed

%install
mkdir -p %{{buildroot}}
cp -rp $AVOCADO_EXT_SYSROOTS/{}/* %{{buildroot}}/

%clean
# Skip clean section - not needed for our use case

%changelog
SPEC_EOF

# Build the RPM with custom architecture target and define the arch macro
rpmbuild --define "_topdir $TMPDIR" --define "_arch {}" --target {} -bb SPECS/package.spec

# Move RPM to output directory
mv RPMS/{}/*.rpm $AVOCADO_PREFIX/output/extensions/{} || {{
    mv RPMS/*/*.rpm $AVOCADO_PREFIX/output/extensions/{} 2>/dev/null || {{
        echo "Failed to find built RPM"
        exit 1
    }}
}}

echo "RPM created successfully: $AVOCADO_PREFIX/output/extensions/{}"

# Cleanup
rm -rf "$TMPDIR"
"#,
            self.extension,
            self.extension,
            self.extension,
            metadata.name,
            metadata.version,
            metadata.release,
            metadata.summary,
            metadata.license,
            metadata.vendor,
            metadata.group,
            if let Some(url) = &metadata.url {
                format!("\nURL: {url}")
            } else {
                String::new()
            },
            metadata.description,
            self.extension,
            metadata.arch,
            metadata.arch,
            metadata.arch,
            rpm_filename,
            rpm_filename,
            rpm_filename,
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
            return Err(anyhow::anyhow!("Docker cp failed: {}", stderr));
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
                "Failed to create temporary container: {}",
                stderr
            ));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }

    /// Get SDK dependencies for the current extension
    fn get_extension_sdk_dependencies(
        &self,
        config: &Config,
        config_content: &str,
        target: &str,
    ) -> Result<HashMap<String, toml::Value>> {
        let extension_sdk_deps = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                config_content,
                Some(&self.config_path),
                Some(target),
            )?;

        // Return the SDK dependencies for this specific extension, or empty if none
        Ok(extension_sdk_deps
            .get(&self.extension)
            .cloned()
            .unwrap_or_default())
    }

    /// Create the SDK RPM package inside the container at $AVOCADO_PREFIX/output/extensions
    async fn create_sdk_rpm_package_in_container(
        &self,
        metadata: &RpmMetadata,
        config: &Config,
        parsed: &toml::Value,
        sdk_dependencies: &HashMap<String, toml::Value>,
        target: &str,
    ) -> Result<PathBuf> {
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the volume state
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&cwd).await?;

        // Create SDK RPM metadata with nativesdk- prefix and all_avocadosdk architecture
        let sdk_metadata = RpmMetadata {
            name: format!("nativesdk-{}", metadata.name),
            version: metadata.version.clone(),
            release: metadata.release.clone(),
            summary: format!("{} SDK dependencies", metadata.summary),
            description: format!("SDK dependencies for {}", metadata.description),
            license: metadata.license.clone(),
            arch: "all_avocadosdk".to_string(),
            vendor: metadata.vendor.clone(),
            group: metadata.group.clone(),
            url: metadata.url.clone(),
        };

        // Create the RPM filename
        let rpm_filename = format!(
            "{}-{}-{}.{}.rpm",
            sdk_metadata.name, sdk_metadata.version, sdk_metadata.release, sdk_metadata.arch
        );

        // Build dependency list for RPM spec
        let mut requires_list = Vec::new();
        for (dep_name, dep_value) in sdk_dependencies {
            let version_spec = match dep_value {
                toml::Value::String(version) if version == "*" => String::new(),
                toml::Value::String(version) => format!(" = {version}"),
                _ => String::new(),
            };
            requires_list.push(format!("{dep_name}{version_spec}"));
        }
        let requires_section = if requires_list.is_empty() {
            String::new()
        } else {
            format!("Requires: {}", requires_list.join(", "))
        };

        // Create SDK RPM using rpmbuild in container
        let rpm_build_script = format!(
            r#"
# Ensure output directory exists
mkdir -p $AVOCADO_PREFIX/output/extensions

# Create temporary directory for RPM build
TMPDIR=$(mktemp -d)
cd "$TMPDIR"

# Create directory structure for rpmbuild
mkdir -p BUILD RPMS SOURCES SPECS SRPMS

# Create spec file for SDK package (no files, only dependencies)
cat > SPECS/sdk-package.spec << 'SPEC_EOF'
%define _buildhost reproducible

Name: {}
Version: {}
Release: {}
Summary: {}
License: {}
Vendor: {}
Group: {}{}
{}

%description
{}

%files
# No files - this is a dependency-only package

%prep
# No prep needed

%build
# No build needed

%install
# No install needed - dependency-only package

%clean
# Skip clean section - not needed for our use case

%changelog
SPEC_EOF

# Build the RPM with custom architecture target and define the arch macro
rpmbuild --define "_topdir $TMPDIR" --define "_arch {}" --target {} -bb SPECS/sdk-package.spec

# Move RPM to output directory
mv RPMS/{}/*.rpm $AVOCADO_PREFIX/output/extensions/{} || {{
    mv RPMS/*/*.rpm $AVOCADO_PREFIX/output/extensions/{} 2>/dev/null || {{
        echo "Failed to find built SDK RPM"
        exit 1
    }}
}}

echo "SDK RPM created successfully: $AVOCADO_PREFIX/output/extensions/{}"

# Cleanup
rm -rf "$TMPDIR"
"#,
            sdk_metadata.name,
            sdk_metadata.version,
            sdk_metadata.release,
            sdk_metadata.summary,
            sdk_metadata.license,
            sdk_metadata.vendor,
            sdk_metadata.group,
            if let Some(url) = &sdk_metadata.url {
                format!("\nURL: {url}")
            } else {
                String::new()
            },
            requires_section,
            sdk_metadata.description,
            sdk_metadata.arch,
            sdk_metadata.arch,
            sdk_metadata.arch,
            rpm_filename,
            rpm_filename,
            rpm_filename,
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
            ..Default::default()
        };

        if self.verbose {
            print_info(
                "Creating SDK RPM package in container...",
                OutputLevel::Normal,
            );
        }

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to create SDK RPM package in container"
            ));
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
            "test.toml".to_string(),
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
            "test.toml".to_string(),
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
    fn test_generate_arch_from_target() {
        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        assert_eq!(
            cmd.generate_arch_from_target("x86_64-unknown-linux-gnu"),
            "avocado_x86_64_unknown_linux_gnu"
        );
        assert_eq!(
            cmd.generate_arch_from_target("aarch64-unknown-linux-gnu"),
            "avocado_aarch64_unknown_linux_gnu"
        );
        assert_eq!(
            cmd.generate_arch_from_target("riscv64-unknown-linux-gnu"),
            "avocado_riscv64_unknown_linux_gnu"
        );
        assert_eq!(
            cmd.generate_arch_from_target("i686-unknown-linux-gnu"),
            "avocado_i686_unknown_linux_gnu"
        );
    }

    #[test]
    fn test_extract_rpm_metadata_minimal() {
        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "test-extension".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = toml::Value::Table(toml::value::Table::new());
        ext_config.as_table_mut().unwrap().insert(
            "version".to_string(),
            toml::Value::String("1.0.0".to_string()),
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
        assert_eq!(metadata.arch, "avocado_x86_64_unknown_linux_gnu");
        assert_eq!(metadata.vendor, "Unspecified");
        assert_eq!(metadata.group, "system-extension");
        assert_eq!(metadata.url, None);
    }

    #[test]
    fn test_extract_rpm_metadata_full() {
        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "web-server".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = toml::Value::Table(toml::value::Table::new());
        let config_map = ext_config.as_table_mut().unwrap();

        config_map.insert(
            "version".to_string(),
            toml::Value::String("2.1.3".to_string()),
        );
        config_map.insert("release".to_string(), toml::Value::String("2".to_string()));
        config_map.insert(
            "summary".to_string(),
            toml::Value::String("Custom web server".to_string()),
        );
        config_map.insert(
            "description".to_string(),
            toml::Value::String("A custom web server extension".to_string()),
        );
        config_map.insert(
            "license".to_string(),
            toml::Value::String("MIT".to_string()),
        );
        config_map.insert(
            "arch".to_string(),
            toml::Value::String("noarch".to_string()),
        );
        config_map.insert(
            "vendor".to_string(),
            toml::Value::String("Acme Corp".to_string()),
        );
        config_map.insert(
            "url".to_string(),
            toml::Value::String("https://example.com".to_string()),
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
            "test.toml".to_string(),
            "test-extension".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let ext_config = toml::Value::Table(toml::value::Table::new());

        let result = cmd.extract_rpm_metadata(&ext_config, "x86_64-unknown-linux-gnu");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing required 'version' field"));
    }

    #[test]
    fn test_arch_generation_with_different_targets() {
        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        let mut ext_config = toml::Value::Table(toml::value::Table::new());
        ext_config.as_table_mut().unwrap().insert(
            "version".to_string(),
            toml::Value::String("1.0.0".to_string()),
        );

        // Test various target architectures
        let test_cases = vec![
            (
                "x86_64-unknown-linux-gnu",
                "avocado_x86_64_unknown_linux_gnu",
            ),
            (
                "aarch64-unknown-linux-gnu",
                "avocado_aarch64_unknown_linux_gnu",
            ),
            (
                "riscv64-unknown-linux-gnu",
                "avocado_riscv64_unknown_linux_gnu",
            ),
            ("i686-unknown-linux-gnu", "avocado_i686_unknown_linux_gnu"),
            (
                "armv7-unknown-linux-gnueabihf",
                "avocado_armv7_unknown_linux_gnueabihf",
            ),
        ];

        for (target, expected_arch) in test_cases {
            let metadata = cmd.extract_rpm_metadata(&ext_config, target).unwrap();
            assert_eq!(metadata.arch, expected_arch, "Failed for target: {target}");
        }
    }

    #[test]
    fn test_get_extension_sdk_dependencies_empty() {
        use crate::utils::config::Config;

        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        // Create a minimal config without SDK dependencies
        let config_content = r#"
[ext.test-ext]
version = "1.0.0"
"#;

        let config = Config::from_toml_value(&toml::from_str(config_content).unwrap()).unwrap();
        let sdk_deps = cmd
            .get_extension_sdk_dependencies(&config, config_content, "x86_64-unknown-linux-gnu")
            .unwrap();

        assert!(sdk_deps.is_empty());
    }

    #[test]
    fn test_get_extension_sdk_dependencies_with_deps() {
        use crate::utils::config::Config;

        let cmd = ExtPackageCommand::new(
            "test.toml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        );

        // Create a config with SDK dependencies
        let config_content = r#"
[ext.test-ext]
version = "1.0.0"

[ext.test-ext.sdk.dependencies]
nativesdk-avocado-hitl = "*"
nativesdk-openssh-ssh = "*"
nativesdk-rsync = "1.2.3"
"#;

        let config = Config::from_toml_value(&toml::from_str(config_content).unwrap()).unwrap();
        let sdk_deps = cmd
            .get_extension_sdk_dependencies(&config, config_content, "x86_64-unknown-linux-gnu")
            .unwrap();

        assert_eq!(sdk_deps.len(), 3);
        assert!(sdk_deps.contains_key("nativesdk-avocado-hitl"));
        assert!(sdk_deps.contains_key("nativesdk-openssh-ssh"));
        assert!(sdk_deps.contains_key("nativesdk-rsync"));

        // Check version values
        assert_eq!(
            sdk_deps["nativesdk-avocado-hitl"],
            toml::Value::String("*".to_string())
        );
        assert_eq!(
            sdk_deps["nativesdk-openssh-ssh"],
            toml::Value::String("*".to_string())
        );
        assert_eq!(
            sdk_deps["nativesdk-rsync"],
            toml::Value::String("1.2.3".to_string())
        );
    }
}
