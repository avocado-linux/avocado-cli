//! Prune command to clean up abandoned Docker volumes.
//!
//! This module identifies and removes Docker volumes that are no longer associated
//! with active Avocado configurations or containers.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_error, print_info, print_success, print_warning, OutputLevel};
use crate::utils::volume::VolumeState;

/// Information about a Docker volume from `docker volume inspect`
#[derive(Debug, Clone, Deserialize)]
struct VolumeInspectInfo {
    #[serde(rename = "Name")]
    #[allow(dead_code)]
    name: String,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

/// Classification of a volume's status
#[derive(Debug, Clone, PartialEq)]
enum VolumeStatus {
    /// Volume is actively linked to an existing config
    Active,
    /// Volume is abandoned and can be removed
    Abandoned(String), // Reason for abandonment
}

/// Command to prune abandoned Docker volumes.
///
/// This command identifies and removes volumes that are no longer needed:
/// - `avo-<uuid>` volumes: state volumes for avocado configs
/// - `avocado-src-*` and `avocado-state-*` volumes: container volumes
pub struct PruneCommand {
    /// Container tool to use (docker/podman)
    container_tool: String,
    /// Enable verbose output
    verbose: bool,
    /// Perform dry run without actually removing volumes
    dry_run: bool,
}

impl PruneCommand {
    /// Creates a new PruneCommand instance.
    ///
    /// # Arguments
    /// * `container_tool` - Container tool to use (docker/podman)
    /// * `verbose` - Enable verbose output
    /// * `dry_run` - If true, only show what would be removed
    pub fn new(container_tool: Option<String>, verbose: bool, dry_run: bool) -> Self {
        Self {
            container_tool: container_tool.unwrap_or_else(|| "docker".to_string()),
            verbose,
            dry_run,
        }
    }

