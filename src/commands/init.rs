use anyhow::{Context, Result};
use std::fs;
use std::include_str;
use std::path::Path;

const GITHUB_API_BASE: &str = "https://api.github.com";
const REPO_OWNER: &str = "avocado-linux";
const REPO_NAME: &str = "avocado-os";
const REPO_BRANCH: &str = "main";
const REFERENCES_PATH: &str = "references";

/// GitHub API response structure for directory contents
#[derive(serde::Deserialize, Debug)]
struct GitHubContent {
    path: String,
    #[serde(rename = "type")]
    item_type: String,
    download_url: Option<String>,
}

/// Command to initialize a new Avocado project with configuration files.
///
/// This command creates a new `avocado.toml` configuration file in the specified
/// directory with default settings for the Avocado build system.
pub struct InitCommand {
    /// Target architecture (e.g., "qemux86-64")
    target: Option<String>,
    /// Directory to initialize (defaults to current directory)
    directory: Option<String>,
    /// Reference example to download from avocado-os repository
    reference: Option<String>,
}

impl InitCommand {
    /// Creates a new InitCommand instance.
    ///
    /// # Arguments
    /// * `target` - Optional target architecture string
    /// * `directory` - Optional directory path to initialize
    /// * `reference` - Optional reference example name to download
    pub fn new(
        target: Option<String>,
        directory: Option<String>,
        reference: Option<String>,
    ) -> Self {
        Self {
            target,
            directory,
            reference,
        }
    }

