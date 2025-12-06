use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::volume::VolumeManager;

pub struct ExtCheckoutCommand {
    extension: String,
    ext_path: String,
    src_path: String,
    config_path: String,
    verbose: bool,
    container_tool: String,
    target: Option<String>,
}

impl ExtCheckoutCommand {
    pub fn new(
        extension: String,
        ext_path: String,
        src_path: String,
        config_path: String,
        verbose: bool,
        container_tool: String,
        target: Option<String>,
    ) -> Self {
        Self {
            extension,
            ext_path,
            src_path,
            config_path,
            verbose,
            container_tool,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let cwd = std::env::current_dir().context("Failed to get current directory")?;

        // Get the volume state for this project
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = match volume_manager.get_or_create_volume(&cwd).await {
            Ok(state) => state,
            Err(_) => {
                print_error(
                    "No avocado volume found. Run an extension build first to create the volume.",
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        };

        // Create a temporary container to access the volume
        let temp_container_name = format!("avocado-checkout-{}", uuid::Uuid::new_v4());

        if self.verbose {
            print_info(
                &format!("Creating temporary container: {temp_container_name}"),
                OutputLevel::Normal,
            );
        }

        // Get target from config to determine the extension sysroot path
        let target = self.resolve_target(&cwd, &volume_state.volume_name).await?;
        let ext_sysroot_path = format!("/opt/_avocado/{}/extensions/{}", target, self.extension);
        let full_ext_path = if self.ext_path.starts_with('/') {
            format!("{}{}", ext_sysroot_path, self.ext_path)
        } else {
            format!("{}/{}", ext_sysroot_path, self.ext_path)
        };

        if self.verbose {
            print_info(
                &format!("Extension sysroot path: {ext_sysroot_path}"),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Full source path in volume: {full_ext_path}"),
                OutputLevel::Normal,
            );
        }

        // Check if the path exists in the volume using a temporary container
        let path_exists = self
            .check_path_exists(&volume_state.volume_name, &full_ext_path)
            .await?;

        if !path_exists {
            print_error(
                &format!(
                    "Path '{}' not found in extension '{}' sysroot. Available paths can be listed with 'avocado ext list'.",
                    self.ext_path,
                    self.extension
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Determine if the path is a file or directory
        let is_directory = self
            .check_is_directory(&volume_state.volume_name, &full_ext_path)
            .await?;

        // Prepare the destination path
        let dest_path = cwd.join(&self.src_path);

        if self.verbose {
            print_info(
                &format!("Destination path: {}", dest_path.display()),
                OutputLevel::Normal,
            );
        }

        // Extract the files using docker cp
        self.extract_files(
            &volume_state.volume_name,
            &full_ext_path,
            &dest_path,
            is_directory,
            &temp_container_name,
        )
        .await?;

        // Fix ownership to match host user
        self.fix_ownership(&dest_path).await?;

        print_success(
            &format!(
                "Successfully checked out '{}' from extension '{}' to '{}'",
                self.ext_path, self.extension, self.src_path
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    async fn resolve_target(&self, cwd: &Path, volume_name: &str) -> Result<String> {
        // Strategy 1: Use CLI-provided target
        if let Some(ref target) = self.target {
            if self.verbose {
                print_info(
                    &format!("Using target from CLI: {target}"),
                    OutputLevel::Normal,
                );
            }
            return Ok(target.clone());
        }

        // Strategy 2: Try to get target from config file
        if let Ok(target) = self.get_target_from_config(cwd) {
            if self.verbose {
                print_info(
                    &format!("Using target from config: {target}"),
                    OutputLevel::Normal,
                );
            }
            return Ok(target);
        }

        // Strategy 3: Discover available targets from the volume
        if self.verbose {
            print_info(
                "No target in config, discovering from volume...",
                OutputLevel::Normal,
            );
        }

        let available_targets = self.discover_targets_from_volume(volume_name).await?;

        if available_targets.is_empty() {
            return Err(anyhow::anyhow!(
                "No targets found in volume. Please specify a target with --target or configure a runtime in your config file."
            ));
        }

        if available_targets.len() == 1 {
            let target = available_targets[0].clone();
            if self.verbose {
                print_info(
                    &format!("Auto-detected target from volume: {target}"),
                    OutputLevel::Normal,
                );
            }
            return Ok(target);
        }

        // Multiple targets available, need user to specify
        Err(anyhow::anyhow!(
            "Multiple targets found in volume: {}. Please specify one with --target",
            available_targets.join(", ")
        ))
    }

    async fn discover_targets_from_volume(&self, volume_name: &str) -> Result<Vec<String>> {
        // List directories in /opt/_avocado/ to find available targets
        let output = AsyncCommand::new(&self.container_tool)
            .arg("run")
            .arg("--rm")
            .arg("-v")
            .arg(format!("{volume_name}:/opt/_avocado:ro"))
            .arg("alpine:latest")
            .arg("sh")
            .arg("-c")
            .arg("ls -1 /opt/_avocado 2>/dev/null || true")
            .output()
            .await
            .context("Failed to list targets in volume")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let targets: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect();

        Ok(targets)
    }

    fn get_target_from_config(&self, cwd: &Path) -> Result<String> {
        let config_path = cwd.join(&self.config_path);
        let config_content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

        let parsed: serde_yaml::Value = serde_yaml::from_str(&config_content)
            .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

        // Get target from runtime configuration
        let target = parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_mapping())
            .and_then(|runtime_table| {
                if runtime_table.len() == 1 {
                    runtime_table.values().next()
                } else {
                    runtime_table.get("default")
                }
            })
            .and_then(|runtime| runtime.get("target"))
            .and_then(|target| target.as_str())
            .ok_or_else(|| anyhow::anyhow!("No target specified in runtime configuration"))?;

        Ok(target.to_string())
    }

    async fn check_path_exists(&self, volume_name: &str, path: &str) -> Result<bool> {
        // Create temporary container with volume mounted
        let output = AsyncCommand::new(&self.container_tool)
            .arg("run")
            .arg("--rm")
            .arg("-v")
            .arg(format!("{volume_name}:/opt/_avocado:ro"))
            .arg("alpine:latest")
            .arg("test")
            .arg("-e")
            .arg(path)
            .output()
            .await
            .context("Failed to check if path exists")?;

        Ok(output.status.success())
    }

    async fn check_is_directory(&self, volume_name: &str, path: &str) -> Result<bool> {
        let output = AsyncCommand::new(&self.container_tool)
            .arg("run")
            .arg("--rm")
            .arg("-v")
            .arg(format!("{volume_name}:/opt/_avocado:ro"))
            .arg("alpine:latest")
            .arg("test")
            .arg("-d")
            .arg(path)
            .output()
            .await
            .context("Failed to check if path is directory")?;

        Ok(output.status.success())
    }

    async fn extract_files(
        &self,
        volume_name: &str,
        source_path: &str,
        dest_path: &Path,
        is_directory: bool,
        _container_name: &str,
    ) -> Result<()> {
        // Create a temporary container to copy files from
        let temp_container_id = self.create_temp_container(volume_name).await?;

        // Ensure destination directory exists
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create destination directory: {}",
                    parent.display()
                )
            })?;
        }

        let docker_cp_source = format!("{temp_container_id}:{source_path}");

        // Both files and directories should preserve the directory hierarchy from the original ext-path
        // Use the ext_path to maintain the directory structure
        let ext_path_normalized = if self.ext_path.starts_with('/') {
            &self.ext_path[1..] // Remove leading slash
        } else {
            &self.ext_path
        };

        let docker_cp_dest = if is_directory {
            // For directories, we need to create the parent directory structure and
            // let docker cp create the final directory itself
            let full_dest_path = dest_path.join(ext_path_normalized);

            // Create parent directories (but not the final directory itself, docker cp will do that)
            if let Some(parent) = full_dest_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create parent directory: {}", parent.display())
                })?;
            }

            full_dest_path.to_string_lossy().to_string()
        } else {
            // For files, preserve the directory hierarchy from the original ext-path
            let full_dest_path = dest_path.join(ext_path_normalized);

            // Create parent directories
            if let Some(parent) = full_dest_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create parent directory: {}", parent.display())
                })?;
            }

