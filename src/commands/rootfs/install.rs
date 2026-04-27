//! Rootfs sysroot install command and shared install logic for rootfs/initramfs.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Parse the `overlay:` config value into `(dir, opaque)`.
/// Accepts either a plain string (`"path/to/dir"`) or a mapping
/// (`{ dir: "path/to/dir", mode: "opaque" | "merge" }`).
fn parse_overlay_config(value: &serde_yaml::Value) -> (String, bool) {
    if let Some(dir_str) = value.as_str() {
        (dir_str.to_string(), false)
    } else if let Some(table) = value.as_mapping() {
        let dir = table
            .get("dir")
            .and_then(|d| d.as_str())
            .unwrap_or("overlay")
            .to_string();
        let opaque = table
            .get("mode")
            .and_then(|m| m.as_str())
            .map(|m| m == "opaque")
            .unwrap_or(false);
        (dir, opaque)
    } else {
        ("overlay".to_string(), false)
    }
}

/// Build the shell snippet that applies an overlay directory into a sysroot.
/// `overlay_dir` is the path relative to `/opt/src` (the project root inside the container).
/// `sysroot_dir` is the sysroot subdirectory name (e.g., "rootfs", "initramfs").
fn build_overlay_script(overlay_dir: &str, opaque: bool, sysroot_dir: &str) -> String {
    if opaque {
        format!(
            r#"
# Apply overlay (opaque mode) — cp -r replaces directory contents
if [ -d "/opt/src/{overlay_dir}" ]; then
    echo "Applying overlay '{overlay_dir}' to {sysroot_dir} sysroot (opaque mode)"
    cp -r "/opt/src/{overlay_dir}/." "$AVOCADO_PREFIX/{sysroot_dir}/"
    chown -R root:root "$AVOCADO_PREFIX/{sysroot_dir}/"
else
    echo "Error: Overlay directory '{overlay_dir}' not found in /opt/src"
    exit 1
fi
"#
        )
    } else {
        format!(
            r#"
# Apply overlay (merge mode) — rsync -a adds/replaces files, preserving others
if [ -d "/opt/src/{overlay_dir}" ]; then
    echo "Applying overlay '{overlay_dir}' to {sysroot_dir} sysroot (merge mode)"
    rsync -a --chown=root:root "/opt/src/{overlay_dir}/" "$AVOCADO_PREFIX/{sysroot_dir}/"
else
    echo "Error: Overlay directory '{overlay_dir}' not found in /opt/src"
    exit 1
fi
"#
        )
    }
}

use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    kernel_resolver::{resolve_and_pin_kernel_version, ResolveParams},
    kernel_version::resolve_kernel_family_name,
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    stamps::{
        compute_initramfs_input_hash, compute_rootfs_input_hash, generate_write_stamp_script,
        Stamp, StampOutputs,
    },
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
    /// Skip stamp writing when true.
    pub no_stamps: bool,
    /// Parsed (merged) YAML config — needed for stamp hash computation.
    pub parsed: Option<&'a serde_yaml::Value>,
    /// TUI context for output capture (if TUI is active).
    pub tui_context: Option<crate::utils::container::TuiContext>,
}

