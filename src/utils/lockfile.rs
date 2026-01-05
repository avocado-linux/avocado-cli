//! Lock file utilities for reproducible DNF package installations.
//!
//! This module provides functionality to track and pin package versions
//! across different sysroots to ensure reproducible builds.

// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Current lock file format version
/// Version 2: SDK packages are now keyed by host architecture (sdk/{arch}) instead of just "sdk"
const LOCKFILE_VERSION: u32 = 2;

/// Lock file name
const LOCKFILE_NAME: &str = "lock.json";

/// Lock file directory within src_dir
const LOCKFILE_DIR: &str = ".avocado";

/// Represents different sysroot types for package installation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SysrootType {
    /// SDK sysroot ($AVOCADO_SDK_PREFIX) - keyed by host architecture
    /// SDK packages are nativesdk packages that run on the host, so they need
    /// to be tracked per host architecture (e.g., x86_64, aarch64).
    /// The String parameter is the host architecture.
    Sdk(String),
    /// Rootfs sysroot ($AVOCADO_PREFIX/rootfs)
    Rootfs,
    /// Target sysroot ($AVOCADO_PREFIX/sdk/target-sysroot)
    TargetSysroot,
    /// Local/external extension sysroot ($AVOCADO_EXT_SYSROOTS/{name})
    /// Uses ext-rpm-config-scripts for RPM database
    Extension(String),
    /// DEPRECATED: Versioned extension sysroot
    /// The vsn: syntax is no longer supported. Remote extensions are now defined
    /// in the ext section with source: field and are treated as local extensions
    /// after being fetched to $AVOCADO_PREFIX/includes/<ext_name>/.
    #[deprecated(since = "0.23.0", note = "Use Extension variant for all extensions")]
    #[allow(dead_code)]
    VersionedExtension(String),
    /// Runtime sysroot ($AVOCADO_PREFIX/runtimes/{name})
    Runtime(String),
}

impl SysrootType {
    /// Get the RPM query command environment and root path for this sysroot type
    /// Returns (rpm_etcconfigdir, rpm_configdir, root_path) as shell variable expressions
    ///
    /// For SDK packages, root_path is None because SDK packages are installed into
    /// the native container root but tracked via custom RPM_CONFIGDIR macros that
    /// point to $AVOCADO_SDK_PREFIX/var/lib/rpm.
    pub fn get_rpm_query_config(&self) -> RpmQueryConfig {
        match self {
            // SDK config is the same regardless of which host arch we're tracking
            SysrootType::Sdk(_arch) => RpmQueryConfig {
                // SDK needs custom RPM config to find the SDK's RPM database
                rpm_etcconfigdir: Some("$AVOCADO_SDK_PREFIX".to_string()),
                rpm_configdir: Some("$AVOCADO_SDK_PREFIX/usr/lib/rpm".to_string()),
                // No --root flag - SDK packages use native root with custom RPM_CONFIGDIR
                root_path: None,
            },
            SysrootType::Rootfs => RpmQueryConfig {
                // For installroots, we don't need RPM_ETCCONFIGDIR - the --root flag is sufficient
                // Setting it can interfere with the query by pointing to the wrong rpmrc
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some("$AVOCADO_PREFIX/rootfs".to_string()),
            },
            SysrootType::TargetSysroot => RpmQueryConfig {
                // Target-sysroot: same approach as rootfs - unset config and use --root
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some("$AVOCADO_PREFIX/sdk/target-sysroot".to_string()),
            },
            SysrootType::Extension(name) => RpmQueryConfig {
                // Local/external extensions use ext-rpm-config-scripts
                // The database is at standard location, so --root is sufficient
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_EXT_SYSROOTS/{name}")),
            },
            SysrootType::VersionedExtension(name) => RpmQueryConfig {
                // Versioned extensions use ext-rpm-config which puts database at custom location
                // We need to set RPM_CONFIGDIR to find the database correctly
                rpm_etcconfigdir: None,
                rpm_configdir: Some("$AVOCADO_SDK_PREFIX/ext-rpm-config".to_string()),
                root_path: Some(format!("$AVOCADO_EXT_SYSROOTS/{name}")),
            },
            SysrootType::Runtime(name) => RpmQueryConfig {
                // Runtime: same approach as rootfs - unset config and use --root
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_PREFIX/runtimes/{name}")),
            },
        }
    }
}

/// Configuration for RPM query command
#[derive(Debug, Clone)]
pub struct RpmQueryConfig {
    /// RPM_ETCCONFIGDIR environment variable value (optional - only needed for SDK)
    pub rpm_etcconfigdir: Option<String>,
    /// RPM_CONFIGDIR environment variable value (optional)
    pub rpm_configdir: Option<String>,
    /// Root path for --root flag (None means query native/default database)
    pub root_path: Option<String>,
}

