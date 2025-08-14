use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::volume::{VolumeManager, VolumeState};

/// Command to clean the avocado project by removing docker volumes and state files.
///
/// This command removes docker volumes and `.avocado-state` files created by the Avocado build system.
pub struct CleanCommand {
    /// Directory to clean (defaults to current directory)
    directory: Option<String>,
    /// Whether to also remove docker volumes
    volumes: bool,
    /// Container tool to use (docker/podman)
    container_tool: String,
    /// Verbose output
    verbose: bool,
}

impl CleanCommand {
    /// Creates a new CleanCommand instance.
    ///
    /// # Arguments
    /// * `directory` - Optional directory path to clean (defaults to current directory)
    /// * `volumes` - Whether to clean docker volumes
    /// * `container_tool` - Container tool to use (docker/podman)
    /// * `verbose` - Enable verbose output
    pub fn new(
        directory: Option<String>,
        volumes: bool,
        container_tool: Option<String>,
        verbose: bool,
    ) -> Self {
        Self {
            directory,
            volumes,
            container_tool: container_tool.unwrap_or_else(|| "docker".to_string()),
            verbose,
        }
    }

    /// Executes the clean command, removing volumes, state files, and optionally legacy directories.
    ///
    /// # Returns
    /// * `Ok(())` if the cleaning was successful
    /// * `Err` if there was an error during cleaning
    ///
    /// # Errors
    /// This function will return an error if:
    /// * The specified directory does not exist
    /// * Docker volumes cannot be removed
    /// * State files cannot be removed due to permissions or other I/O errors
    pub async fn execute(&self) -> Result<()> {
        let directory = self.directory.as_deref().unwrap_or(".");

        // Resolve the full path to the directory
        let directory_path = if Path::new(directory).is_absolute() {
            PathBuf::from(directory)
        } else {
            std::env::current_dir()
                .context("Failed to get current directory")?
                .join(directory)
        };

        // Check if the directory exists
        if !directory_path.exists() {
            print_error(
                &format!("Directory '{}' does not exist.", directory_path.display()),
                OutputLevel::Normal,
            );
            anyhow::bail!("Directory '{}' does not exist", directory_path.display());
        }

        // Clean docker volume if requested
        if self.volumes {
            self.clean_volume(&directory_path).await?;
        }

        // Clean state file
        self.clean_state_file(&directory_path)?;

        Ok(())
    }

    /// Clean docker volume associated with the directory
    async fn clean_volume(&self, directory_path: &Path) -> Result<()> {
        // Try to load existing volume state
        if let Some(volume_state) = VolumeState::load_from_dir(directory_path)? {
            let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);

            if self.verbose {
                print_info(
                    &format!("Removing docker volume: {}", volume_state.volume_name),
                    OutputLevel::Normal,
                );
            }

            volume_manager
                .remove_volume(&volume_state.volume_name)
                .await
                .with_context(|| {
                    format!("Failed to remove volume: {}", volume_state.volume_name)
                })?;

            print_success(
                &format!("Removed docker volume: {}", volume_state.volume_name),
                OutputLevel::Normal,
            );
        } else if self.verbose {
            print_info(
                "No volume state found, skipping volume cleanup.",
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Clean .avocado-state file
    fn clean_state_file(&self, directory_path: &Path) -> Result<()> {
        let state_file = directory_path.join(".avocado-state");

        if state_file.exists() {
            fs::remove_file(&state_file).with_context(|| {
                format!("Failed to remove state file: {}", state_file.display())
            })?;

            print_success(
                &format!("Removed state file: {}", state_file.display()),
                OutputLevel::Normal,
            );
        } else if self.verbose {
            print_info("No .avocado-state file found.", OutputLevel::Normal);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_clean_no_state_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Execute clean command without state file
        let clean_cmd = CleanCommand::new(
            Some(temp_path.to_str().unwrap().to_string()),
            false,
            None,
            false,
        );
        let result = clean_cmd.execute().await;

        // Should succeed even if no state file exists
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_clean_nonexistent_directory() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent_path = temp_dir.path().join("nonexistent");

        // Execute clean command on nonexistent directory
        let clean_cmd = CleanCommand::new(
            Some(nonexistent_path.to_str().unwrap().to_string()),
            false,
            None,
            false,
        );
        let result = clean_cmd.execute().await;

        // Should fail for nonexistent directory
        assert!(result.is_err());
    }

    #[test]
    fn test_clean_default_directory() {
        // Test with None (current directory)
        let clean_cmd = CleanCommand::new(None, false, None, false);

        // This test just ensures the command can be created with None
        // We don't execute it since it would try to clean the actual current directory
        assert_eq!(clean_cmd.directory, None);
    }

    #[tokio::test]
    async fn test_clean_state_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a .avocado-state file
        let state_file = temp_path.join(".avocado-state");
        fs::write(&state_file, "test state").unwrap();

        // Verify it exists before cleaning
        assert!(state_file.exists());

        // Execute clean command
        let clean_cmd = CleanCommand::new(
            Some(temp_path.to_str().unwrap().to_string()),
            false,
            None,
            false,
        );
        let result = clean_cmd.execute().await;

        assert!(result.is_ok());
        assert!(!state_file.exists());
    }
}
