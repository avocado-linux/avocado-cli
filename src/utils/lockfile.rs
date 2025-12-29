//! Lock file utilities for reproducible DNF package installations.
//!
//! This module provides functionality to track and pin package versions
//! across different sysroots to ensure reproducible builds.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Current lock file format version
const LOCKFILE_VERSION: u32 = 1;

/// Lock file name
const LOCKFILE_NAME: &str = "lock.json";

/// Lock file directory within src_dir
const LOCKFILE_DIR: &str = ".avocado";

/// Represents different sysroot types for package installation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SysrootType {
    /// SDK sysroot ($AVOCADO_SDK_PREFIX)
    Sdk,
    /// Rootfs sysroot ($AVOCADO_PREFIX/rootfs)
    Rootfs,
    /// Target sysroot ($AVOCADO_PREFIX/sdk/target-sysroot)
    TargetSysroot,
    /// Local/external extension sysroot ($AVOCADO_EXT_SYSROOTS/{name})
    /// Uses ext-rpm-config-scripts for RPM database
    Extension(String),
    /// Versioned extension sysroot ($AVOCADO_EXT_SYSROOTS/{name})
    /// Uses ext-rpm-config for RPM database (different location than local extensions)
    VersionedExtension(String),
    /// Runtime sysroot ($AVOCADO_PREFIX/runtimes/{name})
    Runtime(String),
}

impl SysrootType {
    /// Convert sysroot type to its string key for the lock file
    pub fn to_key(&self) -> String {
        match self {
            SysrootType::Sdk => "sdk".to_string(),
            SysrootType::Rootfs => "rootfs".to_string(),
            SysrootType::TargetSysroot => "target-sysroot".to_string(),
            // Both Extension and VersionedExtension use the same key format
            // They're distinguished at query time but stored the same in lock file
            SysrootType::Extension(name) | SysrootType::VersionedExtension(name) => {
                format!("extensions/{}", name)
            }
            SysrootType::Runtime(name) => format!("runtimes/{}", name),
        }
    }

    /// Parse a string key back to a SysrootType
    #[allow(dead_code)]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "sdk" => Some(SysrootType::Sdk),
            "rootfs" => Some(SysrootType::Rootfs),
            "target-sysroot" => Some(SysrootType::TargetSysroot),
            _ if key.starts_with("extensions/") => Some(SysrootType::Extension(
                key.strip_prefix("extensions/")?.to_string(),
            )),
            _ if key.starts_with("runtimes/") => Some(SysrootType::Runtime(
                key.strip_prefix("runtimes/")?.to_string(),
            )),
            _ => None,
        }
    }

    /// Get the RPM query command environment and root path for this sysroot type
    /// Returns (rpm_etcconfigdir, rpm_configdir, root_path) as shell variable expressions
    ///
    /// For SDK packages, root_path is None because SDK packages are installed into
    /// the native container root but tracked via custom RPM_CONFIGDIR macros that
    /// point to $AVOCADO_SDK_PREFIX/var/lib/rpm.
    pub fn get_rpm_query_config(&self) -> RpmQueryConfig {
        match self {
            SysrootType::Sdk => RpmQueryConfig {
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
                root_path: Some(format!("$AVOCADO_EXT_SYSROOTS/{}", name)),
            },
            SysrootType::VersionedExtension(name) => RpmQueryConfig {
                // Versioned extensions use ext-rpm-config which puts database at custom location
                // We need to set RPM_CONFIGDIR to find the database correctly
                rpm_etcconfigdir: None,
                rpm_configdir: Some("$AVOCADO_SDK_PREFIX/ext-rpm-config".to_string()),
                root_path: Some(format!("$AVOCADO_EXT_SYSROOTS/{}", name)),
            },
            SysrootType::Runtime(name) => RpmQueryConfig {
                // Runtime: same approach as rootfs - unset config and use --root
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_PREFIX/runtimes/{}", name)),
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
                env_setup.push_str(&format!("export RPM_ETCCONFIGDIR=\"{}\"; ", etcconfigdir));
            }
            if let Some(ref configdir) = self.rpm_configdir {
                env_setup.push_str(&format!("export RPM_CONFIGDIR=\"{}\"; ", configdir));
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
                cmd.push_str(&format!("RPM_ETCCONFIGDIR=\"{}\" ", etcconfigdir));
            }
            if let Some(ref configdir) = self.rpm_configdir {
                cmd.push_str(&format!("RPM_CONFIGDIR=\"{}\" ", configdir));
            }
            cmd.push_str(&format!(
                "rpm -q --qf '%{{NAME}} %{{VERSION}}-%{{RELEASE}}.%{{ARCH}}\\n' {} || true",
                packages.join(" ")
            ));
            cmd
        }
    }
}

