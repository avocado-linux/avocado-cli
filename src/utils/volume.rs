//! Docker volume management utilities for Avocado CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::process::Command as AsyncCommand;
use uuid::Uuid;

use crate::utils::output::{print_info, OutputLevel};

/// Volume state configuration stored in .avocado-state file
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeState {
    /// The docker volume name
    pub volume_name: String,
    /// Original source path where avocado config was located
    pub source_path: String,
    /// Container tool being used (docker/podman)
    pub container_tool: String,
}

impl VolumeState {
    /// Create a new volume state with a generated UUID-based name
    pub fn new(source_path: PathBuf, container_tool: String) -> Self {
        let uuid = Uuid::new_v4();
        let volume_name = format!("avo-{uuid}");

        Self {
            volume_name,
            source_path: source_path.to_string_lossy().to_string(),
            container_tool,
        }
    }

    /// Load volume state from .avocado-state file in the given directory
    pub fn load_from_dir(dir_path: &Path) -> Result<Option<Self>> {
        let state_file = dir_path.join(".avocado-state");

        if !state_file.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&state_file).with_context(|| {
            format!("Failed to read volume state file: {}", state_file.display())
        })?;

        let state: VolumeState =
            serde_json::from_str(&content).with_context(|| "Failed to parse volume state file")?;

        Ok(Some(state))
    }

    /// Save volume state to .avocado-state file in the given directory
    pub fn save_to_dir(&self, dir_path: &Path) -> Result<()> {
        let state_file = dir_path.join(".avocado-state");

        let content = serde_json::to_string_pretty(self)
            .with_context(|| "Failed to serialize volume state")?;

        fs::write(&state_file, content).with_context(|| {
            format!(
                "Failed to write volume state file: {}",
                state_file.display()
            )
        })?;

        Ok(())
    }
}

/// Decide whether an `avo-<uuid>` volume is abandoned, based solely on
/// on-disk state (no container calls), given the `avocado.source_path`
/// label recorded on the volume when it was created.
///
/// Returns `Some(reason)` when the volume is a stale leftover safe to
/// reap, or `None` when it is still bound to a live project. The checks
/// mirror the ones `avocado prune` applies so both paths agree on what
/// "abandoned" means.
pub fn avo_abandonment_reason(volume_name: &str, source_path: Option<&str>) -> Option<String> {
    let source_path = match source_path {
        Some(path) => path,
        None => return Some("no source_path label found".to_string()),
    };

    let source_dir = Path::new(source_path);
    if !source_dir.exists() {
        return Some(format!("source directory '{source_path}' does not exist"));
    }

    let state_file = source_dir.join(".avocado-state");
    if !state_file.exists() {
        return Some(format!("no .avocado-state file in '{source_path}'"));
    }

    match VolumeState::load_from_dir(source_dir) {
        Ok(Some(state)) if state.volume_name == volume_name => None,
        Ok(Some(state)) => Some(format!(
            ".avocado-state links to '{}', not this volume",
            state.volume_name
        )),
        Ok(None) => Some(format!("could not read .avocado-state in '{source_path}'")),
        Err(e) => Some(format!("error reading .avocado-state: {e}")),
    }
}

/// Docker volume manager for Avocado operations
pub struct VolumeManager {
    container_tool: String,
    verbose: bool,
}

impl VolumeManager {
    /// Create a new volume manager
    pub fn new(container_tool: String, verbose: bool) -> Self {
        Self {
            container_tool,
            verbose,
        }
    }