/// Stage the kernel `Image` from the rootfs sysroot into the per-target
/// content-addressed kernel sysroot at `$AVOCADO_PREFIX/kernel/<kver>/`.
///
/// Phase 2 of the runtime-binding plan introduces the kernel sysroot as a
/// stable, content-addressed location for boot artifacts that provision can
/// read without going through the rootfs. Until Phase 5 drops the v1 rootfs
/// auto-append entirely, the rootfs install still pulls `kernel-image-<kver>`
/// to its sysroot; this staging step mirrors the resulting `Image` to the
/// kernel sysroot so multiple runtimes pinning the same kver share one copy
/// and provision has a kver-stable path to construct `boot.img` from.
///
/// Records the staged `kernel-image-<kver>` package version in
/// `lock.kernels[<kver>]` so subsequent installs see the kernel sysroot as
/// populated and `validate_kernel_consistency` (Phase 4) can assert the
/// rootfs and kernel-sysroot agree on kver.
#[allow(clippy::too_many_arguments)]
async fn stage_kernel_sysroot_from_rootfs(
    container_helper: &SdkContainer,
    container_image: &str,
    target: &str,
    kver: &str,
    rootfs_image_pkg_name: &str,
    rootfs_image_pkg_version: &str,
    lock_file: &mut LockFile,
    src_dir: &Path,
    repo_url: Option<&str>,
    repo_release: Option<&str>,
    merged_container_args: Option<Vec<String>>,
    runs_on_context: Option<&RunsOnContext>,
    sdk_arch: Option<&String>,
    verbose: bool,
    tui_context: Option<crate::utils::container::TuiContext>,
) -> Result<()> {
    // The rootfs auto-append landed `Image-<kver>` (and `Image-<kver>.gz`
    // for kernels that ship a compressed variant) under
    // `$AVOCADO_PREFIX/rootfs/boot/`. Mirror them into the kernel sysroot
    // directory keyed by version. Use cp -a so any future `Image*` siblings
    // (DTBs, multi-arch builds) get staged uniformly.
    let stage_command = format!(
        r#"
set -e
KERNEL_DIR="$AVOCADO_PREFIX/kernel/{kver}"
mkdir -p "$KERNEL_DIR"
ROOTFS_BOOT="$AVOCADO_PREFIX/rootfs/boot"
if [ ! -d "$ROOTFS_BOOT" ]; then
    echo "[ERROR] Rootfs sysroot has no /boot directory; cannot stage kernel sysroot for {kver}" >&2
    exit 1
fi
# Copy any kver-tagged Image files. Two naming conventions appear across
# distros: `Image-<kver>(.gz)?` and `Image.gz-<kver>`. Match both.
shopt -s nullglob
matched=0
for f in "$ROOTFS_BOOT"/Image-{kver}* "$ROOTFS_BOOT"/Image.gz-{kver}*; do
    cp -a "$f" "$KERNEL_DIR/"
    matched=$((matched+1))
done
if [ "$matched" -eq 0 ]; then
    echo "[ERROR] Did not find Image*-{kver}* in rootfs /boot; cannot stage kernel sysroot" >&2
    exit 1
fi
# Provide a stable name (`Image`) alongside the versioned file so consumers
# don't need to know the kver to find the bootable artifact.
if [ -f "$KERNEL_DIR/Image-{kver}" ] && [ ! -e "$KERNEL_DIR/Image" ]; then
    ln -s "Image-{kver}" "$KERNEL_DIR/Image"
fi
"#
    );

    let stage_run_config = RunConfig {
        container_image: container_image.to_string(),
        target: target.to_string(),
        command: stage_command,
        verbose,
        source_environment: true,
        interactive: false,
        repo_url: repo_url.map(|s| s.to_string()),
        repo_release: repo_release.map(|s| s.to_string()),
        container_args: merged_container_args.clone(),
        sdk_arch: sdk_arch.cloned(),
        tui_context,
        ..Default::default()
    };

    let success = if let Some(context) = runs_on_context {
        container_helper
            .run_in_container_with_context(&stage_run_config, context)
            .await?
    } else {
        container_helper.run_in_container(stage_run_config).await?
    };

    if !success {
        return Err(anyhow::anyhow!(
            "Failed to stage kernel sysroot for kernel-version '{kver}'"
        ));
    }

    // Record the kernel-image package version in the kernel sysroot's
    // lockfile entry. This makes the sysroot's contents reproducible and
    // gives Phase 4's validate_kernel_consistency a hook to verify that
    // rootfs and kernel sysroots agree on kver.
    let mut versions = std::collections::HashMap::new();
    versions.insert(
        rootfs_image_pkg_name.to_string(),
        rootfs_image_pkg_version.to_string(),
    );
    let kernel_sysroot = SysrootType::Kernel(kver.to_string());
    lock_file.update_sysroot_versions(target, &kernel_sysroot, versions);
    lock_file.save(src_dir)?;

    print_success(
        &format!("Staged kernel sysroot at $AVOCADO_PREFIX/kernel/{kver}."),
        OutputLevel::Normal,
    );

    Ok(())
}