impl RpmQueryConfig {
    /// Build the rpm -q command with proper environment and flags
    pub fn build_query_command(&self, packages: &[String]) -> String {
        // Build rpm command with query format
        // Output format: NAME VERSION-RELEASE.ARCH
        // Note: We append "|| true" because the entrypoint uses "set -e" and rpm -q
        // returns non-zero if ANY package is not found, which would cause the script
        // to exit. We want to get partial results even if some packages aren't found.

        if let Some(ref root_path) = self.root_path {
            // For installroot queries, we use a subshell to control env vars precisely
            // The container entrypoint sets RPM_ETCCONFIGDIR and RPM_CONFIGDIR to SDK values
            // which can interfere with --root queries, so we need to override them.

            let mut env_setup = String::new();

            // Build the environment variable setup
            // We unset both first, then set only what we need
            env_setup.push_str("unset RPM_ETCCONFIGDIR RPM_CONFIGDIR; ");

            if let Some(ref etcconfigdir) = self.rpm_etcconfigdir {
                env_setup.push_str(&format!("export RPM_ETCCONFIGDIR=\"{etcconfigdir}\"; "));
            }
            if let Some(ref configdir) = self.rpm_configdir {
                env_setup.push_str(&format!("export RPM_CONFIGDIR=\"{configdir}\"; "));
            }

            format!(
                "({}rpm -q --root=\"{}\" --qf '%{{NAME}} %{{VERSION}}-%{{RELEASE}}.%{{ARCH}}\\n' {}) || true",
                env_setup,
                root_path,
                packages.join(" ")
            )
        } else {
            // For SDK native queries (no --root), set the custom RPM config paths
            let mut cmd = String::new();
            if let Some(ref etcconfigdir) = self.rpm_etcconfigdir {
                cmd.push_str(&format!("RPM_ETCCONFIGDIR=\"{etcconfigdir}\" "));
            }
            if let Some(ref configdir) = self.rpm_configdir {
                cmd.push_str(&format!("RPM_CONFIGDIR=\"{configdir}\" "));
            }
            cmd.push_str(&format!(
                "rpm -q --qf '%{{NAME}} %{{VERSION}}-%{{RELEASE}}.%{{ARCH}}\\n' {} || true",
                packages.join(" ")
            ));
            cmd
        }
    }
}

/// Package versions map: package_name -> version
pub type PackageVersions = HashMap<String, String>;

/// Nested package versions map: sub_key -> package_name -> version
/// Used for SDK (keyed by host arch), extensions (keyed by name), runtimes (keyed by name)
pub type NestedPackageVersions = HashMap<String, PackageVersions>;

/// Lock data for a single target
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetLocks {
    /// SDK packages keyed by host architecture (x86_64, aarch64, etc.)
    /// SDK packages are nativesdk packages that run on the host, so versions
    /// can differ per host architecture.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sdk: NestedPackageVersions,

    /// Rootfs packages (shared across all host architectures)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub rootfs: PackageVersions,

    /// Target-sysroot packages (shared across all host architectures)
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        rename = "target-sysroot"
    )]
    pub target_sysroot: PackageVersions,

    /// Extension packages keyed by extension name
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extensions: NestedPackageVersions,

    /// Runtime packages keyed by runtime name
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub runtimes: NestedPackageVersions,
}

/// Lock file structure for tracking installed package versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Lock file format version
    pub version: u32,
    /// Package versions organized by target
    pub targets: HashMap<String, TargetLocks>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self::new()
    }
}