/// Lock file structure for tracking installed package versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Lock file format version
    pub version: u32,
    /// Package versions organized by target architecture, then by sysroot
    /// Structure: targets -> target_name -> sysroot_key -> package_name -> version
    /// Example: targets["qemux86-64"]["sdk"]["avocado-sdk-toolchain"] = "0.1.0-r0.x86_64"
    pub targets: HashMap<String, HashMap<String, HashMap<String, String>>>,
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
    pub fn load(src_dir: &Path) -> Result<Self> {
        let path = Self::get_path(src_dir);

        if !path.exists() {
            return Ok(Self::new());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read lock file: {}", path.display()))?;

        let lock_file: LockFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse lock file: {}", path.display()))?;

        // Check version compatibility
        if lock_file.version > LOCKFILE_VERSION {
            anyhow::bail!(
                "Lock file version {} is newer than supported version {}. Please upgrade avocado-cli.",
                lock_file.version,
                LOCKFILE_VERSION
            );
        }

        Ok(lock_file)
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
        let content_with_newline = format!("{}\n", content);

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
        let sysroot_key = sysroot.to_key();
        self.targets
            .get(target)
            .and_then(|sysroots| sysroots.get(&sysroot_key))
            .and_then(|packages| packages.get(package))
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
        let sysroot_key = sysroot.to_key();
        self.targets
            .entry(target.to_string())
            .or_default()
            .entry(sysroot_key)
            .or_default()
            .insert(package.to_string(), version.to_string());
    }

    /// Update multiple package versions for a target and sysroot at once
    pub fn update_sysroot_versions(
        &mut self,
        target: &str,
        sysroot: &SysrootType,
        versions: HashMap<String, String>,
    ) {
        let sysroot_key = sysroot.to_key();
        let entry = self
            .targets
            .entry(target.to_string())
            .or_default()
            .entry(sysroot_key)
            .or_default();
        for (package, version) in versions {
            entry.insert(package, version);
        }
    }

    /// Get all locked versions for a target and sysroot
    #[allow(dead_code)]
    pub fn get_sysroot_versions(
        &self,
        target: &str,
        sysroot: &SysrootType,
    ) -> Option<&HashMap<String, String>> {
        let sysroot_key = sysroot.to_key();
        self.targets
            .get(target)
            .and_then(|sysroots| sysroots.get(&sysroot_key))
    }

    /// Check if the lock file has any entries
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
            || self.targets.values().all(|sysroots| {
                sysroots.is_empty() || sysroots.values().all(|packages| packages.is_empty())
            })
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
        format!("{}-{}", package_name, locked_version)
    } else if config_version == "*" {
        // No lock and config says latest - just use package name
        package_name.to_string()
    } else {
        // No lock but config specifies a version
        format!("{}-{}", package_name, config_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_sysroot_type_to_key() {
        assert_eq!(SysrootType::Sdk.to_key(), "sdk");
        assert_eq!(SysrootType::Rootfs.to_key(), "rootfs");
        assert_eq!(SysrootType::TargetSysroot.to_key(), "target-sysroot");
        assert_eq!(
            SysrootType::Extension("my-app".to_string()).to_key(),
            "extensions/my-app"
        );
        // VersionedExtension and Extension produce the same key format
        // They're only distinguished at query time, not in the lock file
        assert_eq!(
            SysrootType::VersionedExtension("my-versioned-app".to_string()).to_key(),
            "extensions/my-versioned-app"
        );
        assert_eq!(
            SysrootType::Runtime("dev".to_string()).to_key(),
            "runtimes/dev"
        );
    }

    #[test]
    fn test_sysroot_type_from_key() {
        assert_eq!(SysrootType::from_key("sdk"), Some(SysrootType::Sdk));
        assert_eq!(SysrootType::from_key("rootfs"), Some(SysrootType::Rootfs));
        assert_eq!(
            SysrootType::from_key("target-sysroot"),
            Some(SysrootType::TargetSysroot)
        );
        assert_eq!(
            SysrootType::from_key("extensions/my-app"),
            Some(SysrootType::Extension("my-app".to_string()))
        );
        assert_eq!(
            SysrootType::from_key("runtimes/dev"),
            Some(SysrootType::Runtime("dev".to_string()))
        );
        assert_eq!(SysrootType::from_key("invalid"), None);
    }

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

        lock.set_locked_version(target, &SysrootType::Sdk, "test-package", "1.0.0-r0.x86_64");

        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "test-package"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );

        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "nonexistent"),
            None
        );

        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Rootfs, "test-package"),
            None
        );

        // Different target should not have the package
        assert_eq!(
            lock.get_locked_version("qemuarm64", &SysrootType::Sdk, "test-package"),
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
        lock.set_locked_version(target, &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");

        // Should use locked version
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &SysrootType::Sdk, "curl", "*"),
            "curl-7.88.1-r0.x86_64"
        );

        // No lock, config says latest
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &SysrootType::Sdk, "wget", "*"),
            "wget"
        );

        // No lock, config specifies version
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &SysrootType::Sdk, "wget", "1.21"),
            "wget-1.21"
        );

        // Different target should not have curl locked
        assert_eq!(
            build_package_spec_with_lock(&lock, "qemuarm64", &SysrootType::Sdk, "curl", "*"),
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

        let mut versions = HashMap::new();
        versions.insert("pkg1".to_string(), "1.0.0-r0.x86_64".to_string());
        versions.insert("pkg2".to_string(), "2.0.0-r0.x86_64".to_string());

        lock.update_sysroot_versions(target, &SysrootType::Sdk, versions);

        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "pkg1"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "pkg2"),
            Some(&"2.0.0-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_multiple_targets() {
        let mut lock = LockFile::new();

        // Set versions for two different targets
        lock.set_locked_version("qemux86-64", &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");
        lock.set_locked_version("qemuarm64", &SysrootType::Sdk, "curl", "7.88.1-r0.aarch64");

        // Each target should have its own version
        assert_eq!(
            lock.get_locked_version("qemux86-64", &SysrootType::Sdk, "curl"),
            Some(&"7.88.1-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version("qemuarm64", &SysrootType::Sdk, "curl"),
            Some(&"7.88.1-r0.aarch64".to_string())
        );
    }

    #[test]
    fn test_is_empty() {
        let lock = LockFile::new();
        assert!(lock.is_empty());

        let mut lock = LockFile::new();
        lock.set_locked_version("qemux86-64", &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");
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

        // Set versions for different sysroots under the same target
        lock.set_locked_version(target, &SysrootType::Sdk, "toolchain", "1.0.0-r0.x86_64");
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
            lock.get_locked_version(target, &SysrootType::Sdk, "toolchain"),
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
            lock.get_locked_version(target, &SysrootType::Sdk, "base-files"),
            None
        );
    }

    #[test]
    fn test_version_update_overwrites() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";

        lock.set_locked_version(target, &SysrootType::Sdk, "curl", "7.88.0-r0.x86_64");
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "curl"),
            Some(&"7.88.0-r0.x86_64".to_string())
        );

        // Update to new version
        lock.set_locked_version(target, &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "curl"),
            Some(&"7.88.1-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_update_sysroot_versions_merges() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";

        // Add initial packages
        let mut versions1 = HashMap::new();
        versions1.insert("pkg1".to_string(), "1.0.0-r0.x86_64".to_string());
        lock.update_sysroot_versions(target, &SysrootType::Sdk, versions1);

        // Add more packages (should merge, not replace)
        let mut versions2 = HashMap::new();
        versions2.insert("pkg2".to_string(), "2.0.0-r0.x86_64".to_string());
        lock.update_sysroot_versions(target, &SysrootType::Sdk, versions2);

        // Both packages should exist
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "pkg1"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            lock.get_locked_version(target, &SysrootType::Sdk, "pkg2"),
            Some(&"2.0.0-r0.x86_64".to_string())
        );
    }

    #[test]
    fn test_get_sysroot_versions() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";

        lock.set_locked_version(target, &SysrootType::Sdk, "pkg1", "1.0.0-r0.x86_64");
        lock.set_locked_version(target, &SysrootType::Sdk, "pkg2", "2.0.0-r0.x86_64");

        let versions = lock.get_sysroot_versions(target, &SysrootType::Sdk);
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
        assert!(lock
            .get_sysroot_versions("nonexistent", &SysrootType::Sdk)
            .is_none());
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
        let sdk_config = SysrootType::Sdk.get_rpm_query_config();
        assert_eq!(
            sdk_config.rpm_etcconfigdir,
            Some("$AVOCADO_SDK_PREFIX".to_string())
        );
        assert!(sdk_config.rpm_configdir.is_some());
        assert!(sdk_config.root_path.is_none()); // SDK doesn't use --root

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
        lock.set_locked_version("qemux86-64", &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");
        lock.set_locked_version(
            "qemux86-64",
            &SysrootType::Extension("app".to_string()),
            "libfoo",
            "1.0.0-r0.core2_64",
        );
        lock.set_locked_version("qemuarm64", &SysrootType::Sdk, "curl", "7.88.1-r0.aarch64");

        let json = serde_json::to_string_pretty(&lock).unwrap();

        // Verify JSON structure
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], 1);
        assert!(parsed["targets"]["qemux86-64"]["sdk"]["curl"].is_string());
        assert!(parsed["targets"]["qemux86-64"]["extensions/app"]["libfoo"].is_string());
        assert!(parsed["targets"]["qemuarm64"]["sdk"]["curl"].is_string());
    }

    #[test]
    fn test_lock_file_persistence_multiple_targets() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();

        // Create lock file with multiple targets and sysroots
        let mut lock = LockFile::new();
        lock.set_locked_version(
            "qemux86-64",
            &SysrootType::Sdk,
            "toolchain",
            "1.0.0-r0.x86_64",
        );
        lock.set_locked_version(
            "qemux86-64",
            &SysrootType::Rootfs,
            "base",
            "1.0.0-r0.core2_64",
        );
        lock.set_locked_version(
            "qemuarm64",
            &SysrootType::Sdk,
            "toolchain",
            "1.0.0-r0.aarch64",
        );
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
            loaded.get_locked_version("qemux86-64", &SysrootType::Sdk, "toolchain"),
            Some(&"1.0.0-r0.x86_64".to_string())
        );
        assert_eq!(
            loaded.get_locked_version("qemux86-64", &SysrootType::Rootfs, "base"),
            Some(&"1.0.0-r0.core2_64".to_string())
        );
        assert_eq!(
            loaded.get_locked_version("qemuarm64", &SysrootType::Sdk, "toolchain"),
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

        // Set a locked version
        lock.set_locked_version(target, &SysrootType::Sdk, "curl", "7.88.1-r0.x86_64");

        // Even if config specifies a different version, locked version should be used
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &SysrootType::Sdk, "curl", "7.80.0"),
            "curl-7.88.1-r0.x86_64"
        );

        // And if config says "*", locked version should still be used
        assert_eq!(
            build_package_spec_with_lock(&lock, target, &SysrootType::Sdk, "curl", "*"),
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

        // Create a lock file with packages in non-alphabetical order
        let mut lock1 = LockFile::new();
        lock1.set_locked_version("qemux86-64", &SysrootType::Sdk, "zebra", "1.0.0-r0");
        lock1.set_locked_version("qemux86-64", &SysrootType::Sdk, "alpha", "2.0.0-r0");
        lock1.set_locked_version("qemux86-64", &SysrootType::Rootfs, "beta", "3.0.0-r0");
        lock1.set_locked_version("qemuarm64", &SysrootType::Sdk, "gamma", "4.0.0-r0");

        lock1.save(src_dir).unwrap();
        let content1 = fs::read_to_string(LockFile::get_path(src_dir)).unwrap();

        // Create another lock file with same data but added in different order
        let mut lock2 = LockFile::new();
        lock2.set_locked_version("qemuarm64", &SysrootType::Sdk, "gamma", "4.0.0-r0");
        lock2.set_locked_version("qemux86-64", &SysrootType::Rootfs, "beta", "3.0.0-r0");
        lock2.set_locked_version("qemux86-64", &SysrootType::Sdk, "alpha", "2.0.0-r0");
        lock2.set_locked_version("qemux86-64", &SysrootType::Sdk, "zebra", "1.0.0-r0");

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

        // Verify package keys are sorted within each sysroot
        let sdk_start = content1.find("\"sdk\"").unwrap();
        let alpha_pos = content1[sdk_start..].find("\"alpha\"").unwrap() + sdk_start;
        let zebra_pos = content1[sdk_start..].find("\"zebra\"").unwrap() + sdk_start;
        assert!(
            alpha_pos < zebra_pos,
            "Package keys should be alphabetically sorted"
        );
    }
}