/// Detect package removals by comparing config packages against lock file.
/// Returns true if the sysroot needs to be cleaned and reinstalled from scratch.
fn detect_sysroot_package_removals(
    config: &Config,
    sysroot_type: &SysrootType,
    target: &str,
    lock_file: &mut LockFile,
) -> bool {
    let locked_names = lock_file.get_locked_package_names(target, sysroot_type);

    if locked_names.is_empty() {
        return false;
    }

    let config_names: HashSet<String> = match sysroot_type {
        SysrootType::Rootfs => config.get_rootfs_packages().keys().cloned().collect(),
        SysrootType::Initramfs => config.get_initramfs_packages().keys().cloned().collect(),
        _ => return false,
    };

    let removed: Vec<String> = locked_names.difference(&config_names).cloned().collect();

    if removed.is_empty() {
        return false;
    }

    let label = match sysroot_type {
        SysrootType::Rootfs => "rootfs",
        SysrootType::Initramfs => "initramfs",
        _ => "sysroot",
    };
    print_info(
        &format!(
            "Packages removed from {label}: {}. Cleaning sysroot for fresh install.",
            removed.join(", ")
        ),
        OutputLevel::Normal,
    );

    // Remove only the stale entries, preserving version pins for remaining packages
    lock_file.remove_packages_from_sysroot(target, sysroot_type, &removed);

    true
}

