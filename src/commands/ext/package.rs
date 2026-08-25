use anyhow::{Context, Result};
use base64::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use std::fs;
use std::path::PathBuf;

use super::find_ext_in_mapping;
use crate::utils::config::{ComposedConfig, Config, ExtensionLocation};
use crate::utils::container::SdkContainer;
use crate::utils::ext_version_source::VersionSource;
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
    pub runtime: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
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
            runtime: None,
            composed_config: None,
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

    /// Set the runtime context. See [`super::build::ExtBuildCommand::with_runtime`].
    pub fn with_runtime(mut self, runtime: Option<String>) -> Self {
        self.runtime = runtime;
        self
    }

    fn runtime_env_vars(&self) -> Option<HashMap<String, String>> {
        self.runtime.as_ref().map(|rt| {
            let mut m = HashMap::new();
            m.insert("AVOCADO_RUNTIME".to_string(), rt.clone());
            m
        })
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

        // On-disk config filename (avocado.yaml or avocado.yml), so a `.yml`
        // extension stages under its real name rather than a hardcoded
        // `avocado.yaml`.
        let cfg_basename = std::path::Path::new(&ext_config_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "avocado.yaml".to_string());

        if self.verbose {
            match &extension_location {
                ExtensionLocation::Local { name, config_path } => {
                    print_info(
                        &format!("Found local extension '{name}' in config '{config_path}'"),
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
        let ext_config = self
            .resolve_ext_config(config, parsed, &extension_location, &target)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Also get the raw (unmerged) extension config to find all target-specific overlays
        // For remote extensions, use the parsed config; for local, read from file
        let raw_ext_config = match &extension_location {
            ExtensionLocation::Remote { .. } => {
                find_ext_in_mapping(parsed, &self.extension, &target).cloned()
            }
            _ => self.get_raw_extension_config(&ext_config_path)?,
        };

        // Extract RPM metadata with defaults
        let rpm_metadata = self.extract_rpm_metadata(&ext_config, &target, &ext_config_path)?;

        // Determine which files to package
        // Pass both merged config (for package_files), raw config (for all target overlays),
        // and full parsed config (for sdk.compile scripts). The version source (if the
        // extension declared `version: { file, key }`) is read from the composed config
        // rather than the raw text, so it is found the same way for a local extension and
        // for one that was fetched from a remote source.
        let version_source = composed.ext_version_sources.get(&self.extension);
        let package_files = self.get_package_files(
            &ext_config,
            raw_ext_config.as_ref(),
            parsed,
            &cfg_basename,
            version_source,
        );

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

        // Project `depends_on` / `class` into RPM metadata (see below).
        let dependency_metadata =
            self.build_dependency_metadata(&ext_config, &rpm_metadata, &target)?;

        // Create main RPM package in container
        // This packages the extension's src_dir (directory containing avocado.yaml)
        // Resolve package-time-only fields in the avocado.yaml that ships in the
        // package payload. Today that is just `version`, and only for the legacy
        // `version: '{{ env.AVOCADO_EXT_VERSION }}'` form: that template resolves
        // only while the env var is set (package time), so if it survived into the
        // published package a downstream build — which has no such env in scope —
        // would interpolate it to '' and fail semver validation. We bake the
        // resolved value and leave every other template (e.g. `{{ avocado.target }}`)
        // for the consumer to resolve at their build time.
        //
        // An extension using a `version: { file, key }` provider is SKIPPED: the
        // provider reads from the extension's own tree, which ships in the payload,
        // so the published config resolves itself. Baking it would also corrupt the
        // file — the line-scoped rewriter would replace the `version:` line and
        // strand its `file:`/`key:` children.
        let version_bake_section = if version_source.is_some() {
            String::new()
        } else {
            match fs::read_to_string(&ext_config_path) {
                Ok(text) => {
                    let baked = crate::utils::config_edit::bake_extension_version(
                        &text,
                        &self.extension,
                        &target,
                        &rpm_metadata.version,
                    )
                    .with_context(|| {
                        format!(
                            "Failed to bake resolved version '{}' into the packaged \
                             config for extension '{}'",
                            rpm_metadata.version, self.extension
                        )
                    })?;
                    let b64 = BASE64_STANDARD.encode(baked.as_bytes());
                    // The resolved version is baked into the base64 payload only;
                    // it is never interpolated into the shell, so a version string
                    // carrying pre-release/build metadata can't reach the command line.
                    format!(
                        r#"
# Bake the resolved extension version into the packaged config so the published
# avocado.yaml carries a concrete semver instead of an env-only template that
# would interpolate to '' during a downstream runtime build.
if [ -f "$STAGING_DIR/{basename}" ]; then
    printf '%s' '{b64}' | base64 -d > "$STAGING_DIR/{basename}"
    echo "Baked resolved extension version into {basename}"
fi
"#,
                        basename = cfg_basename,
                        b64 = b64,
                    )
                }
                // Remote/unreadable source (e.g. packaging a fetched remote ext
                // whose config lives in the container volume): leave the payload
                // untouched rather than fail the package.
                Err(_) => String::new(),
            }
        };

        let output_path = self
            .create_rpm_package_in_container(
                &rpm_metadata,
                config,
                &target,
                &ext_config_path,
                &package_files,
                &version_bake_section,
                &dependency_metadata,
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
        let ext_section = parsed.get("extensions");
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
    /// - The avocado config file (avocado.yaml or avocado.yml)
    /// - All overlay directories (base level and target-specific)
    /// - Compile scripts from sdk.compile sections
    /// - Install scripts from extension package dependencies
    ///
    /// Whatever the outcome, a `version: { file, key }` provider's file is always
    /// included: the published `avocado.yaml` keeps the provider rather than a
    /// baked literal, so a payload missing that file would be unresolvable for
    /// every consumer. See `version_source`.
    ///
    /// # Arguments
    /// * `ext_config` - The merged extension config (for package_files check)
    /// * `raw_ext_config` - The raw unmerged extension config (to find all target-specific overlays)
    /// * `full_parsed_config` - The full parsed config (to find sdk.compile scripts)
    /// * `version_source` - The version provider this extension resolved through, if any
    fn get_package_files(
        &self,
        ext_config: &serde_yaml::Value,
        raw_ext_config: Option<&serde_yaml::Value>,
        full_parsed_config: &serde_yaml::Value,
        cfg_basename: &str,
        version_source: Option<&VersionSource>,
    ) -> Vec<String> {
        // Check if package_files is explicitly defined
        if let Some(package_files) = ext_config.get("package_files") {
            if let Some(files_array) = package_files.as_sequence() {
                let mut files: Vec<String> = files_array
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if !files.is_empty() {
                    // An explicit list replaces the defaults entirely, so the
                    // version file has to be added back here too — forgetting it
                    // would only surface once someone consumed the package.
                    Self::add_version_source_file(&mut files, version_source);
                    return files;
                }
            }
        }

        // Default behavior: the config file + overlays + compile scripts + install
        // scripts. Use the actual on-disk config name so an `avocado.yml` ext is
        // staged under its real name.
        let mut default_files = vec![cfg_basename.to_string()];
        let mut seen_files = std::collections::HashSet::new();

        // If we have the raw extension config, scan for all overlays
        if let Some(raw_config) = raw_ext_config {
            if let Some(mapping) = raw_config.as_mapping() {
                for (key, value) in mapping {
                    // Check if this is the base-level overlay
                    if key.as_str() == Some("overlay") {
                        if let Some(overlay_dir) = Self::extract_overlay_dir(value) {
                            if seen_files.insert(overlay_dir.clone()) {
                                default_files.push(overlay_dir);
                            }
                        }
                    }
                    // Check if this is a target-specific section with an overlay
                    else if let Some(target_config) = value.as_mapping() {
                        if let Some(overlay_value) = target_config.get("overlay") {
                            if let Some(overlay_dir) = Self::extract_overlay_dir(overlay_value) {
                                if seen_files.insert(overlay_dir.clone()) {
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
                    if seen_files.insert(overlay_dir.clone()) {
                        default_files.push(overlay_dir);
                    }
                }
            }
        }

        // Collect compile scripts from sdk.compile sections
        if let Some(sdk_compile) = full_parsed_config
            .get("sdk")
            .and_then(|s| s.get("compile"))
            .and_then(|c| c.as_mapping())
        {
            for (_section_name, section_config) in sdk_compile {
                if let Some(compile_script) = section_config.get("compile").and_then(|c| c.as_str())
                {
                    if seen_files.insert(compile_script.to_string()) {
                        default_files.push(compile_script.to_string());
                    }
                }
            }
        }

        // Collect install scripts from extension package dependencies
        // Format: extensions.<ext>.packages.<dep>.install = "script.sh"
        if let Some(packages) = ext_config.get("packages").and_then(|p| p.as_mapping()) {
            for (_dep_name, dep_spec) in packages {
                if let Some(install_script) = dep_spec.get("install").and_then(|i| i.as_str()) {
                    if seen_files.insert(install_script.to_string()) {
                        default_files.push(install_script.to_string());
                    }
                }
            }
        }

        Self::add_version_source_file(&mut default_files, version_source);

        default_files
    }

    /// Resolve the extension's effective config, applying its per-target
    /// overrides. Everything downstream — notably [`Self::extract_rpm_metadata`],
    /// which reads `version` / `release` / `summary` / `description` / `license`
    /// straight off the result — depends on this being resolved, or a
    /// `target-<name>:` override is silently dropped from the built RPM.
    ///
    /// Note this is deliberately separate from `raw_ext_config` at the call site:
    /// that one stays unresolved on purpose, because `get_package_files` needs to
    /// see *every* target's overlay, not just the active one's.
    fn resolve_ext_config(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        extension_location: &ExtensionLocation,
        target: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        match extension_location {
            ExtensionLocation::Remote { .. } => {
                // Composed value from `parsed` (remote/path-sourced ext configs
                // already merged in); resolve its `target-<name>:` overrides for
                // the same result the Local path gets via get_merged_ext_config.
                Ok(super::resolve_remote_ext_config(
                    config,
                    parsed,
                    &self.extension,
                    target,
                ))
            }
            ExtensionLocation::Local { config_path, .. } => {
                // For local extensions, read from the file with proper target merging
                config.get_merged_ext_config(&self.extension, target, config_path)
            }
        }
    }

    /// Append a `version: { file, key }` provider's file to a package file list.
    ///
    /// Idempotent — an author who already listed the file doesn't get it twice.
    fn add_version_source_file(files: &mut Vec<String>, version_source: Option<&VersionSource>) {
        let Some(source) = version_source else { return };
        if !files.iter().any(|f| f == &source.file) {
            files.push(source.file.clone());
        }
    }

    /// Extract RPM metadata from extension configuration with defaults
    ///
    /// `source_path` is only used to say where an invalid `version` came from.
    fn extract_rpm_metadata(
        &self,
        ext_config: &serde_yaml::Value,
        _target: &str, // Not used - extensions default to noarch
        source_path: &str,
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
        crate::utils::version::validate_ext_version(&self.extension, &version, source_path)?;

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
    /// Project the extension's `depends_on` and `class` into RPM metadata.
    ///
    /// Mirrors the existing `supported_targets` → `avocado-target(...)`
    /// projection: a field of `avocado.yaml` becomes repo metadata, readable
    /// from `primary.xml` without downloading the package. That is what lets
    /// the dependency graph be inspected before anything is materialized.
    ///
    /// The `Requires:` lines do real work — because the nested layout installs
    /// every extension into one shared includes installroot, a single
    /// `dnf install` depsolves and materializes the whole dependency closure
    /// in one transaction rather than N discovery rounds.
    ///
    /// Dependencies are named by the virtual capability `avocado-ext(<name>)`,
    /// never by the bare RPM name: `source.package` lets a consumer publish an
    /// extension under a different package name, which would break a literal
    /// `Requires: <ext-name>`.
    fn build_dependency_metadata(
        &self,
        ext_config: &serde_yaml::Value,
        metadata: &RpmMetadata,
        target: &str,
    ) -> Result<String> {
        use crate::utils::ext_deps::{rpm_capability, ExtensionClass, ExtensionDependency};

        // RPM form, not semver: the spec's Version: is converted with
        // to_rpm_version (1.0.0-rc.1 -> 1.0.0~rc.1) and the Requires bounds
        // to_rpm_requires emits are in RPM form too. Advertising the provide
        // in semver form made an exact pre-release Requires unsatisfiable.
        let provide_version = crate::utils::version::to_rpm_version(&metadata.version)
            .unwrap_or_else(|_| metadata.version.clone());
        let mut lines = vec![format!(
            "Provides: {} = {}",
            rpm_capability(&self.extension),
            provide_version
        )];

        let class = ExtensionClass::from_ext_config(&self.extension, ext_config)?;
        lines.push(format!("Provides: avocado-ext-class({})", class.as_str()));

        if let Some(seq) = ext_config.get("depends_on").and_then(|v| v.as_sequence()) {
            for entry in seq {
                let dep = ExtensionDependency::parse_entry(&self.extension, entry, target)?;
                lines.extend(dep.to_rpm_requires()?);
            }
        }

        Ok(lines.join("\n"))
    }

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

    /// Generate the `rpmbuild` shell script for this extension.
    ///
    /// Split out of [`Self::create_rpm_package_in_container`] so the two places
    /// the RPM-form version lands — the spec's `Version:` line and the NVR
    /// filename — are assertable without running a container. Mirrors
    /// `sdk::package`'s `generate_rpm_build_script`.
    #[allow(clippy::too_many_arguments)]
    fn generate_rpm_build_script(
        &self,
        metadata: &RpmMetadata,
        rpm_version: &str,
        rpm_filename: &str,
        target_provides: &str,
        container_src_dir: &str,
        package_files_str: &str,
        version_bake_section: &str,
        dependency_metadata: &str,
    ) -> String {
        format!(
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
if [ ! -f "$EXT_SRC_DIR/avocado.yaml" ] && [ ! -f "$EXT_SRC_DIR/avocado.yml" ]; then
echo "No avocado.yaml/yml found in $EXT_SRC_DIR"
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
        cp -r "$file" "$STAGING_DIR/$file"
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
{version_bake_section}
echo "Creating RPM with $FILE_COUNT files from source directory..."

if [ "$FILE_COUNT" -eq 0 ]; then
echo "No files matched the package_files patterns: $PACKAGE_FILES"
exit 1
fi

# Create spec file
# The extension's src_dir maps to a top-level /<ext_name>/ directory in the package, so
# that installing into a SHARED includes installroot lands its content at
# includes/<ext_name>/ without colliding with other extensions' files (and one rpmdb
# tracks all installed extensions).
cat > SPECS/package.spec << SPEC_EOF
%define _buildhost reproducible
# The payload is already-built target content, so rpm has to package it verbatim.
# Two stock behaviours get in the way: the build-root policy scripts run the
# container's host toolchain (strip and friends) over target binaries, and the
# noarch guard rejects a payload carrying target ELF. These packages are noarch
# by design - a target routes on the avocado-target provides below, not on the
# package arch - so both checks are false positives here.
# Comments must stay macro-free; rpm expands macros inside spec comments.
%define __os_install_post %{{nil}}
%define _binaries_in_noarch_packages_terminate_build 0
AutoReqProv: no

Name: {name}
Version: {version}
Release: {release}
Summary: {summary}
License: {license}
Vendor: {vendor}
Group: {group}{url_line}
# Self-describe the on-disk layout so the CLI knows how to install this package: content
# is nested under /<ext_name>/, so it installs into the SHARED includes installroot.
# Legacy packages (content at /) lack this provide and use the per-ext installroot.
Provides: avocado-ext-layout(nested)
# Self-describe which targets this extension supports, derived from avocado.yaml
# supported_targets. Surfaced in the feed's repo metadata (primary.xml) so the CLI can
# query target compatibility without downloading the RPM, and so the feed server can route
# it to the correct per-target -ext feed(s). "*" means all targets (cross-target).
{target_provides}
# Self-describe the inter-extension dependency edges, derived from avocado.yaml
# `depends_on` / `class`. Dependents require the virtual `avocado-ext(<name>)`
# capability rather than the RPM name, so a `source.package` rename can't break the
# edge. Because nested packages share one includes installroot, these Requires let a
# single `dnf install` materialize the whole closure in one transaction.
{dependency_metadata}

%description
{description}

%files
/{name}

%prep
# No prep needed

%build
# No build needed

%install
# Nest the staged files under /<ext_name>/ so a shared includes installroot yields
# includes/<ext_name>/... (collision-free, one rpmdb per includes root).
mkdir -p %{{buildroot}}/{name}
cp -r "$STAGING_DIR"/* %{{buildroot}}/{name}/

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
            version = rpm_version,
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
            target_provides = target_provides,
            dependency_metadata = dependency_metadata,
            rpm_filename = rpm_filename,
            container_src_dir = container_src_dir,
            package_files_str = package_files_str,
            version_bake_section = version_bake_section,
        )
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
    #[allow(clippy::too_many_arguments)]
    async fn create_rpm_package_in_container(
        &self,
        metadata: &RpmMetadata,
        config: &Config,
        target: &str,
        ext_config_path: &str,
        package_files: &[String],
        version_bake_section: &str,
        dependency_metadata: &str,
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

        // RPM forbids `-` in Version (it is the Version/Release separator), so map
        // the semver version to its RPM form (`1.0.0-rc.1` -> `1.0.0~rc.1`). Used
        // for the spec `Version:` and the built RPM's name; the semver form is kept
        // for the avocado.yaml baked into the payload (consumers validate semver).
        let rpm_version = crate::utils::version::to_rpm_version(&metadata.version)
            .with_context(|| format!("Extension '{}' cannot be packaged", self.extension))?;

        // Create the RPM filename (matches the built RPM's NVR, so it uses the
        // RPM-form version, not the semver form).
        let rpm_filename = format!(
            "{}-{}-{}.{}.rpm",
            metadata.name, rpm_version, metadata.release, metadata.arch
        );

        // Convert package_files to a space-separated string for the shell script
        let package_files_str = package_files.join(" ");

        // Stamp the supported targets into the RPM as `avocado-target(<machine>)` provides,
        // derived from avocado.yaml `supported_targets` (never a hardcoded list). The feed
        // server reads these to route the package to the right per-target `-ext` feed(s).
        // `get_supported_targets()` returns the explicit list, or None for `*`/unset — both
        // of which mean "all targets", which we record as the single wildcard `avocado-target(*)`.
        let target_provides = match config.get_supported_targets() {
            Some(targets) if !targets.is_empty() => targets
                .iter()
                .map(|t| format!("Provides: avocado-target({t})"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "Provides: avocado-target(*)".to_string(),
        };

        // Create RPM using rpmbuild in container
        // Package root (/) maps to the extension's src_dir contents
        let rpm_build_script = self.generate_rpm_build_script(
            metadata,
            &rpm_version,
            &rpm_filename,
            &target_provides,
            &container_src_dir,
            &package_files_str,
            version_bake_section,
            dependency_metadata,
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
            env_vars: self.runtime_env_vars(),
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
                &container_helper.container_tool,
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

    /// Copy the RPM from the container to the host using `<container_tool> cp`.
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
                &format!("Copying RPM to host: {output_dir}"),
                OutputLevel::Normal,
            );
        }

        // Create a temporary container to access the volume (following checkout pattern)
        let temp_container_id = self
            .create_temp_container(container_tool, volume_name)
            .await?;

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
        let copy_output = tokio::process::Command::new(container_tool)
            .arg("cp")
            .arg(&docker_cp_source)
            .arg(&docker_cp_dest)
            .output()
            .await
            .context("Failed to execute docker cp")?;

        // Clean up temporary container
        let _ = tokio::process::Command::new(container_tool)
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

    fn test_cmd() -> ExtPackageCommand {
        ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            false,
            None,
            None,
        )
    }

    fn metadata_with_version(version: &str) -> RpmMetadata {
        RpmMetadata {
            name: "my-ext".to_string(),
            version: version.to_string(),
            release: "r0".to_string(),
            summary: "s".to_string(),
            description: "d".to_string(),
            license: "MIT".to_string(),
            arch: "noarch".to_string(),
            vendor: "v".to_string(),
            group: "g".to_string(),
            url: None,
        }
    }

    /// The spec `Version:` line and the NVR filename must both carry the RPM
    /// form. `rpmbuild` aborts with `Illegal char '-'` on the semver form, so a
    /// refactor that interpolated `metadata.version` into either would break
    /// packaging for every pre-release — and, before this test, do it with a
    /// fully green suite.
    #[test]
    fn test_generate_rpm_build_script_uses_rpm_form_version() {
        let cmd = test_cmd();
        let metadata = metadata_with_version("1.0.0-rc.1");
        let rpm_version = crate::utils::version::to_rpm_version(&metadata.version).unwrap();
        let rpm_filename = format!(
            "{}-{}-{}.{}.rpm",
            metadata.name, rpm_version, metadata.release, metadata.arch
        );

        let script = cmd.generate_rpm_build_script(
            &metadata,
            &rpm_version,
            &rpm_filename,
            "Provides: avocado-target(*)",
            "/opt/src",
            "avocado.yaml",
            "",
            "",
        );

        // The spec field rpmbuild parses.
        assert!(
            script.contains("Version: 1.0.0~rc.1"),
            "spec Version: must be the RPM form"
        );
        assert!(
            !script.contains("Version: 1.0.0-rc.1"),
            "spec Version: must not carry the semver hyphen"
        );
        // The built artifact's name.
        assert!(
            script.contains("my-ext-1.0.0~rc.1-r0.noarch.rpm"),
            "RPM filename must be the RPM form"
        );
        assert!(
            !script.contains("my-ext-1.0.0-rc.1-r0.noarch.rpm"),
            "RPM filename must not carry the semver hyphen"
        );
    }

    /// A plain release version is passed through untouched, so the common path
    /// is unaffected by the mapping.
    #[test]
    fn test_generate_rpm_build_script_release_version_unchanged() {
        let cmd = test_cmd();
        let metadata = metadata_with_version("1.2.3");
        let rpm_version = crate::utils::version::to_rpm_version(&metadata.version).unwrap();

        let script = cmd.generate_rpm_build_script(
            &metadata,
            &rpm_version,
            "my-ext-1.2.3-r0.noarch.rpm",
            "Provides: avocado-target(*)",
            "/opt/src",
            "avocado.yaml",
            "",
            "",
        );

        assert!(script.contains("Version: 1.2.3"));
        assert!(script.contains("my-ext-1.2.3-r0.noarch.rpm"));
        assert!(!script.contains('~'), "no `~` for a release version");
    }

    /// A `target-<name>:` override on a remote/path-sourced extension must reach
    /// [`ExtPackageCommand::extract_rpm_metadata`], so the RPM is built at the
    /// per-target version rather than the base one.
    ///
    /// Regression: this arm used to look only for a legacy bare `<target>:` key
    /// and return the composed value untouched when absent, so the modern
    /// `target-<name>:` form was silently dropped from the packaged RPM (and the
    /// key itself leaked through as literal config content).
    #[test]
    fn test_target_prefix_override_reaches_rpm_metadata() {
        let cmd = ExtPackageCommand::new(
            "avocado.yaml".to_string(),
            "kos-layer-boardconf".to_string(),
            Some("qemux86-64".to_string()),
            None,
            false,
            None,
            None,
        );

        let config =
            Config::load_from_yaml_str("supported_targets: [qemux86-64, raspberrypi4]\n").unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(
            r#"
extensions:
  kos-layer-boardconf:
    version: 2026.7.0
    summary: base summary
    target-qemux86-64:
      version: 2026.7.1
      summary: qemu summary
    target-raspberrypi4:
      version: 2026.7.2
"#,
        )
        .unwrap();
        let location = ExtensionLocation::Remote {
            name: "kos-layer-boardconf".to_string(),
            source: crate::utils::config::ExtensionSource::Path {
                path: "extension-kabs/kos-layer-boardconf".to_string(),
                include: None,
            },
        };

        let resolved = cmd
            .resolve_ext_config(&config, &parsed, &location, "qemux86-64")
            .unwrap()
            .expect("extension present in composed config");
        let meta = cmd
            .extract_rpm_metadata(&resolved, "qemux86-64", "avocado.yaml")
            .unwrap();

        assert_eq!(meta.version, "2026.7.1", "per-target version must win");
        assert_eq!(meta.summary, "qemu summary");
        // The sibling target's block must not leak in, nor the override keys.
        assert!(resolved.get("target-qemux86-64").is_none());
        assert!(resolved.get("target-raspberrypi4").is_none());
    }

    fn dep_metadata(ext_name: &str, ext_yaml: &str) -> String {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            ext_name.to_string(),
            Some("qemux86-64".to_string()),
            None,
            false,
            None,
            None,
        );
        let ext_config: serde_yaml::Value = serde_yaml::from_str(ext_yaml).unwrap();
        let metadata = RpmMetadata {
            name: ext_name.to_string(),
            version: "0.4.0".to_string(),
            release: "r0".to_string(),
            summary: String::new(),
            description: String::new(),
            license: String::new(),
            arch: "x86_64".to_string(),
            vendor: String::new(),
            group: String::new(),
            url: None,
        };
        cmd.build_dependency_metadata(&ext_config, &metadata, "qemux86-64")
            .unwrap()
    }

    /// The versioned provide must be in RPM form, matching the spec's
    /// `Version:` and the RPM-form bounds `to_rpm_requires` emits. In semver
    /// form (`1.0.0-rc.1`) an exact pre-release Requires could never match.
    #[test]
    fn test_dependency_metadata_provide_uses_rpm_version_form() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "app-a".to_string(),
            Some("qemux86-64".to_string()),
            None,
            false,
            None,
            None,
        );
        let ext_config: serde_yaml::Value = serde_yaml::from_str("types: [sysext]\n").unwrap();
        let metadata = RpmMetadata {
            name: "app-a".to_string(),
            version: "1.0.0-rc.1".to_string(),
            release: "r0".to_string(),
            summary: String::new(),
            description: String::new(),
            license: String::new(),
            arch: "x86_64".to_string(),
            vendor: String::new(),
            group: String::new(),
            url: None,
        };
        let spec = cmd
            .build_dependency_metadata(&ext_config, &metadata, "qemux86-64")
            .unwrap();
        assert!(
            spec.contains("Provides: avocado-ext(app-a) = 1.0.0~rc.1"),
            "{spec}"
        );
        assert!(!spec.contains("= 1.0.0-rc.1"), "{spec}");
    }

    #[test]
    fn test_dependency_metadata_provides_capability_and_class() {
        let spec = dep_metadata("app-a", "types: [sysext]\n");
        assert!(
            spec.contains("Provides: avocado-ext(app-a) = 0.4.0"),
            "{spec}"
        );
        // class defaults to application when unset
        assert!(
            spec.contains("Provides: avocado-ext-class(application)"),
            "{spec}"
        );
        assert!(!spec.contains("Requires:"), "{spec}");
    }

    #[test]
    fn test_dependency_metadata_marks_platform_class() {
        let spec = dep_metadata("weston-base", "class: platform\n");
        assert!(
            spec.contains("Provides: avocado-ext-class(platform)"),
            "{spec}"
        );
    }

    #[test]
    fn test_dependency_metadata_emits_requires_on_virtual_capability() {
        // Requires must name avocado-ext(...), never the bare RPM name, so a
        // `source.package` rename can't break the edge.
        let spec = dep_metadata("app-a", "depends_on: [weston-base]\n");
        assert!(
            spec.contains("Requires: avocado-ext(weston-base)"),
            "{spec}"
        );
        assert!(!spec.contains("Requires: weston-base"), "{spec}");
    }

    #[test]
    fn test_dependency_metadata_expands_a_version_range() {
        let spec = dep_metadata(
            "app-a",
            "depends_on:\n  - { name: weston-base, version: \"^1.2.0\" }\n",
        );
        assert!(
            spec.contains("Requires: avocado-ext(weston-base) >= 1.2.0"),
            "{spec}"
        );
        assert!(
            spec.contains("Requires: avocado-ext(weston-base) < 2.0.0"),
            "{spec}"
        );
    }

    #[test]
    fn test_dependency_metadata_interpolates_templated_dep_names() {
        let spec = dep_metadata(
            "app-a",
            "depends_on: [\"avocado-bsp-{{ avocado.target }}\"]\n",
        );
        assert!(
            spec.contains("Requires: avocado-ext(avocado-bsp-qemux86-64)"),
            "{spec}"
        );
    }

    #[test]
    fn test_dependency_metadata_rejects_a_bad_version_requirement() {
        let cmd = ExtPackageCommand::new(
            "test.yaml".to_string(),
            "app-a".to_string(),
            Some("qemux86-64".to_string()),
            None,
            false,
            None,
            None,
        );
        let ext_config: serde_yaml::Value =
            serde_yaml::from_str("depends_on:\n  - { name: weston-base, version: \"nonsense\" }\n")
                .unwrap();
        let metadata = RpmMetadata {
            name: "app-a".to_string(),
            version: "0.4.0".to_string(),
            release: "r0".to_string(),
            summary: String::new(),
            description: String::new(),
            license: String::new(),
            arch: "x86_64".to_string(),
            vendor: String::new(),
            group: String::new(),
            url: None,
        };
        assert!(cmd
            .build_dependency_metadata(&ext_config, &metadata, "qemux86-64")
            .is_err());
    }

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
            .extract_rpm_metadata(&ext_config, "x86_64-unknown-linux-gnu", "avocado.yaml")
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
            .extract_rpm_metadata(&ext_config, "aarch64-unknown-linux-gnu", "avocado.yaml")
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

        let result =
            cmd.extract_rpm_metadata(&ext_config, "x86_64-unknown-linux-gnu", "avocado.yaml");

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
            let metadata = cmd
                .extract_rpm_metadata(&ext_config, target, "avocado.yaml")
                .unwrap();
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

        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files =
            cmd.get_package_files(&ext_config, None, &empty_full_config, "avocado.yaml", None);
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
        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files = cmd.get_package_files(
            &ext_config,
            Some(&ext_config),
            &empty_full_config,
            "avocado.yaml",
            None,
        );
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
        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files = cmd.get_package_files(
            &ext_config,
            Some(&ext_config),
            &empty_full_config,
            "avocado.yaml",
            None,
        );
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

        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files = cmd.get_package_files(
            &ext_config,
            Some(&ext_config),
            &empty_full_config,
            "avocado.yaml",
            None,
        );
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

    /// Helpers for the version-source payload tests below.
    fn ext_with_version(version: &str) -> serde_yaml::Value {
        let mut ext = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        ext.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String(version.to_string()),
        );
        ext
    }

    fn cargo_version_source() -> VersionSource {
        VersionSource {
            file: "Cargo.toml".to_string(),
            key: Some("package.version".to_string()),
            format: None,
        }
    }

    fn package_cmd() -> ExtPackageCommand {
        ExtPackageCommand::new(
            "test.yaml".to_string(),
            "test-ext".to_string(),
            None,
            None,
            false,
            None,
            None,
        )
    }

    /// The published avocado.yaml keeps the `version: { file, key }` provider
    /// rather than a baked literal, so the file it names has to ship or the
    /// package is unresolvable for every consumer.
    #[test]
    fn test_get_package_files_default_includes_version_source_file() {
        let cmd = package_cmd();
        let ext_config = ext_with_version("1.0.0");
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let files = cmd.get_package_files(
            &ext_config,
            None,
            &empty_full_config,
            "avocado.yaml",
            Some(&cargo_version_source()),
        );

        assert_eq!(
            files,
            vec!["avocado.yaml".to_string(), "Cargo.toml".to_string()]
        );
    }

    /// An explicit `package_files` replaces the defaults wholesale, so the
    /// version file has to be added back on that branch too. Forgetting it
    /// would only surface when someone consumed the published package.
    #[test]
    fn test_get_package_files_explicit_list_includes_version_source_file() {
        let cmd = package_cmd();
        let mut ext_config = ext_with_version("1.0.0");
        ext_config.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("package_files".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("avocado.yaml".to_string()),
                serde_yaml::Value::String("src".to_string()),
            ]),
        );
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let files = cmd.get_package_files(
            &ext_config,
            Some(&ext_config),
            &empty_full_config,
            "avocado.yaml",
            Some(&cargo_version_source()),
        );

        assert_eq!(
            files,
            vec![
                "avocado.yaml".to_string(),
                "src".to_string(),
                "Cargo.toml".to_string(),
            ]
        );
    }

    #[test]
    fn test_get_package_files_does_not_duplicate_listed_version_source_file() {
        let cmd = package_cmd();
        let mut ext_config = ext_with_version("1.0.0");
        ext_config.as_mapping_mut().unwrap().insert(
            serde_yaml::Value::String("package_files".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("avocado.yaml".to_string()),
                serde_yaml::Value::String("Cargo.toml".to_string()),
                serde_yaml::Value::String("src".to_string()),
            ]),
        );
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let files = cmd.get_package_files(
            &ext_config,
            Some(&ext_config),
            &empty_full_config,
            "avocado.yaml",
            Some(&cargo_version_source()),
        );

        assert_eq!(
            files.iter().filter(|f| *f == "Cargo.toml").count(),
            1,
            "version source file listed twice: {files:?}"
        );
    }

    /// A literal `version:` has no provider, so nothing extra is added.
    #[test]
    fn test_get_package_files_without_version_source_is_unchanged() {
        let cmd = package_cmd();
        let ext_config = ext_with_version("1.0.0");
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

        let files =
            cmd.get_package_files(&ext_config, None, &empty_full_config, "avocado.yaml", None);

        assert_eq!(files, vec!["avocado.yaml".to_string()]);
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

        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files =
            cmd.get_package_files(&ext_config, None, &empty_full_config, "avocado.yaml", None);
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

        // Pass empty full config since we're not testing compile script extraction
        let empty_full_config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let files = cmd.get_package_files(
            &merged_config,
            Some(&raw_config),
            &empty_full_config,
            "avocado.yaml",
            None,
        );

        // Should include avocado.yaml and both target-specific overlays
        assert!(files.contains(&"avocado.yaml".to_string()));
        assert!(files.contains(&"overlays/reterminal".to_string()));
        assert!(files.contains(&"overlays/reterminal-dm".to_string()));
        assert_eq!(files.len(), 3);
    }
}