    /// Get or create a docker volume for the given source directory
    pub async fn get_or_create_volume(&self, source_dir: &Path) -> Result<VolumeState> {
        // Try to load existing volume state
        if let Some(existing_state) = VolumeState::load_from_dir(source_dir)? {
            // Verify the volume still exists
            if self.volume_exists(&existing_state.volume_name).await? {
                if self.verbose {
                    print_info(
                        &format!("Using existing volume: {}", existing_state.volume_name),
                        OutputLevel::Normal,
                    );
                }
                return Ok(existing_state);
            } else if self.verbose {
                print_info(
                    &format!(
                        "Volume {} no longer exists, creating new one",
                        existing_state.volume_name
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        // We're about to mint a fresh volume for this project. That's the
        // natural moment to sweep volumes left behind by projects deleted
        // from disk without `avocado clean`: a manual `rm -rf` drops the
        // project folder but not its per-project `avo-<uuid>` volume, so
        // they otherwise accumulate forever. Best-effort by design: a
        // container-tool hiccup must never block volume creation.
        let reaped = self.reap_abandoned_avo_volumes().await;
        if !reaped.is_empty() && self.verbose {
            print_info(
                &format!("Reaped {} abandoned build volume(s).", reaped.len()),
                OutputLevel::Normal,
            );
        }

        // Create new volume state
        let state = VolumeState::new(source_dir.to_path_buf(), self.container_tool.clone());

        // Persist the state file BEFORE creating the docker volume. This
        // closes a cross-process race: a concurrent reap in another project
        // must never observe this volume after `docker volume create` but
        // before its `.avocado-state` exists, which would classify it as
        // abandoned and delete it mid-mint. Writing state first means the
        // volume is never visible without its state pointer. `volume_exists`
        // already tolerates a state file pointing at a not-yet-created
        // volume, so a failed create is simply superseded on the next call.
        state.save_to_dir(source_dir)?;

        // Create the docker volume with metadata
        self.create_volume(&state).await?;

        if self.verbose {
            print_info(
                &format!("Created new volume: {}", state.volume_name),
                OutputLevel::Normal,
            );
        }

        Ok(state)
    }

    /// Best-effort removal of abandoned `avo-*` volumes left behind by
    /// projects deleted from disk without `avocado clean`. Each `avo-*`
    /// volume is classified with [`avo_abandonment_reason`] and removed
    /// when abandoned. Any container-tool failure (daemon down, volume
    /// still held by a container, inspect error) is swallowed or skipped so
    /// this never blocks the caller, and a failed inspect is skipped rather
    /// than reaped. Returns the names of the volumes actually removed.
    pub async fn reap_abandoned_avo_volumes(&self) -> Vec<String> {
        let names = match self.list_avo_volumes().await {
            Ok(names) => names,
            Err(_) => return Vec::new(),
        };

        let mut removed = Vec::new();
        for name in names {
            // Skip on inspect failure: a transient docker hiccup must never
            // be read as "no source_path label", which would classify an
            // active project's volume as abandoned and delete it.
            let source_path = match self.inspect_source_path_label(&name).await {
                Ok(source_path) => source_path,
                Err(_) => continue,
            };
            if let Some(reason) = avo_abandonment_reason(&name, source_path.as_deref()) {
                if self.remove_volume(&name).await.is_ok() {
                    if self.verbose {
                        print_info(
                            &format!("Reaped abandoned build volume '{name}': {reason}"),
                            OutputLevel::Normal,
                        );
                    }
                    removed.push(name);
                }
            }
        }
        removed
    }

    /// List every `avo-*` docker volume name.
    async fn list_avo_volumes(&self) -> Result<Vec<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "ls", "--format", "{{.Name}}"])
            .output()
            .await
            .with_context(|| "Failed to list docker volumes")?;

        if !output.status.success() {
            anyhow::bail!("Failed to list docker volumes");
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("avo-"))
            .map(|s| s.to_string())
            .collect())
    }