/// Install a sysroot (rootfs or initramfs) via DNF into the SDK container volume.
///
/// This is the shared implementation used by both `avocado rootfs install`,
/// `avocado initramfs install`, and `avocado sdk install`.
///
/// Features:
/// - Detects package removals by comparing config against lock file
/// - Forces clean reinstall when packages are removed (DNF is additive-only)
/// - Tracks all installed packages in the lock file
/// - Writes install stamps for staleness detection
pub async fn install_sysroot(params: &mut SysrootInstallParams<'_>) -> Result<()> {
    let (label, sysroot_dir, default_pkg) = match params.sysroot_type {
        SysrootType::Rootfs => ("rootfs", "rootfs", "avocado-pkg-rootfs"),
        SysrootType::Initramfs => ("initramfs", "initramfs", "avocado-pkg-initramfs"),
        _ => return Err(anyhow::anyhow!("Unsupported sysroot type for install")),
    };

    print_info(&format!("Installing {label} sysroot."), OutputLevel::Normal);

    // Detect package removals: compare current config packages with lock file.
    // If packages were removed, we must clean the sysroot and reinstall from scratch
    // because DNF install is additive-only and cannot remove packages.
    let needs_clean_reinstall = detect_sysroot_package_removals(
        params.config,
        &params.sysroot_type,
        params.target,
        params.lock_file,
    );

    if needs_clean_reinstall {
        let clean_command = format!(r#"rm -rf "$AVOCADO_PREFIX/{sysroot_dir}""#);
        let clean_config = RunConfig {
            container_image: params.container_image.to_string(),
            target: params.target.to_string(),
            command: clean_command,
            verbose: params.verbose,
            source_environment: true,
            interactive: false,
            repo_url: params.repo_url.map(|s| s.to_string()),
            repo_release: params.repo_release.map(|s| s.to_string()),
            container_args: params.merged_container_args.clone(),
            sdk_arch: params.sdk_arch.cloned(),
            tui_context: params.tui_context.clone(),
            ..Default::default()
        };

        if let Some(context) = params.runs_on_context {
            params
                .container_helper
                .run_in_container_with_context(&clean_config, context)
                .await
                .ok();
        } else {
            params
                .container_helper
                .run_in_container(clean_config)
                .await
                .ok();
        }
    }

    // Get packages from config
    let packages = match params.sysroot_type {
        SysrootType::Rootfs => params.config.get_rootfs_packages(),
        SysrootType::Initramfs => params.config.get_initramfs_packages(),
        _ => unreachable!(),
    };

    // Resolve (or reuse a pinned) KERNEL_VERSION before building package specs
    // so kernel/kernel-module-*/kernel-devsrc-* names get suffixed to exactly
    // one kernel — avoiding dnf's virtual-provider tie-break picking
    // cross-kernel when multiple kernels coexist in the feed.
    // Snapshot the previously-pinned kver BEFORE the resolver runs — the
    // resolver overwrites the lockfile pin in-place when it re-resolves,
    // so reading after would just give us back what it just wrote.
    let prev_pinned_kver = params
        .lock_file
        .get_kernel_version(params.target, &params.sysroot_type)
        .cloned();

    let resolved_kver = {
        let mut resolve_params = ResolveParams {
            container_helper: params.container_helper,
            container_image: params.container_image,
            target: params.target,
            sysroot: params.sysroot_type.clone(),
            runtime_name: None,
            config: params.config,
            lock_file: params.lock_file,
            repo_url: params.repo_url,
            repo_release: params.repo_release,
            merged_container_args: params.merged_container_args.clone(),
            dnf_args: params.dnf_args.clone(),
            runs_on_context: params.runs_on_context,
            sdk_arch: params.sdk_arch,
            verbose: params.verbose,
            tui_context: params.tui_context.clone(),
        };
        resolve_and_pin_kernel_version(&mut resolve_params).await?
    };

    // Detect kernel pin change vs lockfile. dnf install is additive, so if
    // the resolved kver differs from what the lockfile recorded for this
    // sysroot, a plain re-install would land the new kernel-image and
    // module packagegroup *alongside* the prior pin's packages — leaving
    // /lib/modules/<old-kver>/, the old kernel-image, and stale module
    // packages in the sysroot. Force a clean+reinstall so the new pin is
    // the only thing present.
    if let Some(new_kver) = resolved_kver.as_deref() {
        if let Some(prev) = prev_pinned_kver {
            if prev != new_kver {
                print_info(
                    &format!(
                        "{label}: kernel pin changed ({prev} -> {new_kver}); cleaning sysroot for fresh install"
                    ),
                    OutputLevel::Normal,
                );

                let clean_command = format!(r#"rm -rf "$AVOCADO_PREFIX/{sysroot_dir}""#);
                let clean_config = RunConfig {
                    container_image: params.container_image.to_string(),
                    target: params.target.to_string(),
                    command: clean_command,
                    verbose: params.verbose,
                    source_environment: true,
                    interactive: false,
                    repo_url: params.repo_url.map(|s| s.to_string()),
                    repo_release: params.repo_release.map(|s| s.to_string()),
                    container_args: params.merged_container_args.clone(),
                    sdk_arch: params.sdk_arch.cloned(),
                    tui_context: params.tui_context.clone(),
                    ..Default::default()
                };
                if let Some(context) = params.runs_on_context {
                    params
                        .container_helper
                        .run_in_container_with_context(&clean_config, context)
                        .await
                        .ok();
                } else {
                    params
                        .container_helper
                        .run_in_container(clean_config)
                        .await
                        .ok();
                }

                // Wipe the package state for this sysroot so a failed
                // re-install can't leave a stale package map pointing at a
                // now-empty sysroot.
                match params.sysroot_type {
                    SysrootType::Rootfs => params.lock_file.clear_rootfs(params.target),
                    SysrootType::Initramfs => params.lock_file.clear_initramfs(params.target),
                    _ => {}
                }
                // Remove and immediately re-pin the new kver. Remove first so
                // the entry is correct even if the install below fails (empty
                // sysroot + correct kver = retry without re-clean). Re-pin
                // so the sdk/install.rs merge site can see the new kver after
                // a successful install — without it the `if let Some(kver)`
                // check in that merge finds nothing and the old kver from the
                // initial clone bleeds through into the saved lockfile.
                params
                    .lock_file
                    .remove_kernel_version(params.target, &params.sysroot_type);
                params
                    .lock_file
                    .set_kernel_version(params.target, &params.sysroot_type, new_kver);
            }
        }
    }

    // Build package specs for all configured packages. When we have a
    // resolved kernel version, substitute any `{{ avocado.kernel.version }}`
    // templates in package keys so BSP yamls can produce fully-versioned
    // kernel-family names without silent rewriting.
    let resolve_name = |name: &str| -> String {
        match resolved_kver.as_deref() {
            Some(kver) => resolve_kernel_family_name(name, kver),
            None => name.to_string(),
        }
    };

    // When the default meta-package is in the effective list and a kernel is
    // pinned, auto-append the matching per-kernel module packagegroup so
    // transitive module pulls land on the pinned kernel's modules instead of
    // dnf's NVR tie-break. Users opt out implicitly by defining their own
    // rootfs.packages: / initramfs.packages: without the default meta-package.
    let has_default_pkg = packages.is_empty() || packages.contains_key(default_pkg);
    let auto_module_pkg: Option<String> = match (resolved_kver.as_deref(), has_default_pkg) {
        (Some(kver), true) => {
            let name = match params.sysroot_type {
                SysrootType::Rootfs => format!("packagegroup-avocado-rootfs-modules-{kver}"),
                SysrootType::Initramfs => {
                    format!("packagegroup-avocado-initramfs-modules-{kver}")
                }
                _ => unreachable!(),
            };
            print_info(
                &format!("Auto-including {name} for pinned kernel {kver}"),
                OutputLevel::Normal,
            );
            Some(name)
        }
        (None, _) => {
            print_info(
                &format!(
                    "Skipping kernel-modules packagegroup auto-append for {label}: no kernel version resolved"
                ),
                OutputLevel::Normal,
            );
            None
        }
        (_, false) => {
            print_info(
                &format!(
                    "Skipping kernel-modules packagegroup auto-append for {label}: {default_pkg} not in packages list"
                ),
                OutputLevel::Normal,
            );
            None
        }
    };

    // Rootfs only: also auto-append the kernel-image meta so the kernel Image
    // (e.g. /boot/Image-${KERNEL_VERSION}) lands in the sysroot. The provision
    // step uses it to repack boot.img for tegraflash, so the booted kernel
    // matches the resolver-pinned modules. The `kernel-image-${kver}` meta
    // RDEPENDS on the Image-bearing sub-packages (kernel-image-image-${kver}
    // and kernel-image-image.gz-${kver}); dnf pulls both. Initramfs doesn't
    // need this — boot.img embeds the initramfs cpio, the kernel comes from
    // the rootfs sysroot.
    let auto_kernel_image_pkg: Option<String> = match (
        resolved_kver.as_deref(),
        has_default_pkg,
        &params.sysroot_type,
    ) {
        (Some(kver), true, SysrootType::Rootfs) => {
            let name = format!("kernel-image-{kver}");
            print_info(
                &format!("Auto-including {name} for pinned kernel {kver}"),
                OutputLevel::Normal,
            );
            Some(name)
        }
        _ => None,
    };

    let mut pkg_specs: Vec<String> = if packages.is_empty() {
        vec![build_package_spec_with_lock(
            params.lock_file,
            params.target,
            &params.sysroot_type,
            &resolve_name(default_pkg),
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
                    &resolve_name(name),
                    ver,
                )
            })
            .collect()
    };
    if let Some(ref name) = auto_module_pkg {
        pkg_specs.push(build_package_spec_with_lock(
            params.lock_file,
            params.target,
            &params.sysroot_type,
            name,
            "*",
        ));
    }
    if let Some(ref name) = auto_kernel_image_pkg {
        pkg_specs.push(build_package_spec_with_lock(
            params.lock_file,
            params.target,
            &params.sysroot_type,
            name,
            "*",
        ));
    }
    let pkg = pkg_specs.join(" ");

    // Collect all package names for lock file queries
    let mut all_package_names: Vec<String> = if packages.is_empty() {
        vec![default_pkg.to_string()]
    } else {
        packages.keys().cloned().collect()
    };
    if let Some(ref name) = auto_module_pkg {
        all_package_names.push(name.clone());
    }
    if let Some(ref name) = auto_kernel_image_pkg {
        all_package_names.push(name.clone());
    }

    let yes = if params.force { "-y" } else { "" };
    let dnf_args_str = if let Some(args) = &params.dnf_args {
        format!(" {} ", args.join(" "))
    } else {
        String::new()
    };

    // Build optional overlay snippet — appended to the install command so it
    // runs in the same container invocation immediately after DNF finishes.
    let overlay_snippet = params
        .parsed
        .and_then(|parsed| {
            let key = match params.sysroot_type {
                SysrootType::Rootfs => "rootfs",
                SysrootType::Initramfs => "initramfs",
                _ => return None,
            };
            parsed.get(key)?.get("overlay")
        })
        .map(|v| {
            let (dir, opaque) = parse_overlay_config(v);
            build_overlay_script(&dir, opaque, sysroot_dir)
        })
        .unwrap_or_default();

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
{overlay_snippet}"#
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
        tui_context: params.tui_context.clone(),
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

        // Query installed versions for ALL config packages and update lock file
        let installed_versions = params
            .container_helper
            .query_installed_packages(
                &params.sysroot_type,
                &all_package_names,
                params.container_image,
                params.target,
                params.repo_url.map(|s| s.to_string()),
                params.repo_release.map(|s| s.to_string()),
                params.merged_container_args.clone(),
                params.runs_on_context,
                params.sdk_arch,
                None,
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
                    &format!("Updated lock file with {label} package versions."),
                    OutputLevel::Normal,
                );
            }
            params.lock_file.save(params.src_dir)?;
        }

        // Stage the kernel sysroot from the rootfs (Phase 2c). Only when:
        // - sysroot is rootfs (initramfs doesn't carry the kernel Image),
        // - a kernel was resolved (no-op for non-kernel-pinned configs),
        // - the auto-appended kernel-image package was actually pulled.
        if matches!(params.sysroot_type, SysrootType::Rootfs) {
            if let (Some(kver), Some(kernel_image_pkg)) =
                (resolved_kver.as_deref(), auto_kernel_image_pkg.as_deref())
            {
                // We need the version of the kernel-image package that the
                // resolver actually pinned. The lockfile was just updated
                // above with the rootfs install's installed versions; pull
                // it from there.
                let pkg_version = params
                    .lock_file
                    .get_locked_version(params.target, &params.sysroot_type, kernel_image_pkg)
                    .cloned()
                    .unwrap_or_else(|| "*".to_string());

                if let Err(e) = stage_kernel_sysroot_from_rootfs(
                    params.container_helper,
                    params.container_image,
                    params.target,
                    kver,
                    kernel_image_pkg,
                    &pkg_version,
                    params.lock_file,
                    params.src_dir,
                    params.repo_url,
                    params.repo_release,
                    params.merged_container_args.clone(),
                    params.runs_on_context,
                    params.sdk_arch,
                    params.verbose,
                    params.tui_context.clone(),
                )
                .await
                {
                    print_error(
                        &format!(
                            "Kernel sysroot staging failed: {e}. \
                             provision may fall back to reading the Image from the rootfs sysroot."
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Write install stamp (unless --no-stamps or no parsed config available)
        if !params.no_stamps {
            if let Some(parsed) = params.parsed {
                let stamp_result = match params.sysroot_type {
                    SysrootType::Rootfs => {
                        let inputs = compute_rootfs_input_hash(parsed)?;
                        let outputs = StampOutputs::default();
                        Ok(Stamp::rootfs_install(params.target, inputs, outputs))
                    }
                    SysrootType::Initramfs => {
                        let inputs = compute_initramfs_input_hash(parsed)?;
                        let outputs = StampOutputs::default();
                        Ok(Stamp::initramfs_install(params.target, inputs, outputs))
                    }
                    _ => Err(anyhow::anyhow!("Unsupported sysroot type for stamps")),
                };

                if let Ok(stamp) = stamp_result {
                    let stamp_script = generate_write_stamp_script(&stamp)?;
                    let stamp_config = RunConfig {
                        container_image: params.container_image.to_string(),
                        target: params.target.to_string(),
                        command: stamp_script,
                        verbose: params.verbose,
                        source_environment: true,
                        interactive: false,
                        repo_url: params.repo_url.map(|s| s.to_string()),
                        repo_release: params.repo_release.map(|s| s.to_string()),
                        container_args: params.merged_container_args.clone(),
                        sdk_arch: params.sdk_arch.cloned(),
                        tui_context: params.tui_context.clone(),
                        ..Default::default()
                    };

                    if let Some(context) = params.runs_on_context {
                        params
                            .container_helper
                            .run_in_container_with_context(&stamp_config, context)
                            .await?;
                    } else {
                        params
                            .container_helper
                            .run_in_container(stamp_config)
                            .await?;
                    }

                    if params.verbose {
                        print_info(
                            &format!("Wrote install stamp for {label}."),
                            OutputLevel::Normal,
                        );
                    }
                }
            }
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
            no_stamps: self.no_stamps,
            parsed: Some(&composed.merged_value),
            tui_context: None,
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