    /// Detects the system architecture and returns the appropriate default target.
    ///
    /// # Returns
    /// * `"qemux86-64"` for x86_64 systems
    /// * `"qemuarm64"` for aarch64 systems
    /// * `"qemux86-64"` as fallback for unknown architectures
    pub fn get_default_target() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "qemux86-64",
            "aarch64" => "qemuarm64",
            _ => "qemux86-64", // fallback to x86_64 for unknown architectures
        }
    }

    /// Loads the configuration template for the specified target.
    ///
    /// # Arguments
    /// * `target` - The target architecture string
    ///
    /// # Returns
    /// * The configuration template content as a string
    fn load_config_template(target: &str) -> String {
        match target {
            "qemux86-64" => include_str!("../../configs/qemu/qemux86-64.toml").to_string(),
            "qemuarm64" => include_str!("../../configs/qemu/qemuarm64.toml").to_string(),
            "reterminal" => include_str!("../../configs/seeed/reterminal.toml").to_string(),
            "reterminal-dm" => include_str!("../../configs/seeed/reterminal-dm.toml").to_string(),
            "jetson-orin-nano-devkit" => {
                include_str!("../../configs/nvidia/jetson-orin-nano-devkit.toml").to_string()
            }
            "raspberrypi4" => {
                include_str!("../../configs/raspberry-pi/raspberrypi-4-model-b.toml").to_string()
            }
            "raspberrypi5" => {
                include_str!("../../configs/raspberry-pi/raspberrypi-5.toml").to_string()
            }
            "icam-540" => include_str!("../../configs/advantech/icam-540.toml").to_string(),
            _ => {
                // Use default template and substitute the target
                let default_template = include_str!("../../configs/default.toml");
                default_template.replace("{target}", target)
            }
        }
    }

    /// Checks if a reference exists in the avocado-os repository.
    ///
    /// # Arguments
    /// * `reference_name` - The name of the reference to check
    ///
    /// # Returns
    /// * `Ok(true)` if the reference exists
    /// * `Ok(false)` if the reference doesn't exist
    /// * `Err` if there was an error checking
    async fn reference_exists(reference_name: &str) -> Result<bool> {
        let url = format!(
            "{GITHUB_API_BASE}/repos/{REPO_OWNER}/{REPO_NAME}/contents/{REFERENCES_PATH}/{reference_name}"
        );

        let client = reqwest::Client::builder()
            .user_agent("avocado-cli")
            .build()?;

        let response = client.get(&url).send().await?;

        Ok(response.status().is_success())
    }

    /// Downloads a file from GitHub and saves it to the specified path.
    ///
    /// # Arguments
    /// * `download_url` - The URL to download the file from
    /// * `dest_path` - The destination path to save the file
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if there was an error downloading or saving the file
    async fn download_file(download_url: &str, dest_path: &Path) -> Result<()> {
        let client = reqwest::Client::builder()
            .user_agent("avocado-cli")
            .build()?;

        let response = client
            .get(download_url)
            .send()
            .await
            .with_context(|| format!("Failed to download file from {download_url}"))?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to download file: HTTP {}", response.status());
        }

        let content = response
            .bytes()
            .await
            .with_context(|| "Failed to read response content")?;

        // Create parent directories if they don't exist
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
        }

        fs::write(dest_path, content)
            .with_context(|| format!("Failed to write file '{}'", dest_path.display()))?;

        Ok(())
    }

    /// Downloads the avocado.toml file from a reference and returns its content.
    ///
    /// # Arguments
    /// * `reference_name` - The name of the reference folder
    ///
    /// # Returns
    /// * `Ok(String)` with the file content if successful
    /// * `Err` if the file doesn't exist or cannot be downloaded
    async fn download_reference_config(reference_name: &str) -> Result<String> {
        let url = format!(
            "{GITHUB_API_BASE}/repos/{REPO_OWNER}/{REPO_NAME}/contents/{REFERENCES_PATH}/{reference_name}/avocado.toml"
        );

        let client = reqwest::Client::builder()
            .user_agent("avocado-cli")
            .build()?;

        let response = client.get(&url).send().await.with_context(|| {
            format!("Failed to fetch avocado.toml from reference '{reference_name}'")
        })?;

        if !response.status().is_success() {
            anyhow::bail!("Reference '{reference_name}' does not contain an avocado.toml file");
        }

        let content: GitHubContent = response
            .json()
            .await
            .with_context(|| "Failed to parse GitHub API response")?;

        if let Some(download_url) = content.download_url {
            let file_response = client
                .get(&download_url)
                .send()
                .await
                .with_context(|| "Failed to download avocado.toml")?;

            let file_content = file_response
                .text()
                .await
                .with_context(|| "Failed to read avocado.toml content")?;

            Ok(file_content)
        } else {
            anyhow::bail!("Could not get download URL for avocado.toml");
        }
    }

    /// Checks if a target is supported in the given TOML content.
    ///
    /// # Arguments
    /// * `toml_content` - The content of the avocado.toml file
    /// * `target` - The target to check for
    ///
    /// # Returns
    /// * `Ok(true)` if the target is supported or if supported_targets contains "*"
    /// * `Ok(false)` if the target is not supported
    /// * `Err` if the TOML cannot be parsed or doesn't have supported_targets
    fn is_target_supported(toml_content: &str, target: &str) -> Result<bool> {
        let config: toml::Value =
            toml::from_str(toml_content).with_context(|| "Failed to parse avocado.toml")?;

        let supported_targets_value = config.get("supported_targets").ok_or_else(|| {
            anyhow::anyhow!("Reference avocado.toml missing 'supported_targets' field")
        })?;

        // Handle supported_targets as a string (e.g., "*")
        if let Some(s) = supported_targets_value.as_str() {
            return Ok(s == "*");
        }

        // Handle supported_targets as an array
        if let Some(array) = supported_targets_value.as_array() {
            // Check if "*" is in supported_targets (means all targets supported)
            let has_wildcard = array.iter().any(|v| v.as_str() == Some("*"));

            if has_wildcard {
                return Ok(true);
            }

            // Check if the specific target is in supported_targets
            let is_supported = array.iter().any(|v| v.as_str() == Some(target));

            return Ok(is_supported);
        }

        anyhow::bail!("supported_targets must be either a string or an array");
    }

    /// Updates the default_target in the avocado.toml file.
    ///
    /// # Arguments
    /// * `toml_path` - Path to the avocado.toml file
    /// * `new_target` - The new target to set as default
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if the file cannot be read, parsed, or written
    fn update_default_target(toml_path: &Path, new_target: &str) -> Result<()> {
        let content = fs::read_to_string(toml_path)
            .with_context(|| format!("Failed to read '{}'", toml_path.display()))?;

        // Parse as toml::Value to preserve structure
        let mut config: toml::Value =
            toml::from_str(&content).with_context(|| "Failed to parse avocado.toml")?;

        // Update the default_target field
        if let Some(table) = config.as_table_mut() {
            table.insert(
                "default_target".to_string(),
                toml::Value::String(new_target.to_string()),
            );
        } else {
            anyhow::bail!("avocado.toml is not a valid TOML table");
        }

        // Write back to file
        let updated_content = toml::to_string_pretty(&config)
            .with_context(|| "Failed to serialize updated config")?;

        fs::write(toml_path, updated_content).with_context(|| {
            format!(
                "Failed to write updated config to '{}'",
                toml_path.display()
            )
        })?;

        Ok(())
    }

    /// Recursively downloads all contents from a GitHub directory.
    ///
    /// # Arguments
    /// * `reference_name` - The name of the reference folder
    /// * `github_path` - The path within the repository (relative to references/)
    /// * `local_base_path` - The base local path to download to
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if there was an error downloading the contents
    fn download_reference_contents<'a>(
        reference_name: &'a str,
        github_path: &'a str,
        local_base_path: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!(
                "{GITHUB_API_BASE}/repos/{REPO_OWNER}/{REPO_NAME}/contents/{REFERENCES_PATH}/{github_path}"
            );

            let client = reqwest::Client::builder()
                .user_agent("avocado-cli")
                .build()?;

            let response = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("Failed to fetch contents from {url}"))?;

            if !response.status().is_success() {
                anyhow::bail!(
                    "Failed to fetch contents: HTTP {}. The reference '{reference_name}' may not exist.",
                    response.status()
                );
            }

            let contents: Vec<GitHubContent> = response
                .json()
                .await
                .with_context(|| "Failed to parse GitHub API response")?;

            for item in contents {
                let relative_path = item
                    .path
                    .strip_prefix(&format!("{REFERENCES_PATH}/"))
                    .unwrap_or(&item.path)
                    .strip_prefix(&format!("{reference_name}/"))
                    .unwrap_or(&item.path);

                let local_path = local_base_path.join(relative_path);

                match item.item_type.as_str() {
                    "file" => {
                        if let Some(ref download_url) = item.download_url {
                            println!("  Downloading {relative_path}...");
                            Self::download_file(download_url, &local_path).await?;
                        }
                    }
                    "dir" => {
                        fs::create_dir_all(&local_path).with_context(|| {
                            format!("Failed to create directory '{}'", local_path.display())
                        })?;
                        // Recursively download directory contents
                        let sub_path = item.path.replace(&format!("{REFERENCES_PATH}/"), "");
                        Self::download_reference_contents(
                            reference_name,
                            &sub_path,
                            local_base_path,
                        )
                        .await?;
                    }
                    _ => {
                        // Skip other types (symlinks, submodules, etc.)
                    }
                }
            }

            Ok(())
        })
    }

    /// Creates a .gitignore file with Avocado-specific entries.
    ///
    /// # Arguments
    /// * `directory` - The directory to create the .gitignore file in
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if the file cannot be written
    fn create_gitignore(directory: &str) -> Result<()> {
        let gitignore_path = Path::new(directory).join(".gitignore");

        // Don't overwrite existing .gitignore files
        if gitignore_path.exists() {
            // Read existing content
            let existing_content = fs::read_to_string(&gitignore_path).with_context(|| {
                format!("Failed to read existing '{}'", gitignore_path.display())
            })?;

            // Check if .avocado-state is already in the .gitignore
            if !existing_content.contains(".avocado-state") {
                // Append to existing .gitignore
                let mut updated_content = existing_content;
                if !updated_content.ends_with('\n') {
                    updated_content.push('\n');
                }
                updated_content.push_str("\n# Avocado state files\n.avocado-state\n");

                fs::write(&gitignore_path, updated_content)
                    .with_context(|| format!("Failed to update '{}'", gitignore_path.display()))?;

                println!("✓ Updated .gitignore to ignore .avocado-state files.");
            }

            return Ok(());
        }

        // Create new .gitignore with Avocado-specific entries
        let gitignore_content = "# Avocado state files\n.avocado-state\n";

        fs::write(&gitignore_path, gitignore_content).with_context(|| {
            format!(
                "Failed to write .gitignore file '{}'",
                gitignore_path.display()
            )
        })?;

        println!("✓ Created .gitignore file.");

        Ok(())
    }

    /// Executes the init command, creating the avocado.toml configuration file.
    ///
    /// # Returns
    /// * `Ok(())` if the initialization was successful
    /// * `Err` if there was an error during initialization
    ///
    /// # Errors
    /// This function will return an error if:
    /// * The target directory cannot be created
    /// * The avocado.toml file already exists
    /// * The configuration file cannot be written
    /// * The reference doesn't exist (when using --reference)
    /// * There was an error downloading reference contents
    pub async fn execute(&self) -> Result<()> {
        let directory = self.directory.as_deref().unwrap_or(".");

        // Validate and create directory if it doesn't exist
        if !Path::new(directory).exists() {
            fs::create_dir_all(directory)
                .with_context(|| format!("Failed to create directory '{directory}'"))?;
        }

        // If reference is specified, download the reference project
        if let Some(ref_name) = &self.reference {
            println!("Initializing from reference '{ref_name}'...");

            // Check if reference exists
            println!("Checking if reference '{ref_name}' exists...");
            if !Self::reference_exists(ref_name).await? {
                anyhow::bail!(
                    "Reference '{ref_name}' not found in {}/{REPO_NAME}/{REPO_BRANCH}/{REFERENCES_PATH}. \
                    Please check the available references at https://github.com/{REPO_OWNER}/{REPO_NAME}/tree/{REPO_BRANCH}/{REFERENCES_PATH}",
                    REPO_OWNER
                );
            }

            // If both reference and target are specified, validate target support
            if let Some(target) = &self.target {
                println!("Validating target '{target}' is supported by reference '{ref_name}'...");

                // Download and parse the reference's avocado.toml
                let toml_content = Self::download_reference_config(ref_name).await?;

                // Check if target is supported
                if !Self::is_target_supported(&toml_content, target)? {
                    anyhow::bail!(
                        "Target '{target}' is not supported by reference '{ref_name}'. \
                        Please check the reference's avocado.toml for supported_targets."
                    );
                }

                println!("✓ Target '{target}' is supported by reference '{ref_name}'.");
            }

            // Download all contents from the reference
            println!("Downloading reference contents...");
            Self::download_reference_contents(ref_name, ref_name, Path::new(directory)).await?;

            // If a target was specified, update the default_target in the downloaded avocado.toml
            if let Some(target) = &self.target {
                let toml_path = Path::new(directory).join("avocado.toml");
                if toml_path.exists() {
                    println!("Updating default_target to '{target}'...");
                    Self::update_default_target(&toml_path, target)?;
                    println!("✓ Updated default_target to '{target}'.");
                }
            }

            println!(
                "✓ Successfully initialized project from reference '{ref_name}' in '{}'.",
                Path::new(directory)
                    .canonicalize()
                    .unwrap_or_else(|_| Path::new(directory).to_path_buf())
                    .display()
            );
        } else {
            // Original behavior: create avocado.toml from template
            let target = self
                .target
                .as_deref()
                .unwrap_or_else(|| Self::get_default_target());

            // Create the avocado.toml file path
            let toml_path = Path::new(directory).join("avocado.toml");

            // Check if configuration file already exists
            if toml_path.exists() {
                anyhow::bail!(
                    "Configuration file '{}' already exists.",
                    toml_path.display()
                );
            }

            // Load the configuration template for the target
            let config_content = Self::load_config_template(target);

            // Write the configuration file
            fs::write(&toml_path, config_content).with_context(|| {
                format!(
                    "Failed to write configuration file '{}'",
                    toml_path.display()
                )
            })?;

            println!(
                "✓ Created config at {}.",
                toml_path
                    .canonicalize()
                    .unwrap_or_else(|_| toml_path.to_path_buf())
                    .display()
            );
        }

        // Create .gitignore file (for both reference and non-reference paths)
        Self::create_gitignore(directory)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_init_default_target() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.toml");
        assert!(config_path.exists());

        let content = fs::read_to_string(&config_path).unwrap();
        let expected_target = InitCommand::get_default_target();
        assert!(content.contains(&format!("default_target = \"{expected_target}\"")));
        assert!(content.contains("[runtime.dev.dependencies]"));
        assert!(content.contains("avocado-img-bootfiles = \"*\""));
        assert!(content.contains("avocado-img-rootfs = \"*\""));
        assert!(content.contains("avocado-img-initramfs = \"*\""));
        assert!(content.contains("avocado-ext-dev = { ext = \"avocado-ext-dev\", vsn = \"*\" }"));
        assert!(content.contains("image = \"docker.io/avocadolinux/sdk:apollo-edge\""));
        assert!(content.contains("[ext.app]"));
        assert!(content.contains("types = [\"sysext\", \"confext\"]"));
        assert!(content.contains("[ext.config]"));
        assert!(content.contains("avocado-sdk-toolchain = \"*\""));
    }

    #[tokio::test]
    async fn test_init_custom_target() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(
            Some("custom-arch".to_string()),
            Some(temp_path.to_string()),
            None,
        );
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.toml");
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("default_target = \"custom-arch\""));
    }

    #[tokio::test]
    async fn test_init_file_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let config_path = PathBuf::from(temp_path).join("avocado.toml");

        // Create existing file
        fs::write(&config_path, "existing content").unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("already exists"));
    }

    #[tokio::test]
    async fn test_init_creates_directory() {
        let temp_dir = TempDir::new().unwrap();
        let new_dir_path = temp_dir.path().join("new_project");
        let new_dir_str = new_dir_path.to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(new_dir_str.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());
        assert!(new_dir_path.exists());

        let config_path = new_dir_path.join("avocado.toml");
        assert!(config_path.exists());
    }

    #[tokio::test]
    async fn test_init_creates_gitignore() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let gitignore_path = PathBuf::from(temp_path).join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(&gitignore_path).unwrap();
        assert!(content.contains(".avocado-state"));
    }

    #[tokio::test]
    async fn test_init_updates_existing_gitignore() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let gitignore_path = PathBuf::from(temp_path).join(".gitignore");

        // Create existing .gitignore with some content
        fs::write(&gitignore_path, "*.log\n").unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let content = fs::read_to_string(&gitignore_path).unwrap();
        assert!(content.contains("*.log")); // Original content preserved
        assert!(content.contains(".avocado-state")); // New content added
    }

    #[tokio::test]
    async fn test_init_does_not_duplicate_gitignore_entries() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let gitignore_path = PathBuf::from(temp_path).join(".gitignore");

        // Create existing .gitignore with .avocado-state already in it
        fs::write(&gitignore_path, "*.log\n.avocado-state\n").unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let content = fs::read_to_string(&gitignore_path).unwrap();
        // Count occurrences of .avocado-state - should be exactly 1
        let count = content.matches(".avocado-state").count();
        assert_eq!(count, 1);
    }
}