    /// Read the `avocado.source_path` label off a volume.
    ///
    /// Returns `Ok(Some(path))` when the label is present, `Ok(None)` when
    /// the volume exists but carries no such label, and `Err` when the
    /// inspect itself failed (daemon down, timeout, unparseable output).
    /// The tri-state is load-bearing for the reap: a failed inspect must
    /// NOT collapse to "no label", or a transient hiccup would classify an
    /// active project's volume as abandoned and delete it. The caller skips
    /// the volume on `Err`.
    async fn inspect_source_path_label(&self, volume_name: &str) -> Result<Option<String>> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "inspect", volume_name])
            .output()
            .await
            .with_context(|| format!("Failed to inspect volume {volume_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to inspect volume {volume_name}: {}", stderr.trim());
        }

        let infos: Vec<VolumeInfo> = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| "Failed to parse volume inspect output")?;
        Ok(infos
            .into_iter()
            .next()
            .and_then(|info| info.labels)
            .and_then(|labels| labels.get("avocado.source_path").cloned()))
    }

    /// Check if a docker volume exists
    async fn volume_exists(&self, volume_name: &str) -> Result<bool> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "inspect", volume_name])
            .output()
            .await
            .with_context(|| "Failed to check if volume exists")?;

        Ok(output.status.success())
    }

    /// Create a docker volume with metadata
    async fn create_volume(&self, state: &VolumeState) -> Result<()> {
        let mut cmd = AsyncCommand::new(&self.container_tool);
        cmd.args(["volume", "create"]);

        // Add label with source path metadata
        cmd.args([
            "--label",
            &format!("avocado.source_path={}", state.source_path),
        ]);

        cmd.arg(&state.volume_name);

        let output = cmd
            .output()
            .await
            .with_context(|| "Failed to create docker volume")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create volume {}: {}", state.volume_name, stderr);
        }

        Ok(())
    }

    /// Remove a docker volume
    pub async fn remove_volume(&self, volume_name: &str) -> Result<()> {
        let output = AsyncCommand::new(&self.container_tool)
            .args(["volume", "rm", volume_name])
            .output()
            .await
            .with_context(|| "Failed to remove docker volume")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to remove volume {volume_name}: {stderr}");
        }

        Ok(())
    }

    /// Force remove a docker volume by first stopping and removing all containers using it
    pub async fn force_remove_volume(&self, volume_name: &str) -> Result<()> {
        // Get containers using this volume
        let containers = self.get_containers_using_volume(volume_name).await?;

        if !containers.is_empty() {
            if self.verbose {
                print_info(
                    &format!(
                        "Found {} container(s) using volume, stopping and removing...",
                        containers.len()
                    ),
                    OutputLevel::Normal,
                );
            }

            for container_id in &containers {
                // Kill the container (faster than stop)
                let _ = AsyncCommand::new(&self.container_tool)
                    .args(["kill", container_id])
                    .output()
                    .await;

                // Remove the container
                let output = AsyncCommand::new(&self.container_tool)
                    .args(["rm", "-f", container_id])
                    .output()
                    .await
                    .with_context(|| format!("Failed to remove container {container_id}"))?;

                if self.verbose && output.status.success() {
                    print_info(
                        &format!(
                            "Removed container: {}",
                            &container_id[..12.min(container_id.len())]
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Now remove the volume
        self.remove_volume(volume_name).await
    }

    /// Get list of container IDs using a specific volume
    async fn get_containers_using_volume(&self, volume_name: &str) -> Result<Vec<String>> {
        // Use docker ps with filter to find containers using this volume
        // This includes both running and stopped containers
        let output = AsyncCommand::new(&self.container_tool)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("volume={volume_name}"),
                "--format",
                "{{.ID}}",
            ])
            .output()
            .await
            .with_context(|| "Failed to list containers using volume")?;

        if !output.status.success() {
            // If the command fails, return empty list (volume might not exist)
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

    /// Name prefix for VS Code extension explorer containers
    const EXPLORER_CONTAINER_PREFIX: &'static str = "avocado-explorer-";

    /// Get list of VS Code extension explorer containers using a specific volume.
    /// These containers are created by the avocado-devtools VS Code extension
    /// to browse volume contents and can be safely stopped automatically.
    pub async fn get_explorer_containers_using_volume(
        &self,
        volume_name: &str,
    ) -> Result<Vec<String>> {
        // Find containers that:
        // 1. Use the specified volume
        // 2. Have a name matching the explorer pattern (avocado-explorer-*)
        let output = AsyncCommand::new(&self.container_tool)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("volume={volume_name}"),
                "--filter",
                &format!("name={}", Self::EXPLORER_CONTAINER_PREFIX),
                "--format",
                "{{.ID}}\t{{.Names}}",
            ])
            .output()
            .await
            .with_context(|| "Failed to list explorer containers")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let containers: Vec<String> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 2 {
                    let id = parts[0];
                    let name = parts[1];
                    // Double-check the name starts with our prefix
                    if name.starts_with(Self::EXPLORER_CONTAINER_PREFIX) {
                        return Some(id.to_string());
                    }
                }
                None
            })
            .collect();

        Ok(containers)
    }

    /// Stop and remove VS Code extension explorer containers using a specific volume.
    /// Returns the number of containers that were stopped.
    pub async fn stop_explorer_containers(&self, volume_name: &str) -> Result<usize> {
        let containers = self
            .get_explorer_containers_using_volume(volume_name)
            .await?;

        if containers.is_empty() {
            return Ok(0);
        }

        if self.verbose {
            print_info(
                &format!(
                    "Found {} VS Code explorer container(s) using volume, stopping...",
                    containers.len()
                ),
                OutputLevel::Normal,
            );
        }

        for container_id in &containers {
            // Stop the container gracefully with short timeout
            let _ = AsyncCommand::new(&self.container_tool)
                .args(["stop", "-t", "1", container_id])
                .output()
                .await;

            // Remove the container
            let output = AsyncCommand::new(&self.container_tool)
                .args(["rm", "-f", container_id])
                .output()
                .await
                .with_context(|| format!("Failed to remove explorer container {container_id}"))?;

            if self.verbose && output.status.success() {
                print_info(
                    &format!(
                        "Stopped explorer container: {}",
                        &container_id[..12.min(container_id.len())]
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(containers.len())
    }

    /// Remove a docker volume, automatically stopping any VS Code explorer containers first.
    /// Unlike force_remove_volume, this only stops known safe containers (explorer containers)
    /// and will still fail if other containers are using the volume.
    pub async fn remove_volume_with_explorer_cleanup(&self, volume_name: &str) -> Result<()> {
        // First, stop any VS Code explorer containers that might be using this volume
        let stopped = self.stop_explorer_containers(volume_name).await?;

        if stopped > 0 && self.verbose {
            print_info(
                &format!("Stopped {stopped} VS Code explorer container(s) before volume removal"),
                OutputLevel::Normal,
            );
        }

        // Now try to remove the volume
        self.remove_volume(volume_name).await
    }
}

/// Information about a docker volume
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeInfo {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "Mountpoint")]
    pub mountpoint: String,
    #[serde(rename = "Labels")]
    pub labels: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_volume_state_creation() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().to_path_buf();
        let container_tool = "docker".to_string();

        let state = VolumeState::new(source_path.clone(), container_tool.clone());

        assert!(state.volume_name.starts_with("avo-"));
        assert_eq!(state.source_path, source_path.to_string_lossy());
        assert_eq!(state.container_tool, container_tool);
    }

    #[test]
    fn test_volume_state_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().to_path_buf();
        let state = VolumeState::new(source_path.clone(), "docker".to_string());

        // Save state
        state.save_to_dir(temp_dir.path()).unwrap();

        // Load state
        let loaded_state = VolumeState::load_from_dir(temp_dir.path())
            .unwrap()
            .unwrap();

        assert_eq!(state.volume_name, loaded_state.volume_name);
        assert_eq!(state.source_path, loaded_state.source_path);
        assert_eq!(state.container_tool, loaded_state.container_tool);
    }

    #[test]
    fn test_load_nonexistent_state() {
        let temp_dir = TempDir::new().unwrap();
        let result = VolumeState::load_from_dir(temp_dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_abandonment_reason_active_when_state_matches() {
        // A volume whose recorded source directory still holds a
        // .avocado-state file pointing back at it is live: no reason.
        let temp_dir = TempDir::new().unwrap();
        let state = VolumeState::new(temp_dir.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(temp_dir.path()).unwrap();

        assert_eq!(
            avo_abandonment_reason(&state.volume_name, Some(&state.source_path)),
            None
        );
    }

    #[test]
    fn test_abandonment_reason_no_source_path_label() {
        let reason = avo_abandonment_reason("avo-x", None).unwrap();
        assert!(reason.contains("no source_path label"));
    }

    #[test]
    fn test_abandonment_reason_missing_source_dir() {
        let reason =
            avo_abandonment_reason("avo-x", Some("/nonexistent/avocado/project/xyz")).unwrap();
        assert!(reason.contains("does not exist"));
    }

    #[test]
    fn test_abandonment_reason_no_state_file() {
        // Source dir exists but carries no .avocado-state: abandoned.
        let temp_dir = TempDir::new().unwrap();
        let reason =
            avo_abandonment_reason("avo-x", Some(temp_dir.path().to_str().unwrap())).unwrap();
        assert!(reason.contains("no .avocado-state"));
    }

    #[test]
    fn test_abandonment_reason_state_points_elsewhere() {
        // The state file links to a different volume name, so this
        // volume is a stale leftover safe to reap.
        let temp_dir = TempDir::new().unwrap();
        let state = VolumeState::new(temp_dir.path().to_path_buf(), "docker".to_string());
        state.save_to_dir(temp_dir.path()).unwrap();

        let reason =
            avo_abandonment_reason("avo-different-uuid", Some(&state.source_path)).unwrap();
        assert!(reason.contains("not this volume"));
    }
}