impl LockFile {
    /// Create a new empty lock file
    pub fn new() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            targets: HashMap::new(),
        }
    }

    /// Get the lock file path for a given src_dir
    pub fn get_path(src_dir: &Path) -> PathBuf {
        src_dir.join(LOCKFILE_DIR).join(LOCKFILE_NAME)
    }

    /// Load lock file from disk, or return a new one if it doesn't exist
    ///
    /// This function also handles migration from older lock file versions:
    /// - Version 1 -> 2: SDK packages now nested under arch key, extensions/runtimes
    ///   restructured. Old format is migrated automatically.
    pub fn load(src_dir: &Path) -> Result<Self> {
        let path = Self::get_path(src_dir);

        if !path.exists() {
            return Ok(Self::new());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read lock file: {}", path.display()))?;

        // First, try to parse as v2 format
        if let Ok(lock_file) = serde_json::from_str::<LockFile>(&content) {
            if lock_file.version >= LOCKFILE_VERSION {
                if lock_file.version > LOCKFILE_VERSION {
                    anyhow::bail!(
                        "Lock file version {} is newer than supported version {}. Please upgrade avocado-cli.",
                        lock_file.version,
                        LOCKFILE_VERSION
                    );
                }
                return Ok(lock_file);
            }
        }

        // Try to parse as v1 format and migrate
        let v1_lock: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse lock file: {}", path.display()))?;

        let version = v1_lock.get("version").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        if version == 1 {
            return Ok(Self::migrate_v1_to_v2(&v1_lock));
        }

        // Unknown format
        anyhow::bail!("Unable to parse lock file format");
    }

    /// Migrate lock file from version 1 to version 2
    ///
    /// Version 1 stored:
    /// - SDK packages under flat "sdk" key
    /// - Extensions under "extensions/{name}"
    /// - Runtimes under "runtimes/{name}"
    ///
    /// Version 2 stores:
    /// - SDK packages under nested sdk -> {arch} -> packages
    /// - Extensions under nested extensions -> {name} -> packages
    /// - Runtimes under nested runtimes -> {name} -> packages
    ///
    /// Since we can't know what architecture the v1 SDK packages were installed for,
    /// we discard them. Users will need to re-run `avocado sdk install`.
    fn migrate_v1_to_v2(v1_lock: &serde_json::Value) -> LockFile {
        let mut lock_file = LockFile::new();

        if let Some(targets) = v1_lock.get("targets").and_then(|t| t.as_object()) {
            for (target_name, sysroots) in targets {
                let target_locks = lock_file.targets.entry(target_name.clone()).or_default();

                if let Some(sysroots_map) = sysroots.as_object() {
                    for (key, packages) in sysroots_map {
                        if let Some(packages_map) = packages.as_object() {
                            let pkg_versions: PackageVersions = packages_map
                                .iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect();

                            match key.as_str() {
                                "sdk" => {
                                    // Discard - we don't know the host arch
                                }
                                "rootfs" => {
                                    target_locks.rootfs = pkg_versions;
                                }
                                "target-sysroot" => {
                                    target_locks.target_sysroot = pkg_versions;
                                }
                                _ if key.starts_with("extensions/") => {
                                    if let Some(name) = key.strip_prefix("extensions/") {
                                        target_locks
                                            .extensions
                                            .insert(name.to_string(), pkg_versions);
                                    }
                                }
                                _ if key.starts_with("runtimes/") => {
                                    if let Some(name) = key.strip_prefix("runtimes/") {
                                        target_locks
                                            .runtimes
                                            .insert(name.to_string(), pkg_versions);
                                    }
                                }
                                _ => {
                                    // Unknown key, ignore
                                }
                            }
                        }
                    }
                }
            }
        }

        lock_file
    }

    /// Save lock file to disk using JSON Canonicalization Scheme (RFC 8785)
    /// This ensures deterministic output with sorted keys and consistent formatting
    pub fn save(&self, src_dir: &Path) -> Result<()> {
        let path = Self::get_path(src_dir);

        // Ensure the .avocado directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create lock file directory: {}", parent.display())
            })?;
        }

        // Use JSON Canonicalization Scheme for deterministic output
        let content = serde_jcs::to_string(self)
            .with_context(|| "Failed to serialize lock file using JCS")?;

        // Add a newline at the end for better git diffs
        let content_with_newline = format!("{content}\n");

        fs::write(&path, content_with_newline)
            .with_context(|| format!("Failed to write lock file: {}", path.display()))?;

        Ok(())
    }

    /// Get the locked version for a package in a specific target and sysroot
    pub fn get_locked_version(
        &self,
        target: &str,
        sysroot: &SysrootType,
        package: &str,
    ) -> Option<&String> {
        let target_locks = self.targets.get(target)?;

        match sysroot {
            SysrootType::Sdk(arch) => target_locks
                .sdk
                .get(arch)
                .and_then(|pkgs| pkgs.get(package)),
            SysrootType::Rootfs => target_locks.rootfs.get(package),
            SysrootType::TargetSysroot => target_locks.target_sysroot.get(package),
            SysrootType::Extension(name) | SysrootType::VersionedExtension(name) => target_locks
                .extensions
                .get(name)
                .and_then(|pkgs| pkgs.get(package)),
            SysrootType::Runtime(name) => target_locks
                .runtimes
                .get(name)
                .and_then(|pkgs| pkgs.get(package)),
        }
    }

    /// Set the locked version for a package in a specific target and sysroot
    #[allow(dead_code)]
    pub fn set_locked_version(
        &mut self,
        target: &str,
        sysroot: &SysrootType,
        package: &str,
        version: &str,
    ) {
        let target_locks = self.targets.entry(target.to_string()).or_default();

        let packages = match sysroot {
            SysrootType::Sdk(arch) => target_locks.sdk.entry(arch.clone()).or_default(),
            SysrootType::Rootfs => &mut target_locks.rootfs,
            SysrootType::TargetSysroot => &mut target_locks.target_sysroot,
            SysrootType::Extension(name) | SysrootType::VersionedExtension(name) => {
                target_locks.extensions.entry(name.clone()).or_default()
            }
            SysrootType::Runtime(name) => target_locks.runtimes.entry(name.clone()).or_default(),
        };

        packages.insert(package.to_string(), version.to_string());
    }

    /// Update multiple package versions for a target and sysroot at once
    pub fn update_sysroot_versions(
        &mut self,
        target: &str,
        sysroot: &SysrootType,
        versions: HashMap<String, String>,
    ) {
        let target_locks = self.targets.entry(target.to_string()).or_default();

        let packages = match sysroot {
            SysrootType::Sdk(arch) => target_locks.sdk.entry(arch.clone()).or_default(),
            SysrootType::Rootfs => &mut target_locks.rootfs,
            SysrootType::TargetSysroot => &mut target_locks.target_sysroot,
            SysrootType::Extension(name) | SysrootType::VersionedExtension(name) => {
                target_locks.extensions.entry(name.clone()).or_default()
            }
            SysrootType::Runtime(name) => target_locks.runtimes.entry(name.clone()).or_default(),
        };

        for (package, version) in versions {
            packages.insert(package, version);
        }
    }

    /// Get all locked versions for a target and sysroot
    /// Returns None if no packages are recorded for this sysroot
    #[allow(dead_code)]
    pub fn get_sysroot_versions(
        &self,
        target: &str,
        sysroot: &SysrootType,
    ) -> Option<&HashMap<String, String>> {
        let target_locks = self.targets.get(target)?;

        let result = match sysroot {
            SysrootType::Sdk(arch) => target_locks.sdk.get(arch),
            SysrootType::Rootfs => Some(&target_locks.rootfs),
            SysrootType::TargetSysroot => Some(&target_locks.target_sysroot),
            SysrootType::Extension(name) | SysrootType::VersionedExtension(name) => {
                target_locks.extensions.get(name)
            }
            SysrootType::Runtime(name) => target_locks.runtimes.get(name),
        };

        // Return None for empty collections (matches expected behavior)
        result.filter(|m| !m.is_empty())
    }

    /// Check if the lock file has any entries
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
            || self.targets.values().all(|target_locks| {
                target_locks.sdk.is_empty()
                    && target_locks.rootfs.is_empty()
                    && target_locks.target_sysroot.is_empty()
                    && target_locks.extensions.is_empty()
                    && target_locks.runtimes.is_empty()
            })
    }

    /// Clear all SDK entries for a specific target (all architectures)
    pub fn clear_sdk(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.sdk.clear();
        }
    }

    /// Clear rootfs entries for a specific target
    pub fn clear_rootfs(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.rootfs.clear();
        }
    }

    /// Clear target-sysroot entries for a specific target
    pub fn clear_target_sysroot(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.target_sysroot.clear();
        }
    }

    /// Clear a specific extension's entries for a target
    pub fn clear_extension(&mut self, target: &str, extension_name: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.extensions.remove(extension_name);
        }
    }

    /// Clear all extension entries for a target
    #[allow(dead_code)]
    pub fn clear_all_extensions(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.extensions.clear();
        }
    }

    /// Clear a specific runtime's entries for a target
    pub fn clear_runtime(&mut self, target: &str, runtime_name: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.runtimes.remove(runtime_name);
        }
    }

    /// Clear all runtime entries for a target
    #[allow(dead_code)]
    pub fn clear_all_runtimes(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.runtimes.clear();
        }
    }

    /// Clear all entries for a target (SDK, rootfs, target-sysroot, extensions, runtimes)
    pub fn clear_all(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.sdk.clear();
            target_locks.rootfs.clear();
            target_locks.target_sysroot.clear();
            target_locks.extensions.clear();
            target_locks.runtimes.clear();
        }
    }

    /// Get all target names in the lock file
    #[allow(dead_code)]
    pub fn get_targets(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }
}

