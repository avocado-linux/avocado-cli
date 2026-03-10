//! Rootfs sysroot install command and shared install logic for rootfs/initramfs.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    target::validate_and_log_target,
};

/// Parameters for the shared sysroot install function.
pub struct SysrootInstallParams<'a> {
    pub sysroot_type: SysrootType,
    pub config: &'a Config,
    pub lock_file: &'a mut LockFile,
    pub src_dir: &'a Path,
    pub container_helper: &'a SdkContainer,
    pub container_image: &'a str,
    pub target: &'a str,
    pub repo_url: Option<&'a str>,
    pub repo_release: Option<&'a str>,
    pub merged_container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub verbose: bool,
    pub force: bool,
    pub runs_on_context: Option<&'a RunsOnContext>,
    pub sdk_arch: Option<&'a String>,
}

/// Install a sysroot (rootfs or initramfs) via DNF into the SDK container volume.
///
/// This is the shared implementation used by both `avocado rootfs install`,
/// `avocado initramfs install`, and `avocado sdk install`.
pub async fn install_sysroot(params: &mut SysrootInstallParams<'_>) -> Result<()> {
    let (label, sysroot_dir, default_pkg) = match params.sysroot_type {
        SysrootType::Rootfs => ("rootfs", "rootfs", "avocado-pkg-rootfs"),
        SysrootType::Initramfs => ("initramfs", "initramfs", "avocado-pkg-initramfs"),
        _ => return Err(anyhow::anyhow!("Unsupported sysroot type for install")),
    };

    print_info(&format!("Installing {label} sysroot."), OutputLevel::Normal);

    // Get packages from config
    let packages = match params.sysroot_type {
        SysrootType::Rootfs => params.config.get_rootfs_packages(),
        SysrootType::Initramfs => params.config.get_initramfs_packages(),
        _ => unreachable!(),
    };

    // Build package specs for all configured packages
    let pkg_specs: Vec<String> = if packages.is_empty() {
        vec![build_package_spec_with_lock(
            params.lock_file,
            params.target,
            &params.sysroot_type,
            default_pkg,
            "*",
        )]
    } else {
        packages
            .iter()
            .map(|(name, version)| {
                let ver = version.as_str().unwrap_or("*");
                build_package_spec_with_lock(
                    params.lock_file,
                    params.target,
                    &params.sysroot_type,
                    name,
                    ver,
                )
            })
            .collect()
    };
    let pkg = pkg_specs.join(" ");
    // The first package name is used as the base for lock file queries
    let base_pkg = packages
        .keys()
        .next()
        .map(|s| s.as_str())
        .unwrap_or(default_pkg);

    let yes = if params.force { "-y" } else { "" };
    let dnf_args_str = if let Some(args) = &params.dnf_args {
        format!(" {} ", args.join(" "))
    } else {
        String::new()
    };

    let command = format!(
        r#"
# Create usrmerge symlinks before install so scriptlets (depmod, ldconfig) can
# resolve /lib/modules, /sbin, /bin paths within the sysroot
mkdir -p $AVOCADO_PREFIX/{sysroot_dir}/usr/bin $AVOCADO_PREFIX/{sysroot_dir}/usr/sbin $AVOCADO_PREFIX/{sysroot_dir}/usr/lib
ln -sfn usr/bin $AVOCADO_PREFIX/{sysroot_dir}/bin
ln -sfn usr/sbin $AVOCADO_PREFIX/{sysroot_dir}/sbin
ln -sfn usr/lib $AVOCADO_PREFIX/{sysroot_dir}/lib

RPM_NO_CHROOT_FOR_SCRIPTS=1 \
AVOCADO_EXT_INSTALLROOT=$AVOCADO_PREFIX/{sysroot_dir} \
AVOCADO_SYSROOT_SCRIPTS=1 \
PATH=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin:$PATH \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF \
    {dnf_args_str} {yes} --installroot $AVOCADO_PREFIX/{sysroot_dir} install {pkg}
"#
    );

    let mut run_config = RunConfig {
        container_image: params.container_image.to_string(),
        target: params.target.to_string(),
        command,
        verbose: params.verbose,
        source_environment: false,
        interactive: !params.force,
        repo_url: params.repo_url.map(|s| s.to_string()),
        repo_release: params.repo_release.map(|s| s.to_string()),
        container_args: params.merged_container_args.clone(),
        dnf_args: params.dnf_args.clone(),
        disable_weak_dependencies: params.config.get_sdk_disable_weak_dependencies(),
        ..Default::default()
    };

    // Inject sdk_arch if provided
    if let Some(arch) = params.sdk_arch {
        run_config.sdk_arch = Some(arch.clone());
    }

    let success = if let Some(context) = params.runs_on_context {
        params
            .container_helper
            .run_in_container_with_context(&run_config, context)
            .await?
    } else {
        params.container_helper.run_in_container(run_config).await?
    };

    if success {
        print_success(&format!("Installed {label} sysroot."), OutputLevel::Normal);

        // Query installed version and update lock file
        let installed_versions = params
            .container_helper
            .query_installed_packages(
                &params.sysroot_type,
                &[base_pkg.to_string()],
                params.container_image,
                params.target,
                params.repo_url.map(|s| s.to_string()),
                params.repo_release.map(|s| s.to_string()),
                params.merged_container_args.clone(),
                params.runs_on_context,
                params.sdk_arch,
            )
            .await?;

        if !installed_versions.is_empty() {
            params.lock_file.update_sysroot_versions(
                params.target,
                &params.sysroot_type,
                installed_versions,
            );
            if params.verbose {
                print_info(
                    &format!("Updated lock file with {label} package version."),
                    OutputLevel::Normal,
                );
            }
            params.lock_file.save(params.src_dir)?;
        }
    } else {
        return Err(anyhow::anyhow!("Failed to install {label} sysroot."));
    }

    Ok(())
}

/// Implementation of the 'rootfs install' command.
pub struct RootfsInstallCommand {
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    runs_on: Option<String>,
    nfs_port: Option<u16>,
    sdk_arch: Option<String>,
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RootfsInstallCommand {
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
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
        }
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
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };

        let config = &composed.config;
        let target = validate_and_log_target(self.target.as_deref(), config)?;
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

        let src_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let mut lock_file = LockFile::load(src_dir)?;

        let result = install_sysroot(&mut SysrootInstallParams {
            sysroot_type: SysrootType::Rootfs,
            config,
            lock_file: &mut lock_file,
            src_dir,
            container_helper: &container_helper,
            container_image,
            target: &target,
            repo_url: repo_url.as_deref(),
            repo_release: repo_release.as_deref(),
            merged_container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            verbose: self.verbose,
            force: self.force,
            runs_on_context: runs_on_context.as_ref(),
            sdk_arch: self.sdk_arch.as_ref(),
        })
        .await;

        // Always teardown runs_on context
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
