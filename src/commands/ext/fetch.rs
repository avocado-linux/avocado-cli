//! Extension fetch command implementation.
//!
//! This command fetches remote extensions from various sources (repo, git, path)
//! and installs them to `$AVOCADO_PREFIX/includes/<ext_name>/`.

use anyhow::{Context, Result};

use crate::utils::config::{Config, ExtensionSource};
use crate::utils::ext_fetch::ExtensionFetcher;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

/// Command to fetch remote extensions
pub struct ExtFetchCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Specific extension to fetch (if None, fetches all remote extensions)
    pub extension: Option<String>,
    /// Enable verbose output
    pub verbose: bool,
    /// Force re-fetch even if already installed
    pub force: bool,
    /// Target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Run command on remote host
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
}

impl ExtFetchCommand {
    /// Create a new ExtFetchCommand instance
    pub fn new(
        config_path: String,
        extension: Option<String>,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            extension,
            verbose,
            force,
            target,
            container_args,
            sdk_arch: None,
            runs_on: None,
            nfs_port: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set remote execution host and NFS port
    pub fn with_runs_on(mut self, runs_on: String, nfs_port: Option<u16>) -> Self {
        self.runs_on = Some(runs_on);
        self.nfs_port = nfs_port;
        self
    }

    /// Execute the fetch command
    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // Resolve target
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Get container image
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration"))?;

        // Discover remote extensions (with target interpolation for extension names)
        let remote_extensions =
            Config::discover_remote_extensions(&self.config_path, Some(&target))?;

        if remote_extensions.is_empty() {
            print_info(
                "No remote extensions found in configuration.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Filter to specific extension if requested
        let extensions_to_fetch: Vec<(String, ExtensionSource)> =
            if let Some(ref ext_name) = self.extension {
                remote_extensions
                    .into_iter()
                    .filter(|(name, _)| name == ext_name)
                    .collect()
            } else {
                remote_extensions
            };

        if extensions_to_fetch.is_empty() {
            if let Some(ref ext_name) = self.extension {
                return Err(anyhow::anyhow!(
                    "Extension '{ext_name}' not found in configuration or is not a remote extension"
                ));
            }
            return Ok(());
        }

        // Get the extensions install directory (container path)
        // The directory will be created inside the container, not on the host
        let extensions_dir = config.get_extensions_dir(&self.config_path, &target);

        if self.verbose {
            print_info(
                &format!(
                    "Fetching {} remote extension(s) to {}",
                    extensions_to_fetch.len(),
                    extensions_dir.display()
                ),
                OutputLevel::Normal,
            );
        }

        // Create the fetcher
        // If container_args were already passed (e.g., from sdk install), use them directly
        // Otherwise, merge from config
        let effective_container_args = if self.container_args.is_some() {
            self.container_args.clone()
        } else {
            config.merge_sdk_container_args(None)
        };

        // Get the resolved src_dir for resolving relative extension paths
        let src_dir = config.get_resolved_src_dir(&self.config_path);

        let fetcher = ExtensionFetcher::new(
            self.config_path.clone(),
            target.clone(),
            container_image.to_string(),
            self.verbose,
        )
        .with_repo_url(config.get_sdk_repo_url())
        .with_repo_release(config.get_sdk_repo_release())
        .with_container_args(effective_container_args)
        .with_sdk_arch(self.sdk_arch.clone())
        .with_src_dir(src_dir);

        // Fetch each extension
        let mut fetched_count = 0;
        let mut skipped_count = 0;

        for (ext_name, source) in &extensions_to_fetch {
            // Check if already installed
            if !self.force && ExtensionFetcher::is_extension_installed(&extensions_dir, ext_name) {
                if self.verbose {
                    print_info(
                        &format!("Extension '{ext_name}' is already installed, skipping (use --force to re-fetch)"),
                        OutputLevel::Normal,
                    );
                }
                skipped_count += 1;
                continue;
            }

            print_info(
                &format!("Fetching extension '{ext_name}'..."),
                OutputLevel::Normal,
            );

            match fetcher.fetch(ext_name, source, &extensions_dir).await {
                Ok(install_path) => {
                    print_success(
                        &format!(
                            "Successfully fetched extension '{ext_name}' to {}",
                            install_path.display()
                        ),
                        OutputLevel::Normal,
                    );
                    fetched_count += 1;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to fetch extension '{ext_name}': {e}"
                    ));
                }
            }
        }

        // Summary
        if fetched_count > 0 || skipped_count > 0 {
            let mut summary_parts = Vec::new();
            if fetched_count > 0 {
                summary_parts.push(format!("{fetched_count} fetched"));
            }
            if skipped_count > 0 {
                summary_parts.push(format!("{skipped_count} skipped"));
            }
            print_info(
                &format!("Extension fetch complete: {}", summary_parts.join(", ")),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Get the list of remote extensions that would be fetched
    #[allow(dead_code)]
    pub fn get_remote_extensions(&self) -> Result<Vec<(String, ExtensionSource)>> {
        Config::discover_remote_extensions(&self.config_path, self.target.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ext_fetch_command_creation() {
        let cmd = ExtFetchCommand::new(
            "avocado.yaml".to_string(),
            Some("test-ext".to_string()),
            true,
            false,
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert_eq!(cmd.extension, Some("test-ext".to_string()));
        assert!(cmd.verbose);
        assert!(!cmd.force);
    }
}