/// Parse rpm -q output into a map of package names to versions
/// Expected format: "NAME VERSION-RELEASE.ARCH" per line
///
/// For SDK packages, we strip the architecture suffix to make the lock file portable
/// across different host architectures (x86_64, aarch64, etc.)
pub fn parse_rpm_query_output(output: &str, strip_arch: bool) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Skip lines that indicate package not installed
        if line.contains("is not installed") {
            continue;
        }

        // Skip info/error/debug lines from container output
        if line.starts_with("[INFO]")
            || line.starts_with("[ERROR]")
            || line.starts_with("[SUCCESS]")
            || line.starts_with("[DEBUG]")
            || line.starts_with("[WARNING]")
        {
            continue;
        }

        // Split on first space: NAME VERSION-RELEASE.ARCH
        if let Some((name, version)) = line.split_once(' ') {
            // Additional validation: package names shouldn't contain brackets or special chars
            if name.starts_with('[') || name.contains('=') {
                continue;
            }

            let version_to_store = if strip_arch {
                // Strip the architecture suffix (.ARCH) from the version
                // Format: VERSION-RELEASE.ARCH -> VERSION-RELEASE
                if let Some(idx) = version.rfind('.') {
                    version[..idx].to_string()
                } else {
                    version.to_string()
                }
            } else {
                version.to_string()
            };

            result.insert(name.to_string(), version_to_store);
        }
    }

    result
}

