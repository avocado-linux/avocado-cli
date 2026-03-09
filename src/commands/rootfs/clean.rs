//! Rootfs clean command and shared clean logic.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    target::resolve_target_required,
};

/// Generate the shell command to clean a sysroot (rootfs or initramfs).
pub fn clean_sysroot_command(sysroot_dir: &str) -> String {
    format!(r#"rm -rf "$AVOCADO_PREFIX/{sysroot_dir}""#)
}

/// Implementation of the 'rootfs clean' command.
pub struct RootfsCleanCommand {
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
}

impl RootfsCleanCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            sdk_arch: None,
        }
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = Arc::new(
            Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                || format!("Failed to load composed config from {}", self.config_path),
            )?,
        );
        let config = &composed.config;
        let target = resolve_target_required(self.target.as_deref(), config)?;
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        print_info("Cleaning rootfs sysroot.", OutputLevel::Normal);

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_sysroot_command("rootfs"),
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: config.get_sdk_repo_url(),
            repo_release: config.get_sdk_repo_release(),
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if success {
            print_success("Cleaned rootfs sysroot.", OutputLevel::Normal);
        } else {
            print_error("Failed to clean rootfs sysroot.", OutputLevel::Normal);
            return Err(anyhow::anyhow!("Failed to clean rootfs sysroot"));
        }

        Ok(())
    }
}
