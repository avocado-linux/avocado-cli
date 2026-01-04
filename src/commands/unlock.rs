//! Unlock command implementation for removing lock file entries.

use anyhow::{Context, Result};
use std::path::Path;

use crate::utils::config::Config;
use crate::utils::lockfile::LockFile;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

/// Command to unlock (remove lock entries for) sysroots.
///
/// This command removes entries from the lock file, allowing packages to be
/// updated to newer versions on the next install.
pub struct UnlockCommand {
    /// Path to configuration file
    config_path: String,
    /// Enable verbose output
    verbose: bool,
    /// Target architecture
    target: Option<String>,
    /// Unlock specific extension
    extension: Option<String>,
    /// Unlock specific runtime
    runtime: Option<String>,
    /// Unlock SDK (includes rootfs, target-sysroot, and all SDK arches)
    sdk: bool,
}

impl UnlockCommand {
    /// Create a new UnlockCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        target: Option<String>,
        extension: Option<String>,
        runtime: Option<String>,
        sdk: bool,
    ) -> Self {
        Self {
            config_path,
            verbose,
            target,
            extension,
            runtime,
            sdk,
        }
    }

    /// Execute the unlock command
    pub fn execute(&self) -> Result<()> {
        // Load configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Resolve target
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Get src_dir from config
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                Path::new(&self.config_path)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf()
            });

        // Load lock file
        let mut lock_file = LockFile::load(&src_dir)
            .with_context(|| format!("Failed to load lock file from {}", src_dir.display()))?;

        if lock_file.is_empty() {
            print_info(
                "Lock file is empty, nothing to unlock.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Determine what to unlock
        let unlock_all = !self.sdk && self.extension.is_none() && self.runtime.is_none();

        let mut unlocked_something = false;

        if unlock_all {
            // Unlock everything for the target
            if self.verbose {
                print_info(
                    &format!("Unlocking all entries for target '{}'", target),
                    OutputLevel::Normal,
                );
            }
            lock_file.clear_all(&target);
            unlocked_something = true;
            print_success(
                &format!("Unlocked all entries for target '{}'.", target),
                OutputLevel::Normal,
            );
        } else {
            // Unlock SDK if requested
            if self.sdk {
                if self.verbose {
                    print_info(
                        &format!(
                            "Unlocking SDK, rootfs, and target-sysroot for target '{}'",
                            target
                        ),
                        OutputLevel::Normal,
                    );
                }
                lock_file.clear_sdk(&target);
                lock_file.clear_rootfs(&target);
                lock_file.clear_target_sysroot(&target);
                unlocked_something = true;
                print_success(
                    &format!(
                        "Unlocked SDK, rootfs, and target-sysroot for target '{}'.",
                        target
                    ),
                    OutputLevel::Normal,
                );
            }

            // Unlock extension if specified
            if let Some(ref ext_name) = self.extension {
                if self.verbose {
                    print_info(
                        &format!("Unlocking extension '{}' for target '{}'", ext_name, target),
                        OutputLevel::Normal,
                    );
                }
                lock_file.clear_extension(&target, ext_name);
                unlocked_something = true;
                print_success(
                    &format!("Unlocked extension '{}' for target '{}'.", ext_name, target),
                    OutputLevel::Normal,
                );
            }

            // Unlock runtime if specified
            if let Some(ref runtime_name) = self.runtime {
                if self.verbose {
                    print_info(
                        &format!(
                            "Unlocking runtime '{}' for target '{}'",
                            runtime_name, target
                        ),
                        OutputLevel::Normal,
                    );
                }
                lock_file.clear_runtime(&target, runtime_name);
                unlocked_something = true;
                print_success(
                    &format!(
                        "Unlocked runtime '{}' for target '{}'.",
                        runtime_name, target
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        if unlocked_something {
            // Save updated lock file
            lock_file
                .save(&src_dir)
                .with_context(|| "Failed to save lock file")?;

            if self.verbose {
                print_info("Lock file updated.", OutputLevel::Normal);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::lockfile::SysrootType;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config(temp_dir: &TempDir) -> String {
        let config_content = r#"
default_target: "qemux86-64"
sdk:
  image: "test-image"
ext:
  my-app:
    version: "1.0.0"
runtime:
  dev:
    target: "qemux86-64"
"#;
        let config_path = temp_dir.path().join("avocado.yaml");
        fs::write(&config_path, config_content).unwrap();
        config_path.to_string_lossy().to_string()
    }

    fn create_test_lock_file(temp_dir: &TempDir) {
        let mut lock = LockFile::new();
        let target = "qemux86-64";

        // Add some test entries
        lock.set_locked_version(
            target,
            &SysrootType::Sdk("x86_64".to_string()),
            "test-sdk-pkg",
            "1.0.0-r0",
        );
        lock.set_locked_version(target, &SysrootType::Rootfs, "test-rootfs-pkg", "1.0.0-r0");
        lock.set_locked_version(
            target,
            &SysrootType::TargetSysroot,
            "test-sysroot-pkg",
            "1.0.0-r0",
        );
        lock.set_locked_version(
            target,
            &SysrootType::Extension("my-app".to_string()),
            "test-ext-pkg",
            "1.0.0-r0",
        );
        lock.set_locked_version(
            target,
            &SysrootType::Runtime("dev".to_string()),
            "test-runtime-pkg",
            "1.0.0-r0",
        );

        lock.save(temp_dir.path()).unwrap();
    }

    #[test]
    fn test_unlock_all() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = create_test_config(&temp_dir);
        create_test_lock_file(&temp_dir);

        let cmd = UnlockCommand::new(config_path, false, None, None, None, false);
        let result = cmd.execute();
        assert!(result.is_ok());

        // Verify lock file is now empty for target
        let lock = LockFile::load(temp_dir.path()).unwrap();
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Sdk("x86_64".to_string()),
                "test-sdk-pkg"
            )
            .is_none());
    }

    #[test]
    fn test_unlock_sdk() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = create_test_config(&temp_dir);
        create_test_lock_file(&temp_dir);

        let cmd = UnlockCommand::new(config_path, false, None, None, None, true);
        let result = cmd.execute();
        assert!(result.is_ok());

        // Verify SDK, rootfs, and target-sysroot are cleared but extensions/runtimes remain
        let lock = LockFile::load(temp_dir.path()).unwrap();
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Sdk("x86_64".to_string()),
                "test-sdk-pkg"
            )
            .is_none());
        assert!(lock
            .get_locked_version("qemux86-64", &SysrootType::Rootfs, "test-rootfs-pkg")
            .is_none());
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::TargetSysroot,
                "test-sysroot-pkg"
            )
            .is_none());
        // Extensions and runtimes should still be present
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Extension("my-app".to_string()),
                "test-ext-pkg"
            )
            .is_some());
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Runtime("dev".to_string()),
                "test-runtime-pkg"
            )
            .is_some());
    }

    #[test]
    fn test_unlock_extension() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = create_test_config(&temp_dir);
        create_test_lock_file(&temp_dir);

        let cmd = UnlockCommand::new(
            config_path,
            false,
            None,
            Some("my-app".to_string()),
            None,
            false,
        );
        let result = cmd.execute();
        assert!(result.is_ok());

        // Verify extension is cleared but others remain
        let lock = LockFile::load(temp_dir.path()).unwrap();
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Extension("my-app".to_string()),
                "test-ext-pkg"
            )
            .is_none());
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Sdk("x86_64".to_string()),
                "test-sdk-pkg"
            )
            .is_some());
    }

    #[test]
    fn test_unlock_runtime() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = create_test_config(&temp_dir);
        create_test_lock_file(&temp_dir);

        let cmd = UnlockCommand::new(
            config_path,
            false,
            None,
            None,
            Some("dev".to_string()),
            false,
        );
        let result = cmd.execute();
        assert!(result.is_ok());

        // Verify runtime is cleared but others remain
        let lock = LockFile::load(temp_dir.path()).unwrap();
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Runtime("dev".to_string()),
                "test-runtime-pkg"
            )
            .is_none());
        assert!(lock
            .get_locked_version(
                "qemux86-64",
                &SysrootType::Sdk("x86_64".to_string()),
                "test-sdk-pkg"
            )
            .is_some());
    }
}
