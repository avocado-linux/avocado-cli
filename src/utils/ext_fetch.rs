//! Extension fetching utilities for remote extensions.
//!
//! This module provides functionality to fetch extensions from various sources:
//! - Package repository (avocado extension repo)
//! - Git repositories (with optional sparse checkout)
//! - Local filesystem paths

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::utils::config::ExtensionSource;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, OutputLevel};

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
            ExtensionSource::Repo { version, repo_name } => {
                self.fetch_from_repo(ext_name, version, repo_name.as_deref(), &ext_install_path)
                    .await?;
            }
            ExtensionSource::Git {
                url,
                git_ref,
                sparse_checkout,
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
            ExtensionSource::Path { path } => {
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
        repo_name: Option<&str>,
        install_path: &Path,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!(
                    "Fetching extension '{ext_name}' version '{version}' from package repository"
                ),
                OutputLevel::Normal,
            );
        }

        // Create the install directory
        std::fs::create_dir_all(install_path).with_context(|| {
            format!(
                "Failed to create extension directory: {}",
                install_path.display()
            )
        })?;

        // Build the package spec
        let package_spec = if version == "*" {
            ext_name.to_string()
        } else {
            format!("{ext_name}-{version}")
        };

        // Build the DNF command to download and extract the package
        // We use --downloadonly and then extract the RPM contents
        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();

        let install_path_str = install_path.to_string_lossy();

        // The fetch script downloads the package and extracts it to the install path
        let fetch_script = format!(
            r#"
set -e

# Create temp directory for download
TMPDIR=$(mktemp -d)
cd "$TMPDIR"

# Download the extension package
dnf download {repo_arg} --destdir="$TMPDIR" {package_spec}

# Find the downloaded RPM
RPM_FILE=$(ls -1 *.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    echo "ERROR: Failed to download extension package '{ext_name}'"
    exit 1
fi

# Extract RPM contents to install path
# The package root / maps to the extension's src_dir
mkdir -p "{install_path_str}"
cd "{install_path_str}"
rpm2cpio "$TMPDIR/$RPM_FILE" | cpio -idmv

echo "Successfully fetched extension '{ext_name}' to {install_path_str}"

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
        install_path: &Path,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Fetching extension '{ext_name}' from git: {url}"),
                OutputLevel::Normal,
            );
        }

        // Create parent directory
        if let Some(parent) = install_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory: {}", parent.display())
            })?;
        }

        let install_path_str = install_path.to_string_lossy();
        let ref_arg = git_ref.unwrap_or("HEAD");

        // Build the git clone command
        let git_cmd = if let Some(sparse_paths) = sparse_checkout {
            // Use sparse checkout for specific paths
            let sparse_paths_str = sparse_paths.join(" ");
            format!(
                r#"
set -e
rm -rf "{install_path_str}"
mkdir -p "{install_path_str}"
cd "{install_path_str}"
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
rm -rf "{install_path_str}"
git clone --depth 1 --branch {ref_arg} "{url}" "{install_path_str}" || \
git clone --depth 1 "{url}" "{install_path_str}"
cd "{install_path_str}"
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
    async fn fetch_from_path(
        &self,
        ext_name: &str,
        source_path: &str,
        install_path: &Path,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Fetching extension '{ext_name}' from path: {source_path}"),
                OutputLevel::Normal,
            );
        }

        // Resolve the source path relative to the config file
        let config_dir = Path::new(&self.config_path)
            .parent()
            .unwrap_or(Path::new("."));
        let resolved_source = if Path::new(source_path).is_absolute() {
            PathBuf::from(source_path)
        } else {
            config_dir.join(source_path)
        };

        if !resolved_source.exists() {
            return Err(anyhow::anyhow!(
                "Extension source path does not exist: {}",
                resolved_source.display()
            ));
        }

        // Create the install directory
        if let Some(parent) = install_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory: {}", parent.display())
            })?;
        }

        // Remove existing install path if it exists
        if install_path.exists() {
            std::fs::remove_dir_all(install_path).with_context(|| {
                format!(
                    "Failed to remove existing directory: {}",
                    install_path.display()
                )
            })?;
        }

        // Copy the directory (or create symlink for efficiency)
        // For now, we'll copy to ensure isolation
        Self::copy_dir_recursive(&resolved_source, install_path)?;

        if self.verbose {
            print_info(
                &format!(
                    "Successfully copied extension '{ext_name}' from {} to {}",
                    resolved_source.display(),
                    install_path.display()
                ),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Recursively copy a directory
    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;

        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
                Self::copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }

        Ok(())
    }

    /// Check if an extension is already fetched/installed
    pub fn is_extension_installed(install_dir: &Path, ext_name: &str) -> bool {
        let ext_path = install_dir.join(ext_name);
        // Check if the directory exists and has an avocado config file
        ext_path.exists()
            && (ext_path.join("avocado.yaml").exists()
                || ext_path.join("avocado.yml").exists()
                || ext_path.join("avocado.toml").exists())
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