            full_dest_path.to_string_lossy().to_string()
        };

        if self.verbose {
            print_info(
                &format!("Docker cp: {docker_cp_source} -> {docker_cp_dest}"),
                OutputLevel::Normal,
            );
        }

        let output = AsyncCommand::new(&self.container_tool)
            .arg("cp")
            .arg(&docker_cp_source)
            .arg(&docker_cp_dest)
            .output()
            .await
            .context("Failed to execute docker cp")?;

        // Clean up the temporary container
        let _ = AsyncCommand::new(&self.container_tool)
            .arg("rm")
            .arg("-f")
            .arg(&temp_container_id)
            .output()
            .await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Docker cp failed: {stderr}"));
        }

        Ok(())
    }

    async fn create_temp_container(&self, volume_name: &str) -> Result<String> {
        let output = AsyncCommand::new(&self.container_tool)
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

    async fn fix_ownership(&self, path: &Path) -> Result<()> {
        // On Unix systems, fix ownership to match the current user
        #[cfg(unix)]
        {
            // Get current user ID and group ID
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };

            if self.verbose {
                print_info(
                    &format!(
                        "Setting ownership to {}:{} for {}",
                        uid,
                        gid,
                        path.display()
                    ),
                    OutputLevel::Normal,
                );
            }

            // Use chown to fix ownership recursively
            let output = AsyncCommand::new("chown")
                .arg("-R")
                .arg(format!("{uid}:{gid}"))
                .arg(path)
                .output()
                .await
                .context("Failed to change file ownership")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if self.verbose {
                    print_info(
                        &format!("Note: Could not change ownership (may not be needed): {stderr}"),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // On Windows, ownership changes are not needed/supported in the same way
        #[cfg(windows)]
        {
            if self.verbose {
                print_info(
                    &format!(
                        "Ownership changes not needed on Windows for {}",
                        path.display()
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkout_command_creation() {
        let cmd = ExtCheckoutCommand::new(
            "test-ext".to_string(),
            "/etc/config".to_string(),
            "extracted/config".to_string(),
            "avocado.yaml".to_string(),
            false,
            "docker".to_string(),
            None,
        );

        assert_eq!(cmd.extension, "test-ext");
        assert_eq!(cmd.ext_path, "/etc/config");
        assert_eq!(cmd.src_path, "extracted/config");
    }
}