/// Build a package specification for DNF install, using locked version if available
/// Returns the package spec string (e.g., "curl" or "curl-7.88.1-r0.core2_64")
pub fn build_package_spec_with_lock(
    lock_file: &LockFile,
    target: &str,
    sysroot: &SysrootType,
    package_name: &str,
    config_version: &str,
) -> String {
    // First, check if we have a locked version for this target
    if let Some(locked_version) = lock_file.get_locked_version(target, sysroot, package_name) {
        // Use the full locked version (NEVRA format)
        format!("{package_name}-{locked_version}")
    } else if config_version == "*" {
        // No lock and config says latest - just use package name
        package_name.to_string()
    } else {
        // No lock but config specifies a version
        format!("{package_name}-{config_version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_lock_file_new() {
        let lock = LockFile::new();
        assert_eq!(lock.version, LOCKFILE_VERSION);
        assert!(lock.targets.is_empty());
    }

    #[test]
    fn test_lock_file_get_set_version() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());

        lock.set_locked_version(target, &sdk_x86, "test-package", "1.0.0-r0.x86_64");

        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "test-package"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );

        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "nonexistent"),
            None
        );

        // Different host architecture should not have the package
        assert_eq!(
            lock.get_locked_version(target, &sdk_aarch64, "test-package"),
            None
        );

        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Rootfs, "test-package"),
            None
        );

        // Different target should not have the package
        assert_eq!(
            lock.get_locked_version("qemuarm64", &sdk_x86, "test-package"),
            None
        );
    }

    #[test]
    fn test_lock_file_save_load() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();
        let target = "qemux86-64";

        let mut lock = LockFile::new();
        lock.set_locked_version(
            target,
            &SysrootType::Extension("my-app".to_string()),
            "curl",
            "7.88.1-r0.core2_64",
        );

        lock.save(src_dir).unwrap();

        let loaded = LockFile::load(src_dir).unwrap();
        assert_eq!(loaded.version, LOCKFILE_VERSION);
        assert_eq!(
            loaded.get_locked_version(
                target,
                &SysrootType::Extension("my-app".to_string()),
                "curl"
            ),
            Some(&"7.88.1-r0.core2_64".to_string())
        );
    }

    #[test]
    fn test_lock_file_load_nonexistent() {
        let temp_dir = TempDir::new().unwrap();
        let lock = LockFile::load(temp_dir.path()).unwrap();
        assert!(lock.is_empty());
    }

    #[test]
    fn test_parse_rpm_query_output() {
        let output = r#"curl 7.88.1-r0.core2_64
openssl 3.0.8-r0.core2_64
package-xyz is not installed
wget 1.21-r0.core2_64
"#;

        // Test without stripping architecture
        let result = parse_rpm_query_output(output, false);
        assert_eq!(result.len(), 3);
        assert_eq!(result.get("curl"), Some(&"7.88.1-r0.core2_64".to_string()));
        assert_eq!(
            result.get("openssl"),
            Some(&"3.0.8-r0.core2_64".to_string())
        );
        assert_eq!(result.get("wget"), Some(&"1.21-r0.core2_64".to_string()));
        assert_eq!(result.get("package-xyz"), None);

        // Test with stripping architecture (for SDK packages)
        let result_stripped = parse_rpm_query_output(output, true);
        assert_eq!(result_stripped.len(), 3);
        assert_eq!(result_stripped.get("curl"), Some(&"7.88.1-r0".to_string()));
        assert_eq!(
            result_stripped.get("openssl"),
            Some(&"3.0.8-r0".to_string())
        );
        assert_eq!(result_stripped.get("wget"), Some(&"1.21-r0".to_string()));
    }

    #[test]
    fn test_build_package_spec_with_lock() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());
        lock.set_locked_version(target, &sdk_x86, "curl", "7.88.1-r0.x86_64");

        // Should use locked version
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_x86, "curl", "*"),
            "curl-7.88.1-r0.x86_64"
        );

        // No lock, config says latest
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_x86, "wget", "*"),
            "wget"
        );

        // No lock, config specifies version
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_x86, "wget", "1.21"),
            "wget-1.21"
        );

        // Different target should not have curl locked
        assert_eq!(
            build_package_spec_with_lock(&lock, "qemuarm64", &sdk_x86, "curl", "*"),
            "curl"
        );

        // Different host architecture should not have curl locked
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_aarch64, "curl", "*"),
            "curl"
        );
    }

    #[test]
    fn test_rpm_query_config_build_command() {
        // Test with root_path (for installroot sysroot queries)
        // These must explicitly UNSET the env vars to override entrypoint settings
        let config = RpmQueryConfig {
            rpm_etcconfigdir: None,
            rpm_configdir: None,
            root_path: Some("$AVOCADO_EXT_SYSROOTS/my-ext".to_string()),
        };

        let cmd = config.build_query_command(&["curl".to_string(), "wget".to_string()]);
        // Should use a subshell with unset to properly remove env vars
        assert!(cmd.contains("unset RPM_ETCCONFIGDIR RPM_CONFIGDIR"));
        assert!(cmd.contains("--root=\"$AVOCADO_EXT_SYSROOTS/my-ext\""));
        assert!(cmd.contains("curl wget"));

        // Test without root_path (for SDK native queries)
        let sdk_config = RpmQueryConfig {
            rpm_etcconfigdir: Some("$AVOCADO_SDK_PREFIX".to_string()),
            rpm_configdir: Some("$AVOCADO_SDK_PREFIX/usr/lib/rpm".to_string()),
            root_path: None,
        };

        let sdk_cmd = sdk_config.build_query_command(&["pkg1".to_string(), "pkg2".to_string()]);
        assert!(sdk_cmd.contains("RPM_ETCCONFIGDIR=\"$AVOCADO_SDK_PREFIX\""));
        assert!(sdk_cmd.contains("RPM_CONFIGDIR=\"$AVOCADO_SDK_PREFIX/usr/lib/rpm\""));
        assert!(!sdk_cmd.contains("--root"));
        assert!(sdk_cmd.contains("pkg1 pkg2"));
    }

    #[test]
    fn test_update_sysroot_versions() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        let mut versions = HashMap::new();
        versions.insert("pkg1".to_string(), "1.0.0-r0.x86_64".to_string());
        versions.insert("pkg2".to_string(), "2.0.0-r0.x86_64".to_string());

        lock.update_sysroot_versions(target, &sdk_x86, versions);

        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "pkg1"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "pkg2"),
            Some(&"2.0.0-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_multiple_targets_and_host_archs() {
        let mut lock = LockFile::new();
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());

        // Set versions for two different targets on same host arch
        lock.set_locked_version("qemux86-64", &sdk_x86, "curl", "7.88.1-r0");
        lock.set_locked_version("qemuarm64", &sdk_x86, "curl", "7.88.1-r0");

        // Set version for same target but different host arch
        lock.set_locked_version("qemux86-64", &sdk_aarch64, "curl", "7.88.1-r0.4");

        // Each target+arch combo should have its own version
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sdk_x86, "curl"),
            Some(&"7.88.1-r0".to_string())
        );
        assert_eq!(
            lock.get_locked_version("qemuarm64", &sdk_x86, "curl"),
            Some(&"7.88.1-r0".to_string())
        );
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sdk_aarch64, "curl"),
            Some(&"7.88.1-r0.4".to_string())
        );
    }

    #[test]
    fn test_is_empty() {
        let lock = LockFile::new();
        assert!(lock.is_empty());

        let mut lock = LockFile::new();
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        lock.set_locked_version("qemux86-64", &sdk_x86, "curl", "7.88.1-r0.x86_64");
        assert!(!lock.is_empty());
    }

    #[test]
    fn test_lock_file_path() {
        let path = LockFile::get_path(std::path::Path::new("/home/user/project"));
        assert_eq!(
            path,
            std::path::PathBuf::from("/home/user/project/.avocado/lock.json")
        );
    }

    #[test]
    fn test_multiple_sysroots_same_target() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        // Set versions for different sysroots under the same target
        lock.set_locked_version(target, &sdk_x86, "toolchain", "1.0.0-r0.x86_64");
        lock.set_locked_version(
            target,
            &SysrootType::Rootfs,
            "base-files",
            "3.0.0-r0.core2_64",
        );
        lock.set_locked_version(
            target,
            &SysrootType::Extension("my-app".to_string()),
            "libfoo",
            "2.0.0-r0.core2_64",
        );
        lock.set_locked_version(
            target,
            &SysrootType::Runtime("dev".to_string()),
            "runtime-base",
            "1.5.0-r0.core2_64",
        );
        lock.set_locked_version(
            target,
            &SysrootType::TargetSysroot,
            "glibc",
            "2.37-r0.core2_64",
        );

        // Verify each sysroot has its package
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "toolchain"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Rootfs, "base-files"),
            Some(&"3.0.0-r0.core2_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(
                target,
                &SysrootType::Extension("my-app".to_string()),
                "libfoo"
            ),
            Some(&"2.0.0-r0.core2_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(
                target,
                &SysrootType::Runtime("dev".to_string()),
                "runtime-base"
            ),
            Some(&"1.5.0-r0.core2_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::TargetSysroot, "glibc"),
            Some(&"2.37-r0.core2_64".to_string())
        );

        // Verify cross-sysroot isolation
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "base-files"),
            None
        );
    }

    #[test]
    fn test_version_update_overwrites() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        lock.set_locked_version(target, &sdk_x86, "curl", "7.88.0-r0.x86_64");
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "curl"),
            Some(&"7.88.0-r0.x86_64".to_string())
        );

        // Update to new version
        lock.set_locked_version(target, &sdk_x86, "curl", "7.88.1-r0.x86_64");
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "curl"),
            Some(&"7.88.1-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_update_sysroot_versions_merges() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        // Add initial packages
        let mut versions1 = HashMap::new();
        versions1.insert("pkg1".to_string(), "1.0.0-r0.x86_64".to_string());
        lock.update_sysroot_versions(target, &sdk_x86, versions1);

        // Add more packages (should merge, not replace)
        let mut versions2 = HashMap::new();
        versions2.insert("pkg2".to_string(), "2.0.0-r0.x86_64".to_string());
        lock.update_sysroot_versions(target, &sdk_x86, versions2);

        // Both packages should exist
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "pkg1"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &sdk_x86, "pkg2"),
            Some(&"2.0.0-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_get_sysroot_versions() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        lock.set_locked_version(target, &sdk_x86, "pkg1", "1.0.0-r0.x86_64");
        lock.set_locked_version(target, &sdk_x86, "pkg2", "2.0.0-r0.x86_64");

        let versions = lock.get_sysroot_versions(target, &sdk_x86);
        assert!(versions.is_some());
        let versions = versions.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions.get("pkg1"), Some(&"1.0.0-r0.x86_64".to_string()));
        assert_eq!(versions.get("pkg2"), Some(&"2.0.0-r0.x86_64".to_string()));

        // Non-existent sysroot should return None
        assert!(lock
            .get_sysroot_versions(target, &SysrootType::Rootfs)
            .is_none());

        // Non-existent target should return None
        assert!(lock.get_sysroot_versions("nonexistent", &sdk_x86).is_none());
    }

    #[test]
    fn test_parse_rpm_query_output_edge_cases() {
        // Empty output
        let result = parse_rpm_query_output("", false);
        assert!(result.is_empty());

        // Whitespace only
        let result = parse_rpm_query_output("   \n  \n  ", false);
        assert!(result.is_empty());

        // Only "not installed" messages
        let result = parse_rpm_query_output(
            "package-a is not installed\npackage-b is not installed",
            false,
        );
        assert!(result.is_empty());

        // Mixed valid and invalid
        let output =
            "valid-pkg 1.0.0-r0.x86_64\nbad-pkg is not installed\nanother-pkg 2.0.0-r0.x86_64";
        let result = parse_rpm_query_output(output, false);
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("valid-pkg"));
        assert!(result.contains_key("another-pkg"));
    }

    #[test]
    fn test_parse_rpm_query_output_filters_info_lines() {
        // Output mixed with container info messages
        let output = r#"[INFO] Using repo URL: 'http://192.168.1.10:8080'
[INFO] Using repo release: 'latest/apollo/edge'
curl 7.88.1-r0.core2_64
openssl 3.0.8-r0.core2_64
[ERROR] Some error message
[SUCCESS] Something succeeded
wget 1.21-r0.core2_64
[DEBUG] Debug info
[WARNING] Warning message
"#;

        let result = parse_rpm_query_output(output, false);
        assert_eq!(result.len(), 3);
        assert_eq!(result.get("curl"), Some(&"7.88.1-r0.core2_64".to_string()));
        assert_eq!(
            result.get("openssl"),
            Some(&"3.0.8-r0.core2_64".to_string())
        );
        assert_eq!(result.get("wget"), Some(&"1.21-r0.core2_64".to_string()));
        // These should NOT be in the result
        assert_eq!(result.get("[INFO]"), None);
        assert_eq!(result.get("[ERROR]"), None);
        assert_eq!(result.get("[SUCCESS]"), None);
        assert_eq!(result.get("[DEBUG]"), None);
        assert_eq!(result.get("[WARNING]"), None);
    }

    #[test]
    fn test_parse_rpm_query_output_strips_arch_for_sdk() {
        // Test SDK package output with architecture stripping
        let output = r#"nativesdk-curl 7.88.1-r0.x86_64_avocadosdk
nativesdk-openssl 3.0.8-r0.x86_64_avocadosdk
avocado-sdk-toolchain 0.1.0-r0.x86_64_avocadosdk
"#;

        // With strip_arch=true (for SDK packages)
        let result_stripped = parse_rpm_query_output(output, true);
        assert_eq!(result_stripped.len(), 3);
        assert_eq!(
            result_stripped.get("nativesdk-curl"),
            Some(&"7.88.1-r0".to_string())
        );
        assert_eq!(
            result_stripped.get("nativesdk-openssl"),
            Some(&"3.0.8-r0".to_string())
        );
        assert_eq!(
            result_stripped.get("avocado-sdk-toolchain"),
            Some(&"0.1.0-r0".to_string())
        );

        // With strip_arch=false (for non-SDK packages)
        let result_full = parse_rpm_query_output(output, false);
        assert_eq!(result_full.len(), 3);
        assert_eq!(
            result_full.get("nativesdk-curl"),
            Some(&"7.88.1-r0.x86_64_avocadosdk".to_string())
        );
        assert_eq!(
            result_full.get("nativesdk-openssl"),
            Some(&"3.0.8-r0.x86_64_avocadosdk".to_string())
        );
        assert_eq!(
            result_full.get("avocado-sdk-toolchain"),
            Some(&"0.1.0-r0.x86_64_avocadosdk".to_string())
        );
    }

    #[test]
    fn test_rpm_query_config_without_configdir() {
        // For installroot queries, we must explicitly UNSET env vars to override entrypoint
        let config = RpmQueryConfig {
            rpm_etcconfigdir: None,
            rpm_configdir: None,
            root_path: Some("$AVOCADO_PREFIX/rootfs".to_string()),
        };

        let cmd = config.build_query_command(&["curl".to_string()]);
        // Should use a subshell with unset to properly remove env vars
        assert!(cmd.contains("unset RPM_ETCCONFIGDIR RPM_CONFIGDIR"));
        assert!(cmd.contains("--root=\"$AVOCADO_PREFIX/rootfs\""));
        // Command should start with subshell
        assert!(cmd.starts_with("(unset"));
    }

    #[test]
    fn test_sysroot_type_get_rpm_query_config() {
        // Test SDK config - no root_path because SDK uses native container with custom RPM_CONFIGDIR
        // The arch in SDK sysroot type doesn't affect the RPM query config
        let sdk_config = SysrootType::Sdk("x86_64".to_string()).get_rpm_query_config();
        assert_eq!(
            sdk_config.rpm_etcconfigdir,
            Some("$AVOCADO_SDK_PREFIX".to_string())
        );
        assert!(sdk_config.rpm_configdir.is_some());
        assert!(sdk_config.root_path.is_none()); // SDK doesn't use --root

        // Different arch should produce the same RPM query config
        let sdk_config_aarch64 = SysrootType::Sdk("aarch64".to_string()).get_rpm_query_config();
        assert_eq!(
            sdk_config.rpm_etcconfigdir,
            sdk_config_aarch64.rpm_etcconfigdir
        );
        assert_eq!(sdk_config.rpm_configdir, sdk_config_aarch64.rpm_configdir);

        // Test Rootfs config - installroots don't need RPM_ETCCONFIGDIR, just --root
        let rootfs_config = SysrootType::Rootfs.get_rpm_query_config();
        assert!(rootfs_config.rpm_etcconfigdir.is_none());
        assert!(rootfs_config.rpm_configdir.is_none());
        assert_eq!(
            rootfs_config.root_path,
            Some("$AVOCADO_PREFIX/rootfs".to_string())
        );

        // Test Extension config - local/external extensions don't need RPM_CONFIGDIR
        let ext_config = SysrootType::Extension("my-ext".to_string()).get_rpm_query_config();
        assert!(ext_config.rpm_etcconfigdir.is_none());
        assert!(ext_config.rpm_configdir.is_none());
        assert_eq!(
            ext_config.root_path,
            Some("$AVOCADO_EXT_SYSROOTS/my-ext".to_string())
        );

        // Test VersionedExtension config - needs RPM_CONFIGDIR for ext-rpm-config database location
        let versioned_ext_config =
            SysrootType::VersionedExtension("my-versioned-ext".to_string()).get_rpm_query_config();
        assert!(versioned_ext_config.rpm_etcconfigdir.is_none());
        assert_eq!(
            versioned_ext_config.rpm_configdir,
            Some("$AVOCADO_SDK_PREFIX/ext-rpm-config".to_string())
        );
        assert_eq!(
            versioned_ext_config.root_path,
            Some("$AVOCADO_EXT_SYSROOTS/my-versioned-ext".to_string())
        );

        // Test Runtime config - same as rootfs, no custom config needed
        let runtime_config = SysrootType::Runtime("dev".to_string()).get_rpm_query_config();
        assert!(runtime_config.rpm_etcconfigdir.is_none());
        assert!(runtime_config.rpm_configdir.is_none());
        assert_eq!(
            runtime_config.root_path,
            Some("$AVOCADO_PREFIX/runtimes/dev".to_string())
        );

        // Test TargetSysroot config - same as rootfs, no custom config needed
        let target_config = SysrootType::TargetSysroot.get_rpm_query_config();
        assert!(target_config.rpm_etcconfigdir.is_none());
        assert!(target_config.rpm_configdir.is_none());
        assert_eq!(
            target_config.root_path,
            Some("$AVOCADO_PREFIX/sdk/target-sysroot".to_string())
        );
    }

    #[test]
    fn test_lock_file_json_format() {
        let mut lock = LockFile::new();
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());
        lock.set_locked_version("qemux86-64", &sdk_x86, "curl", "7.88.1-r0.x86_64");
        lock.set_locked_version(
            "qemux86-64",
            &SysrootType::Extension("app".to_string()),
            "libfoo",
            "1.0.0-r0.core2_64",
        );
        lock.set_locked_version("qemuarm64", &sdk_aarch64, "curl", "7.88.1-r0.aarch64");

        let json = serde_json::to_string_pretty(&lock).unwrap();

        // Verify JSON structure - SDK is nested under arch, extensions under name
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], 2);
        // SDK packages nested under sdk -> {arch} -> {package}
        assert!(parsed["targets"]["qemux86-64"]["sdk"]["x86_64"]["curl"].is_string());
        // Extensions nested under extensions -> {name} -> {package}
        assert!(parsed["targets"]["qemux86-64"]["extensions"]["app"]["libfoo"].is_string());
        assert!(parsed["targets"]["qemuarm64"]["sdk"]["aarch64"]["curl"].is_string());
    }

    #[test]
    fn test_lock_file_persistence_multiple_targets() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());

        // Create lock file with multiple targets and sysroots
        let mut lock = LockFile::new();
        lock.set_locked_version("qemux86-64", &sdk_x86, "toolchain", "1.0.0-r0.x86_64");
        lock.set_locked_version(
            "qemux86-64",
            &SysrootType::Rootfs,
            "base",
            "1.0.0-r0.core2_64",
        );
        lock.set_locked_version("qemuarm64", &sdk_aarch64, "toolchain", "1.0.0-r0.aarch64");
        lock.set_locked_version(
            "qemuarm64",
            &SysrootType::Extension("app".to_string()),
            "libapp",
            "2.0.0-r0.cortexa57",
        );

        lock.save(src_dir).unwrap();

        // Load and verify
        let loaded = LockFile::load(src_dir).unwrap();

        assert_eq!(
            loaded.get_locked_version("qemux86-64", &sdk_x86, "toolchain"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            loaded.get_locked_version("qemux86-64", &SysrootType::Rootfs, "base"),
            Some(&"1.0.0-r0.core2_64".to_string())
        );
        assert_eq!(
            loaded.get_locked_version("qemuarm64", &sdk_aarch64, "toolchain"),
            Some(&"1.0.0-r0.aarch64".to_string())
        );
        assert_eq!(
            loaded.get_locked_version(
                "qemuarm64",
                &SysrootType::Extension("app".to_string()),
                "libapp"
            ),
            Some(&"2.0.0-r0.cortexa57".to_string())
        );
    }

    #[test]
    fn test_build_package_spec_locked_overrides_config() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        // Set a locked version
        lock.set_locked_version(target, &sdk_x86, "curl", "7.88.1-r0.x86_64");

        // Even if config specifies a different version, locked version should be used
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_x86, "curl", "7.80.0"),
            "curl-7.88.1-r0.x86_64"
        );

        // And if config says "*", locked version should still be used
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &sdk_x86, "curl", "*"),
            "curl-7.88.1-r0.x86_64"
        );
    }

    #[test]
    fn test_default_impl() {
        let lock: LockFile = Default::default();
        assert_eq!(lock.version, LOCKFILE_VERSION);
        assert!(lock.targets.is_empty());
    }

    #[test]
    fn test_jcs_deterministic_output() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());

        // Create a lock file with packages in non-alphabetical order
        let mut lock1 = LockFile::new();
        lock1.set_locked_version("qemux86-64", &sdk_x86, "zebra", "1.0.0-r0");
        lock1.set_locked_version("qemux86-64", &sdk_x86, "alpha", "2.0.0-r0");
        lock1.set_locked_version("qemux86-64", &SysrootType::Rootfs, "beta", "3.0.0-r0");
        lock1.set_locked_version("qemuarm64", &sdk_aarch64, "gamma", "4.0.0-r0");

        lock1.save(src_dir).unwrap();
        let content1 = fs::read_to_string(LockFile::get_path(src_dir)).unwrap();

        // Create another lock file with same data but added in different order
        let mut lock2 = LockFile::new();
        lock2.set_locked_version("qemuarm64", &sdk_aarch64, "gamma", "4.0.0-r0");
        lock2.set_locked_version("qemux86-64", &SysrootType::Rootfs, "beta", "3.0.0-r0");
        lock2.set_locked_version("qemux86-64", &sdk_x86, "alpha", "2.0.0-r0");
        lock2.set_locked_version("qemux86-64", &sdk_x86, "zebra", "1.0.0-r0");

        // Remove the first lock file and save the second
        fs::remove_file(LockFile::get_path(src_dir)).unwrap();
        lock2.save(src_dir).unwrap();
        let content2 = fs::read_to_string(LockFile::get_path(src_dir)).unwrap();

        // JCS ensures both produce identical output regardless of insertion order
        assert_eq!(
            content1, content2,
            "JCS should produce identical output regardless of insertion order"
        );

        // Verify keys are sorted (targets should be alphabetically ordered)
        assert!(
            content1.find("qemuarm64").unwrap() < content1.find("qemux86-64").unwrap(),
            "Target keys should be alphabetically sorted"
        );

        // Verify package keys are sorted within sdk -> x86_64 nested object
        let sdk_start = content1.find("\"sdk\"").unwrap();
        let x86_start = content1[sdk_start..].find("\"x86_64\"").unwrap() + sdk_start;
        let alpha_pos = content1[x86_start..].find("\"alpha\"").unwrap() + x86_start;
        let zebra_pos = content1[x86_start..].find("\"zebra\"").unwrap() + x86_start;
        assert!(
            alpha_pos < zebra_pos,
            "Package keys should be alphabetically sorted"
        );
    }

    #[test]
    fn test_migrate_v1_to_v2() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();

        // Create a v1 lock file manually (with old flat key format)
        // v1 had: "sdk", "rootfs", "extensions/name", "runtimes/name" as flat keys
        let v1_content = r#"{"targets":{"jetson-orin-nano-devkit":{"extensions/my-app":{"libfoo":"1.0.0-r0"},"rootfs":{"avocado-pkg-rootfs":"0.1.0-r0.0.avocado_jetson_orin_nano_devkit"},"runtimes/dev":{"runtime-base":"2.0.0-r0"},"sdk":{"avocado-sdk-bootstrap":"0.1.0-r0.0","avocado-sdk-toolchain":"0.1.0-r0.4"}}},"version":1}
