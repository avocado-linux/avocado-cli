//! Extension fetching utilities for remote extensions.
//!
//! This module provides functionality to fetch extensions from various sources:
//! - Package repository (avocado extension repo)
//! - Git repositories (with optional sparse checkout)
//! - Local filesystem paths (mounted via bindfs at runtime)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::config::ExtensionSource;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, OutputLevel};

/// State for extension path mounts stored in .avocado/ext-paths.json
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ExtensionPathState {
    /// Map of extension name to host path for bindfs mounting
    pub path_mounts: HashMap<String, PathBuf>,
}

impl ExtensionPathState {
    /// Load extension path state from .avocado/ext-paths.json in the given directory
    pub fn load_from_dir(dir_path: &Path) -> Result<Option<Self>> {
        let state_file = dir_path.join(".avocado").join("ext-paths.json");

        if !state_file.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&state_file).with_context(|| {
            format!(
                "Failed to read extension path state file: {}",
                state_file.display()
            )
        })?;

        let state: Self = serde_json::from_str(&content).with_context(|| {
            format!(
                "Failed to parse extension path state file: {}",
                state_file.display()
            )
        })?;

        Ok(Some(state))
    }

    /// Save extension path state to .avocado/ext-paths.json in the given directory
    pub fn save_to_dir(&self, dir_path: &Path) -> Result<()> {
        let state_dir = dir_path.join(".avocado");
        fs::create_dir_all(&state_dir).with_context(|| {
            format!(
                "Failed to create .avocado directory: {}",
                state_dir.display()
            )
        })?;

        let state_file = state_dir.join("ext-paths.json");
        let content = serde_json::to_string_pretty(self)
            .with_context(|| "Failed to serialize extension path state".to_string())?;

        fs::write(&state_file, content).with_context(|| {
            format!(
                "Failed to write extension path state file: {}",
                state_file.display()
            )
        })?;

        Ok(())
    }

    /// Add a path mount for an extension
    pub fn add_path_mount(&mut self, ext_name: String, host_path: PathBuf) {
        self.path_mounts.insert(ext_name, host_path);
    }

    /// Remove a path mount for an extension
    #[allow(dead_code)]
    pub fn remove_path_mount(&mut self, ext_name: &str) {
        self.path_mounts.remove(ext_name);
    }

    /// Get the path mount for an extension
    #[allow(dead_code)]
    pub fn get_path_mount(&self, ext_name: &str) -> Option<&PathBuf> {
        self.path_mounts.get(ext_name)
    }
}

/// Extension fetcher for downloading and installing remote extensions
pub struct ExtensionFetcher {
    /// Path to the main configuration file
    config_path: String,
    /// Target architecture
    target: String,
    /// Enable verbose output
    verbose: bool,
    /// Container image for running fetch operations
    container_image: String,
    /// Repository URL for package fetching
    repo_url: Option<String>,
    /// Repository release for package fetching
    repo_release: Option<String>,
    /// Container arguments
    container_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    sdk_arch: Option<String>,
    /// Source directory for resolving relative extension paths
    src_dir: Option<PathBuf>,
}

impl ExtensionFetcher {
    /// Create a new ExtensionFetcher
    pub fn new(
        config_path: String,
        target: String,
        container_image: String,
        verbose: bool,
    ) -> Self {
        Self {
            config_path,
            target,
            verbose,
            container_image,
            repo_url: None,
            repo_release: None,
            container_args: None,
            sdk_arch: None,
            src_dir: None,
        }
    }

    /// Set repository URL
    pub fn with_repo_url(mut self, repo_url: Option<String>) -> Self {
        self.repo_url = repo_url;
        self
    }

    /// Set repository release
    pub fn with_repo_release(mut self, repo_release: Option<String>) -> Self {
        self.repo_release = repo_release;
        self
    }