    /// Executes the prune command.
    ///
    /// # Returns
    /// * `Ok(())` if the pruning was successful
    /// * `Err` if there was an error during pruning
    pub async fn execute(&self) -> Result<()> {
        print_info(
            "Scanning for abandoned Docker volumes...",
            OutputLevel::Normal,
        );

        // Get all avocado-related volumes
        let volumes = self.list_avocado_volumes().await?;

        if volumes.is_empty() {
            print_info("No Avocado-related volumes found.", OutputLevel::Normal);
            return Ok(());
        }

        if self.verbose {
            print_info(
                &format!("Found {} Avocado-related volume(s)", volumes.len()),
                OutputLevel::Normal,
            );
        }

        let mut abandoned_count = 0;
        let mut active_count = 0;
        let mut removed_count = 0;
        let mut failed_count = 0;

        for volume_name in &volumes {
            let status = self.classify_volume(volume_name).await?;

            match status {
                VolumeStatus::Active => {
                    active_count += 1;
                    if self.verbose {
                        print_info(
                            &format!("Volume '{}' is active, skipping", volume_name),
                            OutputLevel::Normal,
                        );
                    }
                }
                VolumeStatus::Abandoned(reason) => {
                    abandoned_count += 1;

                    if self.dry_run {
                        print_warning(
                            &format!("[DRY RUN] Would remove '{}': {}", volume_name, reason),
                            OutputLevel::Normal,
                        );
                    } else {
                        print_info(
                            &format!("Removing '{}': {}", volume_name, reason),
                            OutputLevel::Normal,
                        );

                        match self.remove_volume_with_containers(volume_name).await {
                            Ok(()) => {
                                removed_count += 1;
                                print_success(
                                    &format!("Removed volume '{}'", volume_name),
                                    OutputLevel::Normal,
                                );
                            }
                            Err(e) => {
                                failed_count += 1;
                                print_error(
                                    &format!("Failed to remove '{}': {}", volume_name, e),
                                    OutputLevel::Normal,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Print summary
        println!();
        if self.dry_run {
            print_info(
                &format!(
                    "Dry run complete: {} active, {} would be removed",
                    active_count, abandoned_count
                ),
                OutputLevel::Normal,
            );
        } else {
            let mut summary = format!(
                "Prune complete: {} active, {} removed",
                active_count, removed_count
            );
            if failed_count > 0 {
                summary.push_str(&format!(", {} failed", failed_count));
            }
            print_success(&summary, OutputLevel::Normal);
        }

        Ok(())
    }

    /// List all Avocado-related volumes
    async fn list_avocado_volumes(&self) -> Result<Vec<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "ls", "--format", "{{.Name}}"])
            .output()
            .await
            .context("Failed to list Docker volumes")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to list volumes: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let volumes: Vec<String> = stdout
            .lines()
            .filter(|line| {
                line.starts_with("avo-")
                    || line.starts_with("avocado-src-")
                    || line.starts_with("avocado-state-")
            })
            .map(|s| s.to_string())
            .collect();

        Ok(volumes)
    }

    /// Classify a volume as active or abandoned
    async fn classify_volume(&self, volume_name: &str) -> Result<VolumeStatus> {
        if volume_name.starts_with("avo-") {
            self.classify_avo_volume(volume_name).await
        } else if volume_name.starts_with("avocado-src-")
            || volume_name.starts_with("avocado-state-")
        {
            self.classify_container_volume(volume_name).await
        } else {
            Ok(VolumeStatus::Active) // Unknown volume type, don't touch
        }
    }

    /// Classify an avo-<uuid> volume
    ///
    /// These are state volumes for avocado configs. Check:
    /// 1. If source_path directory exists
    /// 2. If .avocado-state file exists in that directory
    /// 3. If the state file links back to this volume's UUID
    async fn classify_avo_volume(&self, volume_name: &str) -> Result<VolumeStatus> {
        // Get volume info including labels
        let volume_info = self.inspect_volume(volume_name).await?;

        // Get the source_path label
        let source_path = match &volume_info.labels {
            Some(labels) => labels.get("avocado.source_path"),
            None => None,
        };

        let source_path = match source_path {
            Some(path) => path,
            None => {
                return Ok(VolumeStatus::Abandoned(
                    "no source_path label found".to_string(),
                ));
            }
        };

        // Check if the source directory exists
        let source_dir = Path::new(source_path);
        if !source_dir.exists() {
            return Ok(VolumeStatus::Abandoned(format!(
                "source directory '{}' does not exist",
                source_path
            )));
        }

        // Check for .avocado-state file
        let state_file = source_dir.join(".avocado-state");
        if !state_file.exists() {
            return Ok(VolumeStatus::Abandoned(format!(
                "no .avocado-state file in '{}'",
                source_path
            )));
        }

        // Load and verify the state file links to this volume
        match VolumeState::load_from_dir(source_dir) {
            Ok(Some(state)) => {
                if state.volume_name == volume_name {
                    Ok(VolumeStatus::Active)
                } else {
                    Ok(VolumeStatus::Abandoned(format!(
                        ".avocado-state links to '{}', not this volume",
                        state.volume_name
                    )))
                }
            }
            Ok(None) => Ok(VolumeStatus::Abandoned(format!(
                "could not read .avocado-state in '{}'",
                source_path
            ))),
            Err(e) => Ok(VolumeStatus::Abandoned(format!(
                "error reading .avocado-state: {}",
                e
            ))),
        }
    }

    /// Classify an avocado-src-* or avocado-state-* volume
    ///
    /// These volumes are abandoned if:
    /// - Not associated with any containers, OR
    /// - Only associated with stopped containers
    async fn classify_container_volume(&self, volume_name: &str) -> Result<VolumeStatus> {
        // Get containers using this volume
        let containers = self.get_containers_using_volume(volume_name).await?;

        if containers.is_empty() {
            return Ok(VolumeStatus::Abandoned(
                "not associated with any containers".to_string(),
            ));
        }

        // Check if any container is running
        for container_id in &containers {
            if self.is_container_running(container_id).await? {
                return Ok(VolumeStatus::Active);
            }
        }

        // All associated containers are stopped
        Ok(VolumeStatus::Abandoned(format!(
            "only associated with {} stopped container(s)",
            containers.len()
        )))
    }

    /// Inspect a volume and return its info
    async fn inspect_volume(&self, volume_name: &str) -> Result<VolumeInspectInfo> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "inspect", volume_name])
            .output()
            .await
            .context("Failed to inspect volume")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to inspect volume {}: {}", volume_name, stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let infos: Vec<VolumeInspectInfo> =
            serde_json::from_str(&stdout).context("Failed to parse volume inspect output")?;

        infos
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No volume info returned"))
    }

    /// Get list of container IDs using a specific volume
    async fn get_containers_using_volume(&self, volume_name: &str) -> Result<Vec<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("volume={}", volume_name),
                "--format",
                "{{.ID}}",
            ])
            .output()
            .await
            .context("Failed to list containers using volume")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let containers: Vec<String> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|s| s.to_string())
            .collect();

        Ok(containers)
    }

    /// Check if a container is running
    async fn is_container_running(&self, container_id: &str) -> Result<bool> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["inspect", "--format", "{{.State.Running}}", container_id])
            .output()
            .await
            .context("Failed to inspect container")?;

        if !output.status.success() {
            return Ok(false);
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(stdout == "true")
    }

    /// Remove a volume, including stopping/removing any associated containers
    async fn remove_volume_with_containers(&self, volume_name: &str) -> Result<()> {
        // Get containers using this volume
        let containers = self.get_containers_using_volume(volume_name).await?;

        // Remove associated containers
        for container_id in &containers {
            if self.verbose {
                print_info(
                    &format!(
                        "Removing container: {}",
                        &container_id[..12.min(container_id.len())]
                    ),
                    OutputLevel::Normal,
                );
            }

            // Kill the container (in case it's running)
            let _ = AsyncCommand::new(&self.container_tool)
                .args(["kill", container_id])
                .output()
                .await;

            // Remove the container
            let output = AsyncCommand::new(&self.container_tool)
                .args(["rm", "-f", container_id])
                .output()
                .await
                .with_context(|| format!("Failed to remove container {}", container_id))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                print_warning(
                    &format!(
                        "Warning: could not remove container {}: {}",
                        &container_id[..12.min(container_id.len())],
                        stderr.trim()
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        // Remove the volume
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "rm", volume_name])
            .output()
            .await
            .context("Failed to remove volume")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to remove volume: {}", stderr.trim());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_command_creation() {
        let cmd = PruneCommand::new(None, false, false);
        assert_eq!(cmd.container_tool, "docker");
        assert!(!cmd.verbose);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn test_prune_command_with_podman() {
        let cmd = PruneCommand::new(Some("podman".to_string()), true, true);
        assert_eq!(cmd.container_tool, "podman");
        assert!(cmd.verbose);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_volume_status_equality() {
        assert_eq!(VolumeStatus::Active, VolumeStatus::Active);
        assert_eq!(
            VolumeStatus::Abandoned("test".to_string()),
            VolumeStatus::Abandoned("test".to_string())
        );
        assert_ne!(
            VolumeStatus::Active,
            VolumeStatus::Abandoned("test".to_string())
        );
    }
}
