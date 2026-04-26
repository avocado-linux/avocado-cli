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
/// Version 3: Extensions now use ExtensionLock with optional source metadata and packages
/// Version 4: Adds per-sysroot `kernel-versions` map for pinned KERNEL_VERSION
/// Version 5: Adds `kernels` map (per-target, content-addressed by KERNEL_VERSION)
///            and `boot` record tracking the active flashed kernel-version /
///            runtime. Both fields are additive — v4 lockfiles read as v5
///            with empty `kernels` and no `boot`.
const LOCKFILE_VERSION: u32 = 5;

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
    /// Initramfs sysroot ($AVOCADO_PREFIX/initramfs)
    Initramfs,
    /// Target sysroot ($AVOCADO_PREFIX/sdk/target-sysroot)
    TargetSysroot,
    /// Extension sysroot ($AVOCADO_EXT_SYSROOTS/{name})
    /// Uses ext-rpm-config-scripts for RPM database
    Extension(String),
    /// Runtime sysroot ($AVOCADO_PREFIX/runtimes/{name})
    Runtime(String),
    /// Kernel sysroot ($AVOCADO_PREFIX/kernel/{KERNEL_VERSION}) — content-addressed.
    /// Holds the kernel `Image` (and DTBs, in future) shared across runtimes
    /// pinning the same KERNEL_VERSION. The bytes are a pure function of the
    /// version, so two runtimes pinning the same kver share storage.
    #[allow(dead_code)] // Constructed by per-runtime install path (Phase 2c+).
    Kernel(String),
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
            SysrootType::Initramfs => RpmQueryConfig {
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some("$AVOCADO_PREFIX/initramfs".to_string()),
            },
            SysrootType::TargetSysroot => RpmQueryConfig {
                // Target-sysroot: same approach as rootfs - unset config and use --root
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some("$AVOCADO_PREFIX/sdk/target-sysroot".to_string()),
            },
            SysrootType::Extension(name) => RpmQueryConfig {
                // Extensions use ext-rpm-config-scripts
                // The database is at standard location, so --root is sufficient
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_EXT_SYSROOTS/{name}")),
            },
            SysrootType::Runtime(name) => RpmQueryConfig {
                // Runtime: same approach as rootfs - unset config and use --root
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_PREFIX/runtimes/{name}")),
            },
            SysrootType::Kernel(version) => RpmQueryConfig {
                // Kernel sysroot: content-addressed under $AVOCADO_PREFIX/kernel/<kver>.
                rpm_etcconfigdir: None,
                rpm_configdir: None,
                root_path: Some(format!("$AVOCADO_PREFIX/kernel/{version}")),
            },
        }
    }

    /// Stable string key used to index into per-sysroot lockfile maps (e.g.
    /// `kernel-versions`). Mirrors the naming convention already used in the
    /// v1 → v3 migration.
    pub fn lock_key(&self) -> String {
        match self {
            SysrootType::Sdk(arch) => format!("sdk/{arch}"),
            SysrootType::Rootfs => "rootfs".to_string(),
            SysrootType::Initramfs => "initramfs".to_string(),
            SysrootType::TargetSysroot => "target-sysroot".to_string(),
            SysrootType::Extension(name) => format!("extensions/{name}"),
            SysrootType::Runtime(name) => format!("runtimes/{name}"),
            SysrootType::Kernel(version) => format!("kernels/{version}"),
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
/// Used for SDK (keyed by host arch) and runtimes (keyed by name)
pub type NestedPackageVersions = HashMap<String, PackageVersions>;

/// Source metadata for a fetched extension in the lock file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionSourceLock {
    /// Source type (e.g., "package")
    #[serde(rename = "type")]
    pub source_type: String,
    /// RPM package name (may differ from extension name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Actual installed version
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Lock data for a single extension — unifies sysroot packages and source metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionLock {
    /// Source metadata (only present for remote/fetched extensions)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ExtensionSourceLock>,
    /// Packages installed in this extension's sysroot
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub packages: PackageVersions,
}

impl ExtensionLock {
    pub fn is_empty(&self) -> bool {
        self.source.is_none() && self.packages.is_empty()
    }
}

/// Helper for serde skip_serializing_if on extensions map
fn extensions_are_empty(extensions: &HashMap<String, ExtensionLock>) -> bool {
    extensions.is_empty() || extensions.values().all(|e| e.is_empty())
}

/// Authoritative record of what's flashed on the device for a given target.
/// Written by `provision`/`deploy`; read by `deploy` to decide between a
/// userspace push (kernel unchanged) and a full OS update (kernel changed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootRecord {
    /// KERNEL_VERSION of the kernel currently flashed on the device.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "kernel-version"
    )]
    pub kernel_version: Option<String>,
    /// Name of the runtime that drove the current boot kernel choice.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "active-runtime"
    )]
    pub active_runtime: Option<String>,
}

