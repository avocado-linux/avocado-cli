//! Initramfs sysroot install command (delegates to shared sysroot install).

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    lockfile::{LockFile, SysrootType},
    output::{print_error, OutputLevel},
    runs_on::RunsOnContext,
    target::validate_and_log_target,
};

use crate::commands::rootfs::install::{
    install_sysroot, read_sysroot_install_stamp, SysrootInstallParams,
};

/// Implementation of the 'initramfs install' command.
pub struct InitramfsInstallCommand {
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    target_board: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    sdk_arch: Option<String>,
    composed_config: Option<Arc<ComposedConfig>>,
}

impl InitramfsInstallCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            target,
            target_board: None,
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set the CLI target board override
    pub fn with_target_board(mut self, target_board: Option<String>) -> Self {
        self.target_board = target_board;
        self
    }

    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    pub fn with_runs_on(mut self, runs_on: Option<String>, nfs_port: Option<u16>) -> Self {
        self.runs_on = runs_on;
        self.nfs_port = nfs_port;
        self
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed_with_board(
                    &self.config_path,
                    self.target.as_deref(),
                    self.target_board.as_deref(),
                )
                .with_context(|| {
                    format!("Failed to load composed config from {}", self.config_path)
                })?,
            ),
        };

        let config = &composed.config;
        let target = validate_and_log_target(self.target.as_deref(), config)?;
        // Apply the reproducible snapshot pin before any repo_release is read.
        crate::utils::snapshot::resolve_and_apply_for(config, &self.config_path, &target).await?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        let mut runs_on_context: Option<RunsOnContext> = if let Some(ref runs_on) = self.runs_on {
            Some(
                container_helper
                    .create_runs_on_context(runs_on, self.nfs_port, container_image, self.verbose)
                    .await?,
            )
        } else {
            None
        };

        let src_dir = &config.project_root(&self.config_path);
        let mut lock_file = LockFile::load(src_dir)?;

        let prefetched_stamp = read_sysroot_install_stamp(
            &SysrootType::Initramfs,
            self.no_stamps,
            &container_helper,
            RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            },
            runs_on_context.as_ref(),
        )
        .await;

        // Not `?`: the teardown block below is the only thing that tears down the
        // `runs_on` NFS server and remote mount, and an early return here would
        // skip it — the next `--runs-on` run picks a fresh nfs_port and stacks
        // another one on top. Carried into `result` and returned past teardown.
        let (result, pins_recorded) = match prefetched_stamp {
            Err(e) => (Err(e), false),
            Ok(prefetched_stamp) => {
                let mut params = SysrootInstallParams {
                    sysroot_type: SysrootType::Initramfs,
                    config,
                    lock_file: &mut lock_file,
                    src_dir,
                    container_helper: &container_helper,
                    container_image,
                    target: &target,
                    target_board: self.target_board.as_deref(),
                    repo_url: repo_url.as_deref(),
                    repo_release: repo_release.as_deref(),
                    merged_container_args: merged_container_args.clone(),
                    dnf_args: self.dnf_args.clone(),
                    verbose: self.verbose,
                    force: self.force,
                    runs_on_context: runs_on_context.as_ref(),
                    sdk_arch: self.sdk_arch.as_ref(),
                    no_stamps: self.no_stamps,
                    parsed: Some(&composed.merged_value),
                    prefetched_stamp,
                    tui_context: None,
                    pins_recorded: false,
                };
                let outcome = install_sysroot(&mut params).await;
                // Copied out before `params` drops: it holds the `&mut` on
                // `lock_file`, which the save below needs back.
                let pins_recorded = params.pins_recorded;
                (outcome, pins_recorded)
            }
        };

        // Persist the lockfile the install updated — `install_sysroot` leaves
        // saving to its caller. Folded into `result` rather than `?` so a save
        // failure still reaches the teardown below.
        //
        // Also saved when the install failed *after* recording pins: they
        // describe the packages that actually landed, and dropping them makes
        // the next run re-resolve the kernel against feed head with no
        // `prev_pinned_kver` to compare against. The install error is the one
        // returned — it is what the user has to act on.
        let result = match (result, pins_recorded) {
            (Ok(()), _) => lock_file.save(src_dir),
            (Err(e), true) => {
                if let Err(save_err) = lock_file.save(src_dir) {
                    print_error(
                        &format!("Failed to save lock file: {save_err}"),
                        OutputLevel::Normal,
                    );
                }
                Err(e)
            }
            (Err(e), false) => Err(e),
        };

        if let Some(ref mut context) = runs_on_context {
            if let Err(e) = context.teardown().await {
                print_error(
                    &format!("Warning: Failed to cleanup remote resources: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        result
    }
}