    /// Set container arguments
    pub fn with_container_args(mut self, container_args: Option<Vec<String>>) -> Self {
        self.container_args = container_args;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set source directory for resolving relative extension paths
    pub fn with_src_dir(mut self, src_dir: Option<PathBuf>) -> Self {
        self.src_dir = src_dir;
        self
    }

    /// Fetch an extension based on its source configuration
    ///
    /// Returns the path where the extension was installed
    pub async fn fetch(
        &self,
        ext_name: &str,
        source: &ExtensionSource,
        install_dir: &Path,
    ) -> Result<PathBuf> {
        let ext_install_path = install_dir.join(ext_name);

        match source {
            ExtensionSource::Package {
                version,
                package,
                repo_name,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_repo(
                    ext_name,
                    version,
                    package.as_deref(),
                    repo_name.as_deref(),
                    &ext_install_path,
                )
                .await?;
            }
            ExtensionSource::Git {
                url,
                git_ref,
                sparse_checkout,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_git(
                    ext_name,
                    url,
                    git_ref.as_deref(),
                    sparse_checkout.as_deref(),
                    &ext_install_path,
                )
                .await?;
            }
            ExtensionSource::Path { path, .. } => {
                self.fetch_from_path(ext_name, path, &ext_install_path)
                    .await?;
            }
        }

        Ok(ext_install_path)
    }

    /// Fetch an extension from the avocado package repository
    async fn fetch_from_repo(
        &self,
        ext_name: &str,
        version: &str,
        package: Option<&str>,
        repo_name: Option<&str>,
        _install_path: &Path, // Host path - not used, we use container path instead
    ) -> Result<()> {
        // Use explicit package name if provided, otherwise fall back to extension name
        let package_name = package.unwrap_or(ext_name);

        if self.verbose {
            print_info(
                &format!(
                    "Fetching extension '{ext_name}' (package: '{package_name}') version '{version}' from package repository"
                ),
                OutputLevel::Normal,
            );
        }

        // Build the package spec using the package name (not extension name)
        let package_spec = if version == "*" {
            package_name.to_string()
        } else {
            format!("{package_name}-{version}")
        };

        // Build the DNF command to download and extract the package
        // We use --downloadonly and then extract the RPM contents
        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();

        // Use container path $AVOCADO_PREFIX/includes/<ext_name> instead of host path
        // This ensures the directory is created inside the container with proper permissions
        let container_install_path = format!("$AVOCADO_PREFIX/includes/{ext_name}");

        // The fetch script downloads the package and extracts it to the install path
        // Use $DNF_SDK_HOST with $DNF_SDK_COMBINED_REPO_CONF to access target-specific repos
        let fetch_script = format!(
            r#"
set -e

# Create temp directory for download
TMPDIR=$(mktemp -d)

# Download the extension package using SDK DNF with combined repo config
# This includes both SDK repos and target-specific repos (like $AVOCADO_TARGET-ext)
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    {repo_arg} \
    --downloadonly \
    --downloaddir="$TMPDIR" \
    -y \
    install \
    {package_spec}

# Find the downloaded RPM
RPM_FILE=$(ls -1 "$TMPDIR"/*.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    echo "ERROR: Failed to download package '{package_spec}' for extension '{ext_name}'"
    exit 1
fi

# Extract RPM contents to install path (using container path)
# The package root / maps to the extension's src_dir
mkdir -p "{container_install_path}"
cd "{container_install_path}"
rpm2cpio "$RPM_FILE" | cpio -idmv

echo "Successfully fetched extension '{ext_name}' (package: {package_spec}) to {container_install_path}"

# Cleanup
rm -rf "$TMPDIR"
"#
        );

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: fetch_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from package repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a git repository
    async fn fetch_from_git(
        &self,
        ext_name: &str,
        url: &str,
        git_ref: Option<&str>,
        sparse_checkout: Option<&[String]>,
        _install_path: &Path, // Host path - not used, we use container path instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Fetching extension '{ext_name}' from git: {url}"),
                OutputLevel::Normal,
            );
        }

        // Use container path $AVOCADO_PREFIX/includes/<ext_name> instead of host path
        let container_install_path = format!("$AVOCADO_PREFIX/includes/{ext_name}");
        let ref_arg = git_ref.unwrap_or("HEAD");

        // Build the git clone command
        let git_cmd = if let Some(sparse_paths) = sparse_checkout {
            // Use sparse checkout for specific paths
            let sparse_paths_str = sparse_paths.join(" ");
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
mkdir -p "{container_install_path}"
cd "{container_install_path}"
git init
git remote add origin "{url}"
git config core.sparseCheckout true
echo "{sparse_paths_str}" | tr ' ' '\n' > .git/info/sparse-checkout
git fetch --depth 1 origin {ref_arg}
git checkout FETCH_HEAD
# Move sparse checkout contents to root if needed
if [ -d "{sparse_paths_str}" ]; then
    mv {sparse_paths_str}/* . 2>/dev/null || true
    rm -rf {sparse_paths_str}
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        } else {
            // Full clone
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
git clone --depth 1 --branch {ref_arg} "{url}" "{container_install_path}" || \
git clone --depth 1 "{url}" "{container_install_path}"
cd "{container_install_path}"
if [ "{ref_arg}" != "HEAD" ]; then
    git checkout {ref_arg} 2>/dev/null || true
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        };

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: git_cmd,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from git repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a local filesystem path
    ///
    /// Instead of copying files, this validates the path exists and stores the
    /// mapping for bindfs mounting at container runtime. The extension source
    /// will be mounted at `/mnt/ext/<ext_name>` and bindfs'd to
    /// `$AVOCADO_PREFIX/includes/<ext_name>`.
    async fn fetch_from_path(
        &self,
        ext_name: &str,
        source_path: &str,
        _install_path: &Path, // Host path - not used, we use bindfs mounting instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Registering extension '{ext_name}' from path: {source_path}"),
                OutputLevel::Normal,
            );
        }

        // Resolve the source path relative to src_dir (or config dir if src_dir not set)
        let resolved_source = if Path::new(source_path).is_absolute() {
            PathBuf::from(source_path)
        } else {
            // Use src_dir if available, otherwise fall back to config directory
            if let Some(ref src_dir) = self.src_dir {
                src_dir.join(source_path)
            } else {
                let config_dir = Path::new(&self.config_path)
                    .parent()
                    .unwrap_or(Path::new("."));
                config_dir.join(source_path)
            }
        };

        // Canonicalize the path to get the absolute path
        let resolved_source = resolved_source.canonicalize().unwrap_or(resolved_source);

        if !resolved_source.exists() {
            return Err(anyhow::anyhow!(
                "Extension source path does not exist: {}\n\
                 Path was resolved relative to: {}",
                resolved_source.display(),
                self.src_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "config directory".to_string())
            ));
        }

        // Check that the path contains an avocado.yaml or avocado.yml file
        let has_config = resolved_source.join("avocado.yaml").exists()
            || resolved_source.join("avocado.yml").exists();
        if !has_config {
            return Err(anyhow::anyhow!(
                "Extension source path does not contain an avocado.yaml or avocado.yml file: {}",
                resolved_source.display()
            ));
        }

        // Get the state directory (src_dir or config dir)
        let state_dir = self.src_dir.clone().unwrap_or_else(|| {
            Path::new(&self.config_path)
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });

        // Load or create extension path state
        let mut state = ExtensionPathState::load_from_dir(&state_dir)?.unwrap_or_default();

        // Add the path mount for this extension
        state.add_path_mount(ext_name.to_string(), resolved_source.clone());

        // Save the state
        state.save_to_dir(&state_dir)?;

        if self.verbose {
            print_info(
                &format!(
                    "Registered extension '{ext_name}' for bindfs mounting from: {}",
                    resolved_source.display()
                ),
                OutputLevel::Normal,
            );
        }

        print_info(
            &format!(
                "Extension '{ext_name}' will be mounted via bindfs at runtime from: {}",
                resolved_source.display()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Check if an extension is already fetched/installed
    pub fn is_extension_installed(install_dir: &Path, ext_name: &str) -> bool {
        let ext_path = install_dir.join(ext_name);
        // Check if the directory exists and has an avocado config file
        ext_path.exists()
            && (ext_path.join("avocado.yaml").exists() || ext_path.join("avocado.yml").exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_fetcher_creation() {
        let fetcher = ExtensionFetcher::new(
            "avocado.yaml".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
            "docker.io/avocadolinux/sdk:latest".to_string(),
            false,
        );

        assert!(!fetcher.verbose);
        assert_eq!(fetcher.target, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_is_extension_installed() {
        // This would need a temp directory to test properly
        // For now just verify the function exists
        let result =
            ExtensionFetcher::is_extension_installed(Path::new("/nonexistent"), "test-ext");
        assert!(!result);
    }
}