impl BootRecord {
    pub fn is_empty(&self) -> bool {
        self.kernel_version.is_none() && self.active_runtime.is_none()
    }
}

fn boot_record_is_empty(b: &BootRecord) -> bool {
    b.is_empty()
}

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

    /// Initramfs packages (shared across all host architectures)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub initramfs: PackageVersions,

    /// Target-sysroot packages (shared across all host architectures)
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        rename = "target-sysroot"
    )]
    pub target_sysroot: PackageVersions,

    /// Extension data keyed by extension name (source metadata + sysroot packages)
    #[serde(default, skip_serializing_if = "extensions_are_empty")]
    pub extensions: HashMap<String, ExtensionLock>,

    /// Runtime packages keyed by runtime name
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub runtimes: NestedPackageVersions,

    /// Resolved KERNEL_VERSION per sysroot, pinned at first install so that
    /// subsequent installs against the same lockfile produce the same kernel
    /// even if the feed gained newer kernels in the meantime. Keys use the
    /// same naming convention as the v1→v3 migration: `"rootfs"`,
    /// `"initramfs"`, `"runtimes/<name>"`, `"extensions/<name>"`,
    /// `"target-sysroot"`.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        rename = "kernel-versions"
    )]
    pub kernel_versions: HashMap<String, String>,

    /// Per-kernel-sysroot package state. Keyed by KERNEL_VERSION; each entry
    /// records the packages installed in `$AVOCADO_PREFIX/kernel/<kver>/`.
    /// Content-addressed by version — multiple runtimes pinning the same
    /// kver share a single sysroot and a single entry here.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub kernels: NestedPackageVersions,

    /// Active boot record: which kernel is flashed and which runtime drove
    /// that choice. Populated by `provision`/`deploy`.
    #[serde(default, skip_serializing_if = "boot_record_is_empty")]
    pub boot: BootRecord,
}

