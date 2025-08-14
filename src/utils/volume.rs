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

        // Create new volume state
        let state = VolumeState::new(source_dir.to_path_buf(), self.container_tool.clone());

        // Create the docker volume with metadata
        self.create_volume(&state).await?;

        // Save state to file
        state.save_to_dir(source_dir)?;

        if self.verbose {
            print_info(
                &format!("Created new volume: {}", state.volume_name),
                OutputLevel::Normal,
            );
        }

        Ok(state)
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
            anyhow::bail!("Failed to remove volume {}: {}", volume_name, stderr);
        }

        Ok(())
    }
}

/// Information about a docker volume
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
}