"#;

        let lock_path = LockFile::get_path(src_dir);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, v1_content).unwrap();

        // Load the lock file - should trigger migration
        let lock = LockFile::load(src_dir).unwrap();

        // Version should be updated to 2
        assert_eq!(lock.version, 2);

        // Old "sdk" entries should be removed (we can't determine their host arch)
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());
        let sdk_aarch64 = SysrootType::Sdk("aarch64".to_string());
        assert_eq!(
            lock.get_locked_version("jetson-orin-nano-devkit", &sdk_x86, "avocado-sdk-toolchain"),
            None
        );
        assert_eq!(
            lock.get_locked_version(
                "jetson-orin-nano-devkit",
                &sdk_aarch64,
                "avocado-sdk-toolchain"
            ),
            None
        );

        // Rootfs entries should be preserved (they're not arch-dependent)
        assert_eq!(
            lock.get_locked_version(
                "jetson-orin-nano-devkit",
                &SysrootType::Rootfs,
                "avocado-pkg-rootfs"
            ),
            Some(&"0.1.0-r0.0.avocado_jetson_orin_nano_devkit".to_string())
        );

        // Extensions should be migrated from "extensions/name" to nested structure
        assert_eq!(
            lock.get_locked_version(
                "jetson-orin-nano-devkit",
                &SysrootType::Extension("my-app".to_string()),
                "libfoo"
            ),
            Some(&"1.0.0-r0".to_string())
        );

        // Runtimes should be migrated from "runtimes/name" to nested structure
        assert_eq!(
            lock.get_locked_version(
                "jetson-orin-nano-devkit",
                &SysrootType::Runtime("dev".to_string()),
                "runtime-base"
            ),
            Some(&"2.0.0-r0".to_string())
        );
    }
}