/// Lock file structure for tracking installed package versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Lock file format version
    pub version: u32,
    /// Distro release (feed year) at lock time — compatibility guard for feed year changes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro_release: Option<String>,
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
            distro_release: None,
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
    /// - Version 1 -> 5: SDK packages nested under arch key, extensions use ExtensionLock,
    ///   plus v3 → v4 (kernel-versions added) and v4 → v5 (kernels map + boot record).
    /// - Version 2 -> 5: Extensions migrated from flat packages to ExtensionLock structure,
    ///   plus v3 → v4 → v5.
    /// - Version 3 -> 5: Adds empty kernel-versions map (v3→v4) and empty kernels/boot
    ///   (v4→v5); schema is otherwise compatible.
    /// - Version 4 -> 5: Adds empty kernels map and empty boot record. Purely additive.
    pub fn load(src_dir: &Path) -> Result<Self> {
        let path = Self::get_path(src_dir);

        if !path.exists() {
            return Ok(Self::new());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read lock file: {}", path.display()))?;

        // First, try to parse as current (v5) format. The new fields (`kernels`,
        // `boot`) use `#[serde(default)]`, so v3 and v4 files parse successfully
        // here and differ only in the `version` field (which we bump below).
        if let Ok(mut lock_file) = serde_json::from_str::<LockFile>(&content) {
            if lock_file.version > LOCKFILE_VERSION {
                anyhow::bail!(
                    "Lock file version {} is newer than supported version {}. Please upgrade avocado-cli.",
                    lock_file.version,
                    LOCKFILE_VERSION
                );
            }
            if lock_file.version == LOCKFILE_VERSION {
                return Ok(lock_file);
            }
            if lock_file.version == 3 || lock_file.version == 4 {
                // v3 → v4 → v5: purely additive; new fields already default-empty via serde.
                lock_file.version = LOCKFILE_VERSION;
                return Ok(lock_file);
            }
        }

        // Try to parse as older format and migrate
        let old_lock: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse lock file: {}", path.display()))?;

        let version = old_lock
            .get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

        match version {
            1 => Ok(Self::migrate_v1_to_v3(&old_lock)),
            2 => Ok(Self::migrate_v2_to_v3(&old_lock)),
            _ => anyhow::bail!("Unable to parse lock file format"),
        }
    }

    /// Migrate lock file from version 1 to version 3
    ///
    /// Version 1 stored:
    /// - SDK packages under flat "sdk" key
    /// - Extensions under "extensions/{name}"
    /// - Runtimes under "runtimes/{name}"
    ///
    /// Since we can't know what architecture the v1 SDK packages were installed for,
    /// we discard them. Users will need to re-run `avocado sdk install`.
    fn migrate_v1_to_v3(v1_lock: &serde_json::Value) -> LockFile {
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
                                        target_locks.extensions.insert(
                                            name.to_string(),
                                            ExtensionLock {
                                                source: None,
                                                packages: pkg_versions,
                                            },
                                        );
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

    /// Migrate lock file from version 2 to version 3
    ///
    /// Version 2 stored extensions as flat package maps: extensions -> {name} -> {pkg: ver}
    /// Version 3 wraps them in ExtensionLock: extensions -> {name} -> {packages: {pkg: ver}}
    ///
    /// Also migrates any "fetched-extensions" entries into the unified extensions structure
    /// with source metadata.
    fn migrate_v2_to_v3(v2_lock: &serde_json::Value) -> LockFile {
        let mut lock_file = LockFile::new();

        if let Some(targets) = v2_lock.get("targets").and_then(|t| t.as_object()) {
            for (target_name, target_data) in targets {
                let target_locks = lock_file.targets.entry(target_name.clone()).or_default();

                if let Some(target_map) = target_data.as_object() {
                    // Migrate SDK (already nested by arch in v2)
                    if let Some(sdk) = target_map.get("sdk").and_then(|v| v.as_object()) {
                        for (arch, packages) in sdk {
                            if let Some(pkg_map) = packages.as_object() {
                                let pkg_versions: PackageVersions = pkg_map
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.as_str().map(|s| (k.clone(), s.to_string()))
                                    })
                                    .collect();
                                target_locks.sdk.insert(arch.clone(), pkg_versions);
                            }
                        }
                    }

                    // Migrate rootfs
                    if let Some(rootfs) = target_map.get("rootfs").and_then(|v| v.as_object()) {
                        target_locks.rootfs = rootfs
                            .iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect();
                    }

                    // Migrate target-sysroot
                    if let Some(ts) = target_map.get("target-sysroot").and_then(|v| v.as_object()) {
                        target_locks.target_sysroot = ts
                            .iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect();
                    }

                    // Migrate extensions: v2 has flat {name: {pkg: ver}}
                    if let Some(exts) = target_map.get("extensions").and_then(|v| v.as_object()) {
                        for (name, packages) in exts {
                            if let Some(pkg_map) = packages.as_object() {
                                let pkg_versions: PackageVersions = pkg_map
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.as_str().map(|s| (k.clone(), s.to_string()))
                                    })
                                    .collect();
                                let ext_lock =
                                    target_locks.extensions.entry(name.clone()).or_default();
                                ext_lock.packages = pkg_versions;
                            }
                        }
                    }

                    // Migrate fetched-extensions into unified extensions with source metadata
                    if let Some(fetched) = target_map
                        .get("fetched-extensions")
                        .and_then(|v| v.as_object())
                    {
                        for (name, packages) in fetched {
                            if let Some(pkg_map) = packages.as_object() {
                                // In v2, fetched-extensions stored {ext_name: {pkg_name: version}}
                                // The package name and version represent the fetched RPM
                                let ext_lock =
                                    target_locks.extensions.entry(name.clone()).or_default();
                                // Convert the single package entry to source metadata
                                if let Some((pkg_name, version)) = pkg_map.iter().next() {
                                    if let Some(ver_str) = version.as_str() {
                                        ext_lock.source = Some(ExtensionSourceLock {
                                            source_type: "package".to_string(),
                                            package: Some(pkg_name.clone()),
                                            version: Some(ver_str.to_string()),
                                        });
                                    }
                                }
                            }
                        }
                    }

                    // Migrate runtimes (already nested by name in v2)
                    if let Some(runtimes) = target_map.get("runtimes").and_then(|v| v.as_object()) {
                        for (name, packages) in runtimes {
                            if let Some(pkg_map) = packages.as_object() {
                                let pkg_versions: PackageVersions = pkg_map
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.as_str().map(|s| (k.clone(), s.to_string()))
                                    })
                                    .collect();
                                target_locks.runtimes.insert(name.clone(), pkg_versions);
                            }
                        }
                    }
                }
            }
        }

        lock_file
    }

    /// Save lock file to disk as pretty-printed JSON with deterministically
    /// sorted keys. Routing through `serde_json::Value` lands every map in
    /// `serde_json::Map` (a `BTreeMap` without the `preserve_order` feature),
    /// so key order is stable regardless of the `HashMap` insertion order in
    /// the in-memory struct.
    pub fn save(&self, src_dir: &Path) -> Result<()> {
        let path = Self::get_path(src_dir);

        // Ensure the .avocado directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create lock file directory: {}", parent.display())
            })?;
        }

        let value = serde_json::to_value(self).with_context(|| "Failed to serialize lock file")?;
        let content = serde_json::to_string_pretty(&value)
            .with_context(|| "Failed to serialize lock file")?;

        // Add a newline at the end for better git diffs
        let content_with_newline = format!("{content}\n");

        fs::write(&path, content_with_newline)
            .with_context(|| format!("Failed to write lock file: {}", path.display()))?;

        Ok(())
    }

    /// Check if the lock file's distro release matches the config's.
    /// Warns if there's a mismatch (e.g., feed year changed from 2024 to 2026).
    pub fn check_distro_release_compat(&self, config_release: Option<&str>) {
        if let (Some(locked_release), Some(current_release)) =
            (&self.distro_release, config_release)
        {
            if locked_release != current_release {
                eprintln!(
                    "[WARNING] Lock file was created with distro.release '{locked_release}' but config has '{current_release}'. \
                     This may indicate an incompatible feed year change. \
                     Run 'avocado unlock' and reinstall to update."
                );
            }
        }
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
            SysrootType::Initramfs => target_locks.initramfs.get(package),
            SysrootType::TargetSysroot => target_locks.target_sysroot.get(package),
            SysrootType::Extension(name) => target_locks
                .extensions
                .get(name)
                .and_then(|ext| ext.packages.get(package)),
            SysrootType::Runtime(name) => target_locks
                .runtimes
                .get(name)
                .and_then(|pkgs| pkgs.get(package)),
            SysrootType::Kernel(version) => target_locks
                .kernels
                .get(version)
                .and_then(|pkgs| pkgs.get(package)),
        }
    }

    /// Get the pinned KERNEL_VERSION for a specific target and sysroot, if any.
    pub fn get_kernel_version(&self, target: &str, sysroot: &SysrootType) -> Option<&String> {
        self.targets
            .get(target)?
            .kernel_versions
            .get(&sysroot.lock_key())
    }

    /// Pin the resolved KERNEL_VERSION for a specific target and sysroot.
    pub fn set_kernel_version(&mut self, target: &str, sysroot: &SysrootType, kver: &str) {
        self.targets
            .entry(target.to_string())
            .or_default()
            .kernel_versions
            .insert(sysroot.lock_key(), kver.to_string());
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
            SysrootType::Initramfs => &mut target_locks.initramfs,
            SysrootType::TargetSysroot => &mut target_locks.target_sysroot,
            SysrootType::Extension(name) => {
                &mut target_locks
                    .extensions
                    .entry(name.clone())
                    .or_default()
                    .packages
            }
            SysrootType::Runtime(name) => target_locks.runtimes.entry(name.clone()).or_default(),
            SysrootType::Kernel(version) => {
                target_locks.kernels.entry(version.clone()).or_default()
            }
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
            SysrootType::Initramfs => &mut target_locks.initramfs,
            SysrootType::TargetSysroot => &mut target_locks.target_sysroot,
            SysrootType::Extension(name) => {
                &mut target_locks
                    .extensions
                    .entry(name.clone())
                    .or_default()
                    .packages
            }
            SysrootType::Runtime(name) => target_locks.runtimes.entry(name.clone()).or_default(),
            SysrootType::Kernel(version) => {
                target_locks.kernels.entry(version.clone()).or_default()
            }
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
            SysrootType::Initramfs => Some(&target_locks.initramfs),
            SysrootType::TargetSysroot => Some(&target_locks.target_sysroot),
            SysrootType::Extension(name) => {
                target_locks.extensions.get(name).map(|ext| &ext.packages)
            }
            SysrootType::Runtime(name) => target_locks.runtimes.get(name),
            SysrootType::Kernel(version) => target_locks.kernels.get(version),
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
                    && target_locks.initramfs.is_empty()
                    && target_locks.target_sysroot.is_empty()
                    && extensions_are_empty(&target_locks.extensions)
                    && target_locks.runtimes.is_empty()
                    && target_locks.kernels.is_empty()
                    && target_locks.boot.is_empty()
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

    /// Clear initramfs entries for a specific target
    pub fn clear_initramfs(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.initramfs.clear();
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

    /// Get the set of package names previously locked for a target and sysroot.
    /// Returns an empty set if no packages are recorded.
    pub fn get_locked_package_names(
        &self,
        target: &str,
        sysroot: &SysrootType,
    ) -> std::collections::HashSet<String> {
        self.get_sysroot_versions(target, sysroot)
            .map(|versions| versions.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove specific package entries from a sysroot's lock data.
    /// Used during sync to remove only the packages that were removed from the config
    /// while preserving version pinning for packages that remain.
    pub fn remove_packages_from_sysroot(
        &mut self,
        target: &str,
        sysroot: &SysrootType,
        packages_to_remove: &[String],
    ) {
        let Some(target_locks) = self.targets.get_mut(target) else {
            return;
        };

        let pkg_map = match sysroot {
            SysrootType::Sdk(arch) => target_locks.sdk.get_mut(arch),
            SysrootType::Rootfs => Some(&mut target_locks.rootfs),
            SysrootType::Initramfs => Some(&mut target_locks.initramfs),
            SysrootType::TargetSysroot => Some(&mut target_locks.target_sysroot),
            SysrootType::Extension(name) => target_locks
                .extensions
                .get_mut(name)
                .map(|ext| &mut ext.packages),
            SysrootType::Runtime(name) => target_locks.runtimes.get_mut(name),
            SysrootType::Kernel(version) => target_locks.kernels.get_mut(version),
        };

        if let Some(packages) = pkg_map {
            for pkg in packages_to_remove {
                packages.remove(pkg);
            }
        }
    }

    /// Get the source metadata for a fetched extension
    pub fn get_extension_source(
        &self,
        target: &str,
        ext_name: &str,
    ) -> Option<&ExtensionSourceLock> {
        self.targets
            .get(target)
            .and_then(|tl| tl.extensions.get(ext_name))
            .and_then(|ext| ext.source.as_ref())
    }

    /// Set the source metadata for a fetched extension
    pub fn set_extension_source(
        &mut self,
        target: &str,
        ext_name: &str,
        source: ExtensionSourceLock,
    ) {
        let target_locks = self.targets.entry(target.to_string()).or_default();
        let ext_lock = target_locks
            .extensions
            .entry(ext_name.to_string())
            .or_default();
        ext_lock.source = Some(source);
    }

    /// Clear the source metadata for a fetched extension (preserves sysroot packages)
    #[allow(dead_code)]
    pub fn clear_extension_source(&mut self, target: &str, ext_name: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            if let Some(ext_lock) = target_locks.extensions.get_mut(ext_name) {
                ext_lock.source = None;
            }
        }
    }

    /// Clear all entries for a target (SDK, rootfs, initramfs, target-sysroot, extensions, runtimes)
    pub fn clear_all(&mut self, target: &str) {
        if let Some(target_locks) = self.targets.get_mut(target) {
            target_locks.sdk.clear();
            target_locks.rootfs.clear();
            target_locks.initramfs.clear();
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
/// Returns a map of package name -> full version string (including architecture suffix).
pub fn parse_rpm_query_output(output: &str) -> HashMap<String, String> {
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

            result.insert(name.to_string(), version.to_string());
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

        let result = parse_rpm_query_output(output);
        assert_eq!(result.len(), 3);
        assert_eq!(result.get("curl"), Some(&"7.88.1-r0.core2_64".to_string()));
        assert_eq!(
            result.get("openssl"),
            Some(&"3.0.8-r0.core2_64".to_string())
        );
        assert_eq!(result.get("wget"), Some(&"1.21-r0.core2_64".to_string()));
        assert_eq!(result.get("package-xyz"), None);
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
        let result = parse_rpm_query_output("");
        assert!(result.is_empty());

        // Whitespace only
        let result = parse_rpm_query_output("   \n  \n  ");
        assert!(result.is_empty());

        // Only "not installed" messages
        let result =
            parse_rpm_query_output("package-a is not installed\npackage-b is not installed");
        assert!(result.is_empty());

        // Mixed valid and invalid
        let output =
            "valid-pkg 1.0.0-r0.x86_64\nbad-pkg is not installed\nanother-pkg 2.0.0-r0.x86_64";
        let result = parse_rpm_query_output(output);
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

        let result = parse_rpm_query_output(output);
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
    fn test_parse_rpm_query_output_preserves_full_version_for_sdk() {
        // SDK package versions should be preserved in full (including architecture suffix)
        // since SDK packages are already nested per host architecture in the lock file
        let output = r#"nativesdk-curl 7.88.1-r0.x86_64_avocadosdk
nativesdk-openssl 3.0.8-r0.x86_64_avocadosdk
avocado-sdk-toolchain 0.1.0-r0.x86_64_avocadosdk
"#;

        let result = parse_rpm_query_output(output);
        assert_eq!(result.len(), 3);
        assert_eq!(
            result.get("nativesdk-curl"),
            Some(&"7.88.1-r0.x86_64_avocadosdk".to_string())
        );
        assert_eq!(
            result.get("nativesdk-openssl"),
            Some(&"3.0.8-r0.x86_64_avocadosdk".to_string())
        );
        assert_eq!(
            result.get("avocado-sdk-toolchain"),
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

        // Test Extension config - extensions don't need RPM_CONFIGDIR
        let ext_config = SysrootType::Extension("my-ext".to_string()).get_rpm_query_config();
        assert!(ext_config.rpm_etcconfigdir.is_none());
        assert!(ext_config.rpm_configdir.is_none());
        assert_eq!(
            ext_config.root_path,
            Some("$AVOCADO_EXT_SYSROOTS/my-ext".to_string())
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

        // Verify JSON structure - SDK is nested under arch, extensions use ExtensionLock
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], LOCKFILE_VERSION);
        // SDK packages nested under sdk -> {arch} -> {package}
        assert!(parsed["targets"]["qemux86-64"]["sdk"]["x86_64"]["curl"].is_string());
        // Extensions nested under extensions -> {name} -> packages -> {package}
        assert!(
            parsed["targets"]["qemux86-64"]["extensions"]["app"]["packages"]["libfoo"].is_string()
        );
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
    fn test_save_is_pretty_and_deterministic() {
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

        assert_eq!(
            content1, content2,
            "save() should produce identical output regardless of insertion order"
        );

        // Pretty-printed output spans multiple lines with indentation so PR
        // diffs stay readable.
        assert!(
            content1.contains("\n  "),
            "lock file should be pretty-printed with indentation, got:\n{content1}"
        );
        assert!(
            content1.lines().count() > 5,
            "lock file should span multiple lines, got:\n{content1}"
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
    fn test_migrate_v1_to_v3() {
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

        // Version should be updated to the current version
        assert_eq!(lock.version, LOCKFILE_VERSION);

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

        // Extensions should be migrated from "extensions/name" to ExtensionLock
        assert_eq!(
            lock.get_locked_version(
                "jetson-orin-nano-devkit",
                &SysrootType::Extension("my-app".to_string()),
                "libfoo"
            ),
            Some(&"1.0.0-r0".to_string())
        );
        // No source metadata for v1 extensions
        assert!(lock
            .get_extension_source("jetson-orin-nano-devkit", "my-app")
            .is_none());

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

    #[test]
    fn test_migrate_v2_to_v3() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path();

        // Create a v2 lock file with both extensions and fetched-extensions
        let v2_content = r#"{"targets":{"qemux86-64":{"extensions":{"my-app":{"curl":"8.0.0-r0","libfoo":"1.0.0-r0"}},"fetched-extensions":{"remote-ext":{"avocado-ext-remote":"1.0.0-1.el9.x86_64"}},"rootfs":{"base":"1.0.0-r0"},"sdk":{"x86_64":{"toolchain":"1.0.0-r0"}}}},"version":2}
"#;

        let lock_path = LockFile::get_path(src_dir);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, v2_content).unwrap();

        // Load the lock file - should trigger v2 migration (to current version)
        let lock = LockFile::load(src_dir).unwrap();

        // Version should be updated to the current version
        assert_eq!(lock.version, LOCKFILE_VERSION);

        // SDK should be preserved
        assert_eq!(
            lock.get_locked_version(
                "qemux86-64",
                &SysrootType::Sdk("x86_64".to_string()),
                "toolchain"
            ),
            Some(&"1.0.0-r0".to_string())
        );

        // Rootfs preserved
        assert_eq!(
            lock.get_locked_version("qemux86-64", &SysrootType::Rootfs, "base"),
            Some(&"1.0.0-r0".to_string())
        );

        // Regular extensions should be migrated with packages
        assert_eq!(
            lock.get_locked_version(
                "qemux86-64",
                &SysrootType::Extension("my-app".to_string()),
                "curl"
            ),
            Some(&"8.0.0-r0".to_string())
        );
        assert_eq!(
            lock.get_locked_version(
                "qemux86-64",
                &SysrootType::Extension("my-app".to_string()),
                "libfoo"
            ),
            Some(&"1.0.0-r0".to_string())
        );

        // Fetched extension should have source metadata
        let source = lock
            .get_extension_source("qemux86-64", "remote-ext")
            .expect("should have source metadata");
        assert_eq!(source.source_type, "package");
        assert_eq!(source.package.as_deref(), Some("avocado-ext-remote"));
        assert_eq!(source.version.as_deref(), Some("1.0.0-1.el9.x86_64"));
    }

    #[test]
    fn test_extension_source_get_set() {
        let mut lock = LockFile::new();
        let target = "qemux86-64";
        let ext_name = "remote-ext";

        // Initially no source
        assert!(lock.get_extension_source(target, ext_name).is_none());

        // Set source
        lock.set_extension_source(
            target,
            ext_name,
            ExtensionSourceLock {
                source_type: "package".to_string(),
                package: Some("avocado-ext-remote".to_string()),
                version: Some("1.0.0-1.el9.x86_64".to_string()),
            },
        );

        let source = lock.get_extension_source(target, ext_name).unwrap();
        assert_eq!(source.source_type, "package");
        assert_eq!(source.package.as_deref(), Some("avocado-ext-remote"));
        assert_eq!(source.version.as_deref(), Some("1.0.0-1.el9.x86_64"));

        // Clear source (preserves packages)
        lock.set_locked_version(
            target,
            &SysrootType::Extension(ext_name.to_string()),
            "some-pkg",
            "1.0.0",
        );
        lock.clear_extension_source(target, ext_name);
        assert!(lock.get_extension_source(target, ext_name).is_none());
        // Package should still be there
        assert_eq!(
            lock.get_locked_version(
                target,
                &SysrootType::Extension(ext_name.to_string()),
                "some-pkg"
            ),
            Some(&"1.0.0".to_string())
        );

        // clear_extension removes everything (source + packages)
        lock.set_extension_source(
            target,
            ext_name,
            ExtensionSourceLock {
                source_type: "package".to_string(),
                package: None,
                version: None,
            },
        );
        lock.clear_extension(target, ext_name);
        assert!(lock.get_extension_source(target, ext_name).is_none());
        assert!(lock
            .get_locked_version(
                target,
                &SysrootType::Extension(ext_name.to_string()),
                "some-pkg"
            )
            .is_none());
    }

    #[test]
    fn test_get_locked_package_names_empty() {
        let lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());
        let names = lock.get_locked_package_names("qemux86-64", &sysroot);
        assert!(names.is_empty());
    }

    #[test]
    fn test_get_locked_package_names_returns_keys() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());
        let mut versions = HashMap::new();
        versions.insert("curl".to_string(), "8.0.0-r0".to_string());
        versions.insert("iperf3".to_string(), "3.14-r0".to_string());
        lock.update_sysroot_versions("qemux86-64", &sysroot, versions);

        let names = lock.get_locked_package_names("qemux86-64", &sysroot);
        assert_eq!(names.len(), 2);
        assert!(names.contains("curl"));
        assert!(names.contains("iperf3"));
    }

    #[test]
    fn test_get_locked_package_names_different_sysroot_isolated() {
        let mut lock = LockFile::new();
        let ext_a = SysrootType::Extension("ext-a".to_string());
        let ext_b = SysrootType::Extension("ext-b".to_string());

        let mut versions_a = HashMap::new();
        versions_a.insert("curl".to_string(), "8.0.0".to_string());
        lock.update_sysroot_versions("qemux86-64", &ext_a, versions_a);

        let mut versions_b = HashMap::new();
        versions_b.insert("nginx".to_string(), "1.0.0".to_string());
        lock.update_sysroot_versions("qemux86-64", &ext_b, versions_b);

        let names_a = lock.get_locked_package_names("qemux86-64", &ext_a);
        assert_eq!(names_a.len(), 1);
        assert!(names_a.contains("curl"));
        assert!(!names_a.contains("nginx"));
    }

    #[test]
    fn test_remove_packages_from_sysroot_selective() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());
        let mut versions = HashMap::new();
        versions.insert("curl".to_string(), "8.0.0-r0".to_string());
        versions.insert("iperf3".to_string(), "3.14-r0".to_string());
        versions.insert("wget".to_string(), "1.21-r0".to_string());
        lock.update_sysroot_versions("qemux86-64", &sysroot, versions);

        // Remove only iperf3 -- curl and wget should remain with their versions
        lock.remove_packages_from_sysroot("qemux86-64", &sysroot, &["iperf3".to_string()]);

        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "curl"),
            Some(&"8.0.0-r0".to_string())
        );
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "iperf3"),
            None
        );
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "wget"),
            Some(&"1.21-r0".to_string())
        );
    }

    #[test]
    fn test_remove_packages_from_sysroot_nonexistent_target() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());

        // Should not panic when target doesn't exist
        lock.remove_packages_from_sysroot("nonexistent", &sysroot, &["curl".to_string()]);
    }

    #[test]
    fn test_remove_packages_from_runtime_sysroot() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Runtime("dev".to_string());
        let mut versions = HashMap::new();
        versions.insert("avocado-runtime".to_string(), "0.1.0".to_string());
        versions.insert("kernel-tools".to_string(), "6.1.0".to_string());
        lock.update_sysroot_versions("qemux86-64", &sysroot, versions);

        lock.remove_packages_from_sysroot("qemux86-64", &sysroot, &["kernel-tools".to_string()]);

        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "avocado-runtime"),
            Some(&"0.1.0".to_string())
        );
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "kernel-tools"),
            None
        );
    }

    #[test]
    fn test_removal_detection_scenario() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());

        // Simulate initial install: curl + iperf3
        let mut initial_versions = HashMap::new();
        initial_versions.insert("curl".to_string(), "8.0.0-r0".to_string());
        initial_versions.insert("iperf3".to_string(), "3.14-r0".to_string());
        lock.update_sysroot_versions("qemux86-64", &sysroot, initial_versions);

        // Simulate user removing iperf3 from config
        let current_config_packages: std::collections::HashSet<String> =
            ["curl".to_string()].into_iter().collect();
        let locked_names = lock.get_locked_package_names("qemux86-64", &sysroot);

        let removed: Vec<String> = locked_names
            .difference(&current_config_packages)
            .cloned()
            .collect();

        assert_eq!(removed, vec!["iperf3".to_string()]);

        // Remove stale entries preserving curl's version pin
        lock.remove_packages_from_sysroot("qemux86-64", &sysroot, &removed);

        // curl version is preserved
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "curl"),
            Some(&"8.0.0-r0".to_string())
        );
        // iperf3 is gone
        assert_eq!(
            lock.get_locked_version("qemux86-64", &sysroot, "iperf3"),
            None
        );
    }

    #[test]
    fn test_no_removal_when_packages_only_added() {
        let mut lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());

        // Initial: just curl
        let mut initial = HashMap::new();
        initial.insert("curl".to_string(), "8.0.0-r0".to_string());
        lock.update_sysroot_versions("qemux86-64", &sysroot, initial);

        // Config now has curl + iperf3 (addition only)
        let config_packages: std::collections::HashSet<String> =
            ["curl".to_string(), "iperf3".to_string()]
                .into_iter()
                .collect();
        let locked_names = lock.get_locked_package_names("qemux86-64", &sysroot);

        let removed: Vec<String> = locked_names.difference(&config_packages).cloned().collect();

        assert!(removed.is_empty());
    }

    #[test]
    fn test_no_removal_when_lock_is_empty() {
        let lock = LockFile::new();
        let sysroot = SysrootType::Extension("app".to_string());

        let config_packages: std::collections::HashSet<String> =
            ["curl".to_string()].into_iter().collect();
        let locked_names = lock.get_locked_package_names("qemux86-64", &sysroot);

        let removed: Vec<String> = locked_names.difference(&config_packages).cloned().collect();

        assert!(removed.is_empty());
    }

    // --- v5 schema additions: kernel sysroot + boot record ---

    #[test]
    fn test_kernel_sysroot_lock_key_and_path() {
        let s = SysrootType::Kernel("6.6.123-yocto-standard".to_string());
        assert_eq!(s.lock_key(), "kernels/6.6.123-yocto-standard");
        let cfg = s.get_rpm_query_config();
        assert_eq!(
            cfg.root_path.as_deref(),
            Some("$AVOCADO_PREFIX/kernel/6.6.123-yocto-standard")
        );
    }

    #[test]
    fn test_kernel_sysroot_packages_round_trip() {
        let mut lock = LockFile::new();
        let s = SysrootType::Kernel("6.6.123-yocto-standard".to_string());
        let mut versions = HashMap::new();
        versions.insert("kernel-image".to_string(), "6.6.123-r0".to_string());
        versions.insert("kernel-image-image".to_string(), "6.6.123-r0".to_string());
        lock.update_sysroot_versions("icam-540", &s, versions);

        // Read back
        let versions = lock.get_sysroot_versions("icam-540", &s).unwrap();
        assert_eq!(
            versions.get("kernel-image").map(String::as_str),
            Some("6.6.123-r0")
        );
        assert_eq!(
            lock.get_locked_version("icam-540", &s, "kernel-image"),
            Some(&"6.6.123-r0".to_string())
        );
        // Distinct from rootfs / extensions / runtimes — content-addressed bucket
        let target = lock.targets.get("icam-540").unwrap();
        assert_eq!(target.kernels.len(), 1);
        assert!(target.rootfs.is_empty());
    }

    #[test]
    fn test_boot_record_round_trip_through_save_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut lock = LockFile::new();
        let target_locks = lock.targets.entry("icam-540".to_string()).or_default();
        target_locks.boot.kernel_version = Some("6.6.123-yocto-standard".to_string());
        target_locks.boot.active_runtime = Some("prod".to_string());
        lock.save(temp_dir.path()).unwrap();

        let loaded = LockFile::load(temp_dir.path()).unwrap();
        let boot = &loaded.targets.get("icam-540").unwrap().boot;
        assert_eq!(
            boot.kernel_version.as_deref(),
            Some("6.6.123-yocto-standard")
        );
        assert_eq!(boot.active_runtime.as_deref(), Some("prod"));
        assert_eq!(loaded.version, LOCKFILE_VERSION);
    }

    #[test]
    fn test_v4_lockfile_loads_as_v5_with_empty_kernels_and_boot() {
        // v4 lockfile (current production format pre-v5): version=4 + kernel-versions,
        // no `kernels` or `boot` fields.
        let v4_json = r#"{
            "version": 4,
            "targets": {
                "icam-540": {
                    "rootfs": { "avocado-pkg-rootfs": "1.0.0-r0" },
                    "kernel-versions": { "rootfs": "6.6.123-yocto-standard" }
                }
            }
        }"#;
        let temp_dir = tempfile::tempdir().unwrap();
        let lock_dir = temp_dir.path().join(LOCKFILE_DIR);
        fs::create_dir_all(&lock_dir).unwrap();
        fs::write(lock_dir.join(LOCKFILE_NAME), v4_json).unwrap();

        let loaded = LockFile::load(temp_dir.path()).unwrap();
        assert_eq!(loaded.version, LOCKFILE_VERSION); // bumped to v5
        let target = loaded.targets.get("icam-540").unwrap();
        // Existing v4 state preserved.
        assert_eq!(
            target.rootfs.get("avocado-pkg-rootfs").map(String::as_str),
            Some("1.0.0-r0")
        );
        assert_eq!(
            target.kernel_versions.get("rootfs").map(String::as_str),
            Some("6.6.123-yocto-standard")
        );
        // New v5 fields default to empty.
        assert!(target.kernels.is_empty());
        assert!(target.boot.is_empty());
    }
}
