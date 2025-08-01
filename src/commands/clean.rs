use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::output::{print_error, print_info, print_success, OutputLevel};

/// Command to clean the avocado project by removing the _avocado directory.
///
/// This command removes the `_avocado` directory from the specified directory,
/// which contains build artifacts and temporary files created by the Avocado build system.
pub struct CleanCommand {
    /// Directory to clean (defaults to current directory)
    directory: Option<String>,
}

impl CleanCommand {
    /// Creates a new CleanCommand instance.
    ///
    /// # Arguments
    /// * `directory` - Optional directory path to clean (defaults to current directory)
    pub fn new(directory: Option<String>) -> Self {
        Self { directory }
    }

    /// Executes the clean command, removing the _avocado directory.
    ///
    /// # Returns
    /// * `Ok(())` if the cleaning was successful or if no _avocado directory exists
    /// * `Err` if there was an error during cleaning
    ///
    /// # Errors
    /// This function will return an error if:
    /// * The specified directory does not exist
    /// * The _avocado directory cannot be removed due to permissions or other I/O errors
    pub fn execute(&self) -> Result<()> {
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

        // Path to the _avocado directory
        let avocado_dir = directory_path.join("_avocado");

        // Check if _avocado directory exists
        if !avocado_dir.exists() {
            print_info(
                &format!(
                    "No _avocado directory found in '{}'.",
                    directory_path.display()
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Remove the _avocado directory
        fs::remove_dir_all(&avocado_dir).with_context(|| {
            format!(
                "Failed to remove _avocado directory: {}",
                avocado_dir.display()
            )
        })?;

        print_success(
            &format!(
                "Removed _avocado directory from '{}'.",
                directory_path.display()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_clean_removes_avocado_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create an _avocado directory
        let avocado_dir = temp_path.join("_avocado");
        fs::create_dir(&avocado_dir).unwrap();

        // Create some content in the _avocado directory
        fs::write(avocado_dir.join("test_file.txt"), "test content").unwrap();

        // Verify it exists before cleaning
        assert!(avocado_dir.exists());

        // Execute clean command
        let clean_cmd = CleanCommand::new(Some(temp_path.to_str().unwrap().to_string()));
        let result = clean_cmd.execute();

        assert!(result.is_ok());
        assert!(!avocado_dir.exists());
    }

    #[test]
    fn test_clean_no_avocado_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Execute clean command without _avocado directory
        let clean_cmd = CleanCommand::new(Some(temp_path.to_str().unwrap().to_string()));
        let result = clean_cmd.execute();

        // Should succeed even if no _avocado directory exists
        assert!(result.is_ok());
    }

    #[test]
    fn test_clean_nonexistent_directory() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent_path = temp_dir.path().join("nonexistent");

        // Execute clean command on nonexistent directory
        let clean_cmd = CleanCommand::new(Some(nonexistent_path.to_str().unwrap().to_string()));
        let result = clean_cmd.execute();

        // Should fail for nonexistent directory
        assert!(result.is_err());
    }

    #[test]
    fn test_clean_default_directory() {
        // Test with None (current directory)
        let clean_cmd = CleanCommand::new(None);

        // This test just ensures the command can be created with None
        // We don't execute it since it would try to clean the actual current directory
        assert_eq!(clean_cmd.directory, None);
    }

    #[test]
    fn test_clean_removes_nested_structure() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a nested structure in _avocado directory
        let avocado_dir = temp_path.join("_avocado");
        let nested_dir = avocado_dir.join("subdir");
        fs::create_dir_all(&nested_dir).unwrap();

        // Create files in nested structure
        fs::write(avocado_dir.join("root_file.txt"), "root content").unwrap();
        fs::write(nested_dir.join("nested_file.txt"), "nested content").unwrap();

        // Verify structure exists
        assert!(avocado_dir.exists());
        assert!(nested_dir.exists());

        // Execute clean command
        let clean_cmd = CleanCommand::new(Some(temp_path.to_str().unwrap().to_string()));
        let result = clean_cmd.execute();

        assert!(result.is_ok());
        assert!(!avocado_dir.exists());
        assert!(!nested_dir.exists());
    }
}
