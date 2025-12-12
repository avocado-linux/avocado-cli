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
    /// The SHA of the content (for files, dirs, and submodules)
    sha: Option<String>,
    /// For submodules: the git URL of the submodule repository (may not always be present)
    submodule_git_url: Option<String>,
    /// The size of the file (submodules have size 0)
    size: Option<u64>,
    /// The git URL - for submodules this points to the submodule repo's tree
    git_url: Option<String>,
}

/// Command to initialize a new Avocado project with configuration files.
///
/// This command creates a new `avocado.yaml` configuration file in the specified
/// directory with default settings for the Avocado build system.
pub struct InitCommand {
    /// Target architecture (e.g., "qemux86-64")
    target: Option<String>,
    /// Directory to initialize (defaults to current directory)
    directory: Option<String>,
    /// Reference example to download from avocado-os repository
    reference: Option<String>,
    /// Branch to fetch reference from (defaults to "main")
    reference_branch: Option<String>,
    /// Specific commit SHA to fetch reference from
    reference_commit: Option<String>,
    /// Repository to fetch reference from (format: "owner/repo", defaults to "avocado-linux/avocado-os")
    reference_repo: Option<String>,
}

impl InitCommand {
    /// Creates a new InitCommand instance.
    ///
    /// # Arguments
    /// * `target` - Optional target architecture string
    /// * `directory` - Optional directory path to initialize
    /// * `reference` - Optional reference example name to download
    /// * `reference_branch` - Optional branch to fetch reference from
    /// * `reference_commit` - Optional specific commit SHA to fetch reference from
    /// * `reference_repo` - Optional repository to fetch reference from (format: "owner/repo")
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: Option<String>,
        directory: Option<String>,
        reference: Option<String>,
        reference_branch: Option<String>,
        reference_commit: Option<String>,
        reference_repo: Option<String>,
    ) -> Self {
        Self {
            target,
            directory,
            reference,
            reference_branch,
            reference_commit,
            reference_repo,
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

    /// Returns the repository owner from reference_repo or the default.
    fn get_repo_owner(&self) -> &str {
        self.reference_repo
            .as_ref()
            .and_then(|repo| repo.split('/').next())
            .unwrap_or(REPO_OWNER)
    }

    /// Returns the repository name from reference_repo or the default.
    fn get_repo_name(&self) -> &str {
        self.reference_repo
            .as_ref()
            .and_then(|repo| repo.split('/').nth(1))
            .unwrap_or(REPO_NAME)
    }

    /// Returns the git ref (commit, branch, or default branch) for API requests.
    fn get_git_ref(&self) -> &str {
        if let Some(commit) = &self.reference_commit {
            commit.as_str()
        } else if let Some(branch) = &self.reference_branch {
            branch.as_str()
        } else {
            REPO_BRANCH
        }
    }

    /// Returns the display string for the reference source (for error messages).
    fn get_reference_source(&self) -> String {
        let owner = self.get_repo_owner();
        let name = self.get_repo_name();
        let git_ref = self.get_git_ref();
        format!("{owner}/{name}/{git_ref}/{REFERENCES_PATH}")
    }

    /// Loads the configuration template for the specified target.
    ///
    /// # Arguments
    /// * `target` - The target architecture string
    ///
    /// # Returns
    /// * The configuration template content as a string
    fn load_config_template(target: &str) -> String {
        // Try to load YAML config first, fall back to default with target substitution
        let yaml_content = match target {
            "reterminal" => Some(include_str!("../../configs/seeed/reterminal.yaml")),
            "reterminal-dm" => Some(include_str!("../../configs/seeed/reterminal-dm.yaml")),
            "jetson-orin-nano-devkit" => Some(include_str!(
                "../../configs/nvidia/jetson-orin-nano-devkit.yaml"
            )),
            "raspberrypi4" => Some(include_str!(
                "../../configs/raspberry-pi/raspberrypi-4-model-b.yaml"
            )),
            "raspberrypi5" => Some(include_str!(
                "../../configs/raspberry-pi/raspberrypi-5.yaml"
            )),
            "icam-540" => Some(include_str!("../../configs/advantech/icam-540.yaml")),
            _ => None,
        };

        if let Some(content) = yaml_content {
            content.to_string()
        } else {
            // Use default YAML template and substitute the target
            let default_template = include_str!("../../configs/default.yaml");
            default_template.replace("{target}", target)
        }
    }

    /// Checks if a reference exists in the repository.
    ///
    /// # Arguments
    /// * `reference_name` - The name of the reference to check
    ///
    /// # Returns
    /// * `Ok(true)` if the reference exists
    /// * `Ok(false)` if the reference doesn't exist
    /// * `Err` if there was an error checking
    async fn reference_exists(&self, reference_name: &str) -> Result<bool> {
        let owner = self.get_repo_owner();
        let name = self.get_repo_name();
        let git_ref = self.get_git_ref();
        let url = format!(
            "{GITHUB_API_BASE}/repos/{owner}/{name}/contents/{REFERENCES_PATH}/{reference_name}?ref={git_ref}"
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

    /// Downloads the avocado.yaml file from a reference and returns its content.
    ///
    /// # Arguments
    /// * `reference_name` - The name of the reference folder
    ///
    /// # Returns
    /// * `Ok(String)` with the file content if successful
    /// * `Err` if the file doesn't exist or cannot be downloaded
    async fn download_reference_config(&self, reference_name: &str) -> Result<String> {
        let owner = self.get_repo_owner();
        let name = self.get_repo_name();
        let git_ref = self.get_git_ref();
        let url = format!(
            "{GITHUB_API_BASE}/repos/{owner}/{name}/contents/{REFERENCES_PATH}/{reference_name}/avocado.yaml?ref={git_ref}"
        );

        let client = reqwest::Client::builder()
            .user_agent("avocado-cli")
            .build()?;

        let response = client.get(&url).send().await.with_context(|| {
            format!("Failed to fetch avocado.yaml from reference '{reference_name}'")
        })?;

        if !response.status().is_success() {
            anyhow::bail!("Reference '{reference_name}' does not contain an avocado.yaml file");
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
                .with_context(|| "Failed to download avocado.yaml")?;

            let file_content = file_response
                .text()
                .await
                .with_context(|| "Failed to read avocado.yaml content")?;

            Ok(file_content)
        } else {
            anyhow::bail!("Could not get download URL for avocado.yaml");
        }
    }

    /// Checks if a target is supported in the given TOML content.
    ///
    /// # Arguments
    /// * `toml_content` - The content of the avocado.yaml file
    /// * `target` - The target to check for
    ///
    /// # Returns
    /// * `Ok(true)` if the target is supported or if supported_targets contains "*"
    /// * `Ok(false)` if the target is not supported
    /// * `Err` if the TOML cannot be parsed or doesn't have supported_targets
    fn is_target_supported(toml_content: &str, target: &str) -> Result<bool> {
        let config: toml::Value =
            toml::from_str(toml_content).with_context(|| "Failed to parse avocado.yaml")?;

        let supported_targets_value = config.get("supported_targets").ok_or_else(|| {
            anyhow::anyhow!("Reference avocado.yaml missing 'supported_targets' field")
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

    /// Updates the default_target in the avocado.yaml file.
    ///
    /// # Arguments
    /// * `toml_path` - Path to the avocado.yaml file
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
            toml::from_str(&content).with_context(|| "Failed to parse avocado.yaml")?;

        // Update the default_target field
        if let Some(table) = config.as_table_mut() {
            table.insert(
                "default_target".to_string(),
                toml::Value::String(new_target.to_string()),
            );
        } else {
            anyhow::bail!("avocado.yaml is not a valid TOML table");
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
    /// * `repo_owner` - The repository owner
    /// * `repo_name` - The repository name
    /// * `git_ref` - The git ref (branch/commit) to fetch from
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if there was an error downloading the contents
    fn download_reference_contents<'a>(
        reference_name: &'a str,
        github_path: &'a str,
        local_base_path: &'a Path,
        repo_owner: &'a str,
        repo_name: &'a str,
        git_ref: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!(
                "{GITHUB_API_BASE}/repos/{repo_owner}/{repo_name}/contents/{REFERENCES_PATH}/{github_path}?ref={git_ref}"
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

                // Check if this is a submodule (can appear as type "submodule" or "file")
                if Self::is_submodule(&item, repo_owner, repo_name) {
                    // Try to get submodule info from submodule_git_url first, then fall back to git_url
                    let submodule_info = if let Some(ref submodule_url) = item.submodule_git_url {
                        item.sha.as_ref().and_then(|sha| {
                            Self::parse_github_url(submodule_url)
                                .map(|(owner, repo)| (owner, repo, sha.clone()))
                        })
                    } else {
                        // Parse from git_url (format: https://api.github.com/repos/{owner}/{repo}/git/trees/{sha})
                        item.git_url
                            .as_ref()
                            .and_then(|url| Self::parse_git_api_url(url))
                    };

                    if let Some((sub_owner, sub_repo, sha)) = submodule_info {
                        println!("  Downloading submodule {relative_path} from {sub_owner}/{sub_repo}@{sha}...");
                        fs::create_dir_all(&local_path).with_context(|| {
                            format!("Failed to create directory '{}'", local_path.display())
                        })?;
                        Self::download_submodule_contents(
                            &sub_owner,
                            &sub_repo,
                            &sha,
                            "",
                            &local_path,
                        )
                        .await?;
                    } else {
                        println!(
                            "  Warning: Submodule '{relative_path}' missing required info, skipping"
                        );
                    }
                } else {
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
                                repo_owner,
                                repo_name,
                                git_ref,
                            )
                            .await?;
                        }
                        _ => {
                            // Skip other types (symlinks, etc.)
                        }
                    }
                }
            }

            Ok(())
        })
    }

    /// Parses a GitHub URL to extract owner and repo name.
    ///
    /// Supports formats:
    /// - https://github.com/owner/repo.git
    /// - https://github.com/owner/repo
    /// - git://github.com/owner/repo.git
    /// - git@github.com:owner/repo.git
    ///
    /// # Arguments
    /// * `url` - The GitHub URL to parse
    ///
    /// # Returns
    /// * `Some((owner, repo))` if parsing succeeded
    /// * `None` if the URL format is not recognized
    fn parse_github_url(url: &str) -> Option<(String, String)> {
        // Handle git@github.com:owner/repo.git format
        if url.starts_with("git@github.com:") {
            let path = url.strip_prefix("git@github.com:")?;
            let path = path.strip_suffix(".git").unwrap_or(path);
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 2 {
                return Some((parts[0].to_string(), parts[1].to_string()));
            }
            return None;
        }

        // Handle https:// and git:// URLs
        let url = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("git://github.com/"))?;
        let url = url.strip_suffix(".git").unwrap_or(url);
        let parts: Vec<&str> = url.split('/').collect();
        if parts.len() >= 2 {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    }

    /// Parses a GitHub API git_url to extract owner, repo, and SHA.
    ///
    /// The git_url for submodules has the format:
    /// https://api.github.com/repos/{owner}/{repo}/git/trees/{sha}
    ///
    /// # Arguments
    /// * `git_url` - The GitHub API git URL
    ///
    /// # Returns
    /// * `Some((owner, repo, sha))` if parsing succeeded
    /// * `None` if the URL format is not recognized
    fn parse_git_api_url(git_url: &str) -> Option<(String, String, String)> {
        // Format: https://api.github.com/repos/{owner}/{repo}/git/trees/{sha}
        let url = git_url.strip_prefix("https://api.github.com/repos/")?;
        let parts: Vec<&str> = url.split('/').collect();
        // Expected: [owner, repo, "git", "trees", sha]
        if parts.len() >= 5 && parts[2] == "git" && parts[3] == "trees" {
            Some((
                parts[0].to_string(),
                parts[1].to_string(),
                parts[4].to_string(),
            ))
        } else {
            None
        }
    }

    /// Checks if a GitHubContent item is a submodule.
    ///
    /// Submodules in GitHub's API can appear as either:
    /// - type: "submodule" with submodule_git_url
    /// - type: "file" with size: 0, download_url: null, and git_url pointing to another repo's tree
    fn is_submodule(item: &GitHubContent, current_owner: &str, current_repo: &str) -> bool {
        if item.item_type == "submodule" {
            return true;
        }

        // Check for submodules that appear as files
        if item.item_type == "file" && item.download_url.is_none() && item.size == Some(0) {
            // Check if git_url points to a different repository
            if let Some(ref git_url) = item.git_url {
                if let Some((owner, repo, _)) = Self::parse_git_api_url(git_url) {
                    // If the git_url points to a different repo, it's a submodule
                    return owner != current_owner || repo != current_repo;
                }
            }
        }

        false
    }

    /// Recursively downloads all contents from a GitHub submodule at a specific commit.
    ///
    /// # Arguments
    /// * `repo_owner` - The submodule repository owner
    /// * `repo_name` - The submodule repository name
    /// * `git_ref` - The git ref (commit SHA) to fetch from
    /// * `path` - The path within the submodule repository (empty for root)
    /// * `local_path` - The local path to download to
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err` if there was an error downloading the contents
    fn download_submodule_contents<'a>(
        repo_owner: &'a str,
        repo_name: &'a str,
        git_ref: &'a str,
        path: &'a str,
        local_path: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let url = if path.is_empty() {
                format!("{GITHUB_API_BASE}/repos/{repo_owner}/{repo_name}/contents?ref={git_ref}")
            } else {
                format!(
                    "{GITHUB_API_BASE}/repos/{repo_owner}/{repo_name}/contents/{path}?ref={git_ref}"
                )
            };

            let client = reqwest::Client::builder()
                .user_agent("avocado-cli")
                .build()?;

            let response = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("Failed to fetch submodule contents from {url}"))?;

            if !response.status().is_success() {
                anyhow::bail!(
                    "Failed to fetch submodule contents: HTTP {}",
                    response.status()
                );
            }

            let contents: Vec<GitHubContent> = response
                .json()
                .await
                .with_context(|| "Failed to parse GitHub API response for submodule")?;

            for item in contents {
                let item_name = item.path.rsplit('/').next().unwrap_or(&item.path);
                let item_local_path = local_path.join(item_name);

                // Check if this is a submodule (can appear as type "submodule" or "file")
                if Self::is_submodule(&item, repo_owner, repo_name) {
                    // Try to get submodule info from submodule_git_url first, then fall back to git_url
                    let submodule_info = if let Some(ref submodule_url) = item.submodule_git_url {
                        item.sha.as_ref().and_then(|sha| {
                            Self::parse_github_url(submodule_url)
                                .map(|(owner, repo)| (owner, repo, sha.clone()))
                        })
                    } else {
                        // Parse from git_url (format: https://api.github.com/repos/{owner}/{repo}/git/trees/{sha})
                        item.git_url
                            .as_ref()
                            .and_then(|url| Self::parse_git_api_url(url))
                    };

                    if let Some((sub_owner, sub_repo, sha)) = submodule_info {
                        println!("    Downloading nested submodule {item_name} from {sub_owner}/{sub_repo}@{sha}...");
                        fs::create_dir_all(&item_local_path).with_context(|| {
                            format!("Failed to create directory '{}'", item_local_path.display())
                        })?;
                        Self::download_submodule_contents(
                            &sub_owner,
                            &sub_repo,
                            &sha,
                            "",
                            &item_local_path,
                        )
                        .await?;
                    }
                } else {
                    match item.item_type.as_str() {
                        "file" => {
                            if let Some(ref download_url) = item.download_url {
                                Self::download_file(download_url, &item_local_path).await?;
                            }
                        }
                        "dir" => {
                            fs::create_dir_all(&item_local_path).with_context(|| {
                                format!(
                                    "Failed to create directory '{}'",
                                    item_local_path.display()
                                )
                            })?;
                            Self::download_submodule_contents(
                                repo_owner,
                                repo_name,
                                git_ref,
                                &item.path,
                                &item_local_path,
                            )
                            .await?;
                        }
                        _ => {
                            // Skip other types
                        }
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

    /// Executes the init command, creating the avocado.yaml configuration file.
    ///
    /// # Returns
    /// * `Ok(())` if the initialization was successful
    /// * `Err` if there was an error during initialization
    ///
    /// # Errors
    /// This function will return an error if:
    /// * The target directory cannot be created
    /// * The avocado.yaml file already exists
    /// * The configuration file cannot be written
    /// * The reference doesn't exist (when using --reference)
    /// * There was an error downloading reference contents
    pub async fn execute(&self) -> Result<()> {
        let directory = self.directory.as_deref().unwrap_or(".");

        // Validate mutually exclusive options
        if self.reference_branch.is_some() && self.reference_commit.is_some() {
            anyhow::bail!(
                "Cannot specify both --reference-branch and --reference-commit. Please use only one."
            );
        }

        // Validate and create directory if it doesn't exist
        if !Path::new(directory).exists() {
            fs::create_dir_all(directory)
                .with_context(|| format!("Failed to create directory '{directory}'"))?;
        }

        // If reference is specified, download the reference project
        if let Some(ref_name) = &self.reference {
            let reference_source = self.get_reference_source();
            println!("Initializing from reference '{ref_name}'...");

            // Print source info if using non-default values
            if self.reference_repo.is_some()
                || self.reference_branch.is_some()
                || self.reference_commit.is_some()
            {
                println!("Using source: {reference_source}");
            }

            // Check if reference exists
            println!("Checking if reference '{ref_name}' exists...");
            if !self.reference_exists(ref_name).await? {
                anyhow::bail!(
                    "Reference '{ref_name}' not found in {reference_source}. \
                    Please check the available references at https://github.com/{}/{}/tree/{}",
                    self.get_repo_owner(),
                    self.get_repo_name(),
                    self.get_git_ref()
                );
            }

            // If both reference and target are specified, validate target support
            if let Some(target) = &self.target {
                println!("Validating target '{target}' is supported by reference '{ref_name}'...");

                // Download and parse the reference's avocado.yaml
                let toml_content = self.download_reference_config(ref_name).await?;

                // Check if target is supported
                if !Self::is_target_supported(&toml_content, target)? {
                    anyhow::bail!(
                        "Target '{target}' is not supported by reference '{ref_name}'. \
                        Please check the reference's avocado.yaml for supported_targets."
                    );
                }

                println!("✓ Target '{target}' is supported by reference '{ref_name}'.");
            }

            // Download all contents from the reference
            println!("Downloading reference contents...");
            Self::download_reference_contents(
                ref_name,
                ref_name,
                Path::new(directory),
                self.get_repo_owner(),
                self.get_repo_name(),
                self.get_git_ref(),
            )
            .await?;

            // If a target was specified, update the default_target in the downloaded avocado.yaml
            if let Some(target) = &self.target {
                let toml_path = Path::new(directory).join("avocado.yaml");
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
            // Original behavior: create avocado.yaml from template
            let target = self
                .target
                .as_deref()
                .unwrap_or_else(|| Self::get_default_target());

            // Create the avocado.yaml file path
            let toml_path = Path::new(directory).join("avocado.yaml");

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

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None, None, None, None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.yaml");
        assert!(config_path.exists());

        let content = fs::read_to_string(&config_path).unwrap();
        let expected_target = InitCommand::get_default_target();
        assert!(content.contains(&format!("default_target: \"{expected_target}\"")));
        assert!(content.contains("distro:"));
        assert!(content.contains("channel: apollo-edge"));
        assert!(content.contains("version: 0.1.0"));
        assert!(content.contains("runtime:"));
        assert!(content.contains("dev:"));
        assert!(content.contains("dependencies:"));
        assert!(content.contains("avocado-img-bootfiles: \"*\""));
        assert!(content.contains("avocado-img-rootfs: \"*\""));
        assert!(content.contains("avocado-img-initramfs: \"*\""));
        assert!(content.contains("avocado-ext-dev:"));
        assert!(content.contains("ext: avocado-ext-dev"));
        assert!(content.contains("vsn: \"*\""));
        assert!(
            content.contains("image: \"docker.io/avocadolinux/sdk:{{ config.distro.channel }}\"")
        );
        assert!(content.contains("ext:"));
        assert!(content.contains("app:"));
        assert!(content.contains("- sysext"));
        assert!(content.contains("- confext"));
        assert!(content.contains("config:"));
        assert!(content.contains("avocado-sdk-toolchain: \"{{ config.distro.version }}\""));
        assert!(content.contains("nativesdk-avocado-ext-dev: \"*\""));
    }

    #[tokio::test]
    async fn test_init_custom_target() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(
            Some("custom-arch".to_string()),
            Some(temp_path.to_string()),
            None,
            None,
            None,
            None,
        );
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.yaml");
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("default_target: \"custom-arch\""));
    }

    #[tokio::test]
    async fn test_init_file_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let config_path = PathBuf::from(temp_path).join("avocado.yaml");

        // Create existing file
        fs::write(&config_path, "existing content").unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None, None, None, None);
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

        let init_cmd =
            InitCommand::new(None, Some(new_dir_str.to_string()), None, None, None, None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());
        assert!(new_dir_path.exists());

        let config_path = new_dir_path.join("avocado.yaml");
        assert!(config_path.exists());
    }

    #[tokio::test]
    async fn test_init_creates_gitignore() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None, None, None, None);
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

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None, None, None, None);
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

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()), None, None, None, None);
        let result = init_cmd.execute().await;

        assert!(result.is_ok());

        let content = fs::read_to_string(&gitignore_path).unwrap();
        // Count occurrences of .avocado-state - should be exactly 1
        let count = content.matches(".avocado-state").count();
        assert_eq!(count, 1);
    }

    /// Helper function to validate that all [ext.<name>] blocks have a version field
    fn validate_ext_versions(config_content: &str, config_name: &str) {
        let config: serde_yaml::Value = serde_yaml::from_str(config_content)
            .unwrap_or_else(|e| panic!("Failed to parse {config_name} config: {e}"));

        // YAML parses ext.app as a nested structure: ext -> app
        // So we need to look for the "ext" key and then check its children
        if let Some(mapping) = config.as_mapping() {
            if let Some(ext_value) = mapping.get(serde_yaml::Value::String("ext".to_string())) {
                if let Some(ext_table) = ext_value.as_mapping() {
                    // Now iterate through each extension (app, config, etc.)
                    for (ext_name_val, ext_config) in ext_table {
                        let ext_name = ext_name_val.as_str().unwrap_or("");
                        if let Some(ext_config_table) = ext_config.as_mapping() {
                            assert!(
                                ext_config_table.contains_key(serde_yaml::Value::String("version".to_string())),
                                "Config '{config_name}' has ext.{ext_name} block without 'version' field. All extension blocks must have a version field."
                            );

                            // Validate that version is a string
                            assert!(
                                ext_config_table
                                    .get(serde_yaml::Value::String("version".to_string()))
                                    .and_then(|v| v.as_str())
                                    .is_some(),
                                "Config '{config_name}' has ext.{ext_name} block with non-string 'version' field"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_all_config_templates_have_ext_versions() {
        // Test default template (substitute {target} placeholder for validation)
        let default_template =
            include_str!("../../configs/default.yaml").replace("{target}", "test-target");
        validate_ext_versions(&default_template, "default.yaml");

        // Test reterminal
        let reterminal = include_str!("../../configs/seeed/reterminal.yaml");
        validate_ext_versions(reterminal, "reterminal.yaml");

        // Test reterminal-dm
        let reterminal_dm = include_str!("../../configs/seeed/reterminal-dm.yaml");
        validate_ext_versions(reterminal_dm, "reterminal-dm.yaml");

        // Test jetson-orin-nano-devkit
        let jetson = include_str!("../../configs/nvidia/jetson-orin-nano-devkit.yaml");
        validate_ext_versions(jetson, "jetson-orin-nano-devkit.yaml");

        // Test raspberrypi4
        let rpi4 = include_str!("../../configs/raspberry-pi/raspberrypi-4-model-b.yaml");
        validate_ext_versions(rpi4, "raspberrypi-4-model-b.yaml");

        // Test raspberrypi5
        let rpi5 = include_str!("../../configs/raspberry-pi/raspberrypi-5.yaml");
        validate_ext_versions(rpi5, "raspberrypi-5.yaml");

        // Test icam-540
        let icam = include_str!("../../configs/advantech/icam-540.yaml");
        validate_ext_versions(icam, "icam-540.yaml");
    }

    #[tokio::test]
    async fn test_generated_configs_have_ext_versions() {
        // Test that configs generated for all supported targets have versions
        let targets = vec![
            "qemux86-64",
            "qemuarm64",
            "reterminal",
            "reterminal-dm",
            "jetson-orin-nano-devkit",
            "raspberrypi4",
            "raspberrypi5",
            "icam-540",
            "custom-target", // This uses the default template
        ];

        for target in targets {
            let temp_dir = TempDir::new().unwrap();
            let temp_path = temp_dir.path().to_str().unwrap();

            let init_cmd = InitCommand::new(
                Some(target.to_string()),
                Some(temp_path.to_string()),
                None,
                None,
                None,
                None,
            );
            let result = init_cmd.execute().await;

            assert!(
                result.is_ok(),
                "Failed to initialize config for target '{}': {:?}",
                target,
                result.err()
            );

            let config_path = PathBuf::from(temp_path).join("avocado.yaml");
            let content = fs::read_to_string(&config_path).unwrap_or_else(|e| {
                panic!("Failed to read generated config for target '{target}': {e}")
            });

            validate_ext_versions(&content, &format!("generated config for {target}"));
        }
    }

    #[test]
    fn test_parse_github_url() {
        // Test https://github.com/owner/repo.git format
        assert_eq!(
            InitCommand::parse_github_url("https://github.com/avocado-linux/avocado-os.git"),
            Some(("avocado-linux".to_string(), "avocado-os".to_string()))
        );

        // Test https://github.com/owner/repo format (no .git suffix)
        assert_eq!(
            InitCommand::parse_github_url("https://github.com/avocado-linux/avocado-os"),
            Some(("avocado-linux".to_string(), "avocado-os".to_string()))
        );

        // Test git://github.com/owner/repo.git format
        assert_eq!(
            InitCommand::parse_github_url("git://github.com/avocado-linux/avocado-os.git"),
            Some(("avocado-linux".to_string(), "avocado-os".to_string()))
        );

        // Test git@github.com:owner/repo.git format (SSH)
        assert_eq!(
            InitCommand::parse_github_url("git@github.com:avocado-linux/avocado-os.git"),
            Some(("avocado-linux".to_string(), "avocado-os".to_string()))
        );

        // Test invalid URLs
        assert_eq!(
            InitCommand::parse_github_url("https://gitlab.com/avocado-linux/avocado-os.git"),
            None
        );
        assert_eq!(InitCommand::parse_github_url("invalid-url"), None);
    }

    #[test]
    fn test_parse_git_api_url() {
        // Test valid git API URL format
        assert_eq!(
            InitCommand::parse_git_api_url(
                "https://api.github.com/repos/avocado-linux/ref-rust/git/trees/b075804a37c196f6551d8a497d316ddb61dc44cb"
            ),
            Some((
                "avocado-linux".to_string(),
                "ref-rust".to_string(),
                "b075804a37c196f6551d8a497d316ddb61dc44cb".to_string()
            ))
        );

        // Test another valid URL
        assert_eq!(
            InitCommand::parse_git_api_url(
                "https://api.github.com/repos/owner/repo/git/trees/abc123"
            ),
            Some((
                "owner".to_string(),
                "repo".to_string(),
                "abc123".to_string()
            ))
        );

        // Test invalid URLs
        assert_eq!(
            InitCommand::parse_git_api_url("https://api.github.com/repos/owner/repo/contents/file"),
            None
        );
        assert_eq!(InitCommand::parse_git_api_url("invalid-url"), None);
    }

    #[test]
    fn test_is_submodule() {
        // Test explicit submodule type
        let submodule_item = GitHubContent {
            path: "submodule".to_string(),
            item_type: "submodule".to_string(),
            download_url: None,
            sha: Some("abc123".to_string()),
            submodule_git_url: Some("https://github.com/owner/repo.git".to_string()),
            size: Some(0),
            git_url: None,
        };
        assert!(InitCommand::is_submodule(
            &submodule_item,
            "current-owner",
            "current-repo"
        ));

        // Test submodule appearing as file (size 0, no download_url, git_url points to different repo)
        let submodule_as_file = GitHubContent {
            path: "ref-rust".to_string(),
            item_type: "file".to_string(),
            download_url: None,
            sha: Some("b075804a37c196f6551d8a497d316ddb61dc44cb".to_string()),
            submodule_git_url: None,
            size: Some(0),
            git_url: Some(
                "https://api.github.com/repos/avocado-linux/ref-rust/git/trees/b075804a37c196f6551d8a497d316ddb61dc44cb".to_string()
            ),
        };
        assert!(InitCommand::is_submodule(
            &submodule_as_file,
            "avocado-linux",
            "avocado-os"
        ));

        // Test regular file (not a submodule)
        let regular_file = GitHubContent {
            path: "file.txt".to_string(),
            item_type: "file".to_string(),
            download_url: Some("https://example.com/file.txt".to_string()),
            sha: Some("abc123".to_string()),
            submodule_git_url: None,
            size: Some(100),
            git_url: Some(
                "https://api.github.com/repos/avocado-linux/avocado-os/git/blobs/abc123"
                    .to_string(),
            ),
        };
        assert!(!InitCommand::is_submodule(
            &regular_file,
            "avocado-linux",
            "avocado-os"
        ));

        // Test file in same repo (not a submodule even if size is 0)
        let same_repo_file = GitHubContent {
            path: "empty-file".to_string(),
            item_type: "file".to_string(),
            download_url: None,
            sha: Some("abc123".to_string()),
            submodule_git_url: None,
            size: Some(0),
            git_url: Some(
                "https://api.github.com/repos/avocado-linux/avocado-os/git/trees/abc123"
                    .to_string(),
            ),
        };
        assert!(!InitCommand::is_submodule(
            &same_repo_file,
            "avocado-linux",
            "avocado-os"
        ));
    }
}
