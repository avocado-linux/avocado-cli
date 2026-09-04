//! Rootfs sysroot install command and shared install logic for rootfs/initramfs.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::utils::overlay_preprocess::parse_overlay_config;

/// Build the shell snippet that applies an overlay directory into a sysroot.
/// `overlay_dir` is the path relative to `/opt/src` (the project root inside the container).
/// `sysroot_dir` is the sysroot subdirectory name (e.g., "rootfs", "initramfs").
///
/// Uses `cp -a` (preserve attrs + symlinks + recursive) followed by
/// `chown -R root:root` rather than `rsync -a` so we don't depend on rsync
/// being present in the SDK image (some SDK variants don't ship it).
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
# Apply overlay (merge mode) — cp -a copies + preserves attrs, merging into
# the existing sysroot tree; chown -R then resets ownership to root:root.
if [ -d "/opt/src/{overlay_dir}" ]; then
    echo "Applying overlay '{overlay_dir}' to {sysroot_dir} sysroot (merge mode)"
    cp -a "/opt/src/{overlay_dir}/." "$AVOCADO_PREFIX/{sysroot_dir}/"
    chown -R root:root "$AVOCADO_PREFIX/{sysroot_dir}/"
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
    kernel_resolver::{off_kernel_dnf_excludes, resolve_and_pin_kernel_version, ResolveParams},
    kernel_version::substitute_kernel_version,
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{
        print_error, print_info, print_success, print_warning, print_warning_stderr, OutputLevel,
    },
    prerequisites::read_stamps_batch,
    runs_on::RunsOnContext,
    stamps::{
        compute_initramfs_input_hash, compute_rootfs_input_hash, generate_write_stamp_script,
        Stamp, StampInputs, StampOutputs, StampRequirement, SysrootStampInputs,
    },
    target::validate_and_log_target,
};

use super::clean::clean_sysroot_command;

/// Parameters for the shared sysroot install function.
pub struct SysrootInstallParams<'a> {
    pub sysroot_type: SysrootType,
    pub config: &'a Config,
    pub lock_file: &'a mut LockFile,
    pub src_dir: &'a Path,
    pub container_helper: &'a SdkContainer,
    pub container_image: &'a str,
    pub target: &'a str,
    /// CLI target board override for `{{ avocado.target.board }}`
    pub target_board: Option<&'a str>,
    pub repo_url: Option<&'a str>,
    pub repo_release: Option<&'a str>,
    pub merged_container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub verbose: bool,
    pub force: bool,
    pub runs_on_context: Option<&'a RunsOnContext>,
    pub sdk_arch: Option<&'a String>,
    /// Skip stamp reading and writing when true — the escape hatch that
    /// forces a full reinstall.
    pub no_stamps: bool,
    /// Parsed (merged) YAML config — needed for stamp hash computation.
    pub parsed: Option<&'a serde_yaml::Value>,
    /// This sysroot's install stamp, read by the caller. `avocado sdk install`
    /// batches both sysroots' stamps into one container invocation; the
    /// standalone commands read their own. `None` means no stamp was found
    /// (or none was read), which always installs.
    pub prefetched_stamp: Option<Stamp>,
    /// TUI context for output capture (if TUI is active).
    pub tui_context: Option<crate::utils::container::TuiContext>,
    /// Set by [`install_sysroot`] the moment the package install has landed on
    /// disk. From then on `lock_file` describes that disk - the kernel pin
    /// chosen for it, the sections cleared for a clean reinstall, and (when
    /// they could be read) the installed package versions - so it must be
    /// persisted whatever happens next. It stays set when the install then
    /// fails: a caller that drops the lock leaves disk holding kernel B while
    /// the lock still says A, and the next run honors A's pin and installs
    /// additively on top of B. Callers must persist (or merge) the lock
    /// whenever this is true, whatever the returned `Result` says.
    pub pins_recorded: bool,
}

impl SysrootInstallParams<'_> {
    /// The `SysrootType`-appropriate stamp requirement, or `None` for a
    /// sysroot type that has no install stamp.
    fn stamp_requirement(sysroot_type: &SysrootType) -> Option<StampRequirement> {
        match sysroot_type {
            SysrootType::Rootfs => Some(StampRequirement::rootfs_install()),
            SysrootType::Initramfs => Some(StampRequirement::initramfs_install()),
            _ => None,
        }
    }
}

/// Read one sysroot's install stamp for the standalone `avocado rootfs
/// install` / `avocado initramfs install` entry points, which have no
/// sibling task to batch with. Returns `None` when stamps are disabled, the
/// stamp is absent, or it can't be parsed.
pub async fn read_sysroot_install_stamp(
    sysroot_type: &SysrootType,
    no_stamps: bool,
    container_helper: &SdkContainer,
    base_run_config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
) -> Result<Option<Stamp>> {
    if no_stamps {
        return Ok(None);
    }
    let Some(requirement) = SysrootInstallParams::stamp_requirement(sysroot_type) else {
        return Ok(None);
    };
    let batch = read_stamps_batch(
        std::slice::from_ref(&requirement),
        container_helper,
        base_run_config,
        runs_on_context,
    )
    .await?;
    Ok(batch.stamp_for(&requirement))
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
ROOTFS_ROOT="$AVOCADO_PREFIX/rootfs"
if [ ! -d "$ROOTFS_ROOT/boot" ]; then
    echo "[ERROR] Rootfs sysroot has no /boot directory; cannot stage kernel sysroot for {kver}" >&2
    exit 1
fi
# Locate bootable kernel images by their versioned filename suffix rather than
# by enumerating KERNEL_IMAGETYPE names. Any file in /boot named *-{kver} or
# *-{kver}.gz that isn't a known non-bootable kernel-base artifact qualifies.
# (The RPM sysroot db is not queryable here — packages installed via dnf
# --installroot are not tracked in the sysroot's own rpmdb.)
# LC_ALL=C: this order is not cosmetic — the 'Image' symlink block below walks
# boot_files and takes the FIRST uncompressed variant (falling back to [0]), so
# collation decides which kernel binary 'Image' ends up pointing at.
mapfile -t boot_files < <(
    find "$ROOTFS_ROOT/boot" -maxdepth 1 -type f \
        \( -name "*-{kver}" -o -name "*-{kver}.gz" \) \
        ! -name "System.map-*" ! -name "config-*" \
    | LC_ALL=C sort
)
if [ "${{#boot_files[@]}}" -eq 0 ]; then
    echo "[ERROR] No bootable kernel image found for {kver} in rootfs /boot" >&2
    exit 1
fi
for abs_path in "${{boot_files[@]}}"; do
    cp -a "$abs_path" "$KERNEL_DIR/"
done
# Stable 'Image' symlink — prefer uncompressed over .gz so consumers can use
# it directly without decompression. -sfn replaces a stale symlink in place
# on rerun without erroring on "File exists".
for abs_path in "${{boot_files[@]}}"; do
    [[ "$abs_path" == *.gz ]] && continue
    ln -sfn "$(basename "$abs_path")" "$KERNEL_DIR/Image"
    break
done
if [ ! -e "$KERNEL_DIR/Image" ]; then
    ln -sfn "$(basename "${{boot_files[0]}}")" "$KERNEL_DIR/Image"
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

    print_success(
        &format!("Staged kernel sysroot at $AVOCADO_PREFIX/kernel/{kver}."),
        OutputLevel::Normal,
    );

    Ok(())
}

/// Why an install that placed its packages still must not report success.
///
/// `None` means every claim the install stamp makes holds: the packages are on
/// disk, the lock records their versions, and (for rootfs with a pinned
/// kernel) the kernel sysroot is staged. `Some(reason)` means one of them does
/// not, so no stamp is written and `runtime build` will refuse to build.
///
/// Split out of [`install_sysroot`] to keep the wording under test: it is the
/// only place a user learns why an install that looked fine left the build
/// unable to proceed.
fn incomplete_install_reason(
    install_is_clean: bool,
    versions_recorded: bool,
    kernel_staging_error: Option<&str>,
) -> Option<String> {
    if !install_is_clean {
        // Checked first: a sysroot that could not be cleaned may be carrying
        // packages from a previous config, which makes the other two signals
        // describe the wrong tree.
        Some("the sysroot could not be cleaned first, so stale contents may remain".to_string())
    } else if !versions_recorded {
        Some("the installed package versions could not be read".to_string())
    } else {
        kernel_staging_error.map(|detail| {
            format!(
                "the kernel sysroot could not be staged ({detail}). Verify that the rootfs \
                 sysroot's /boot contains a kernel image for this kernel version; without one \
                 the packages that were installed do not include a bootable kernel"
            )
        })
    }
}

/// The error an install that placed its packages but could not finish reports.
///
/// Kept next to [`incomplete_install_reason`] and under test because this text
/// is the whole remedy path: without it the user sees a passing `install` and a
/// `build` that names `<sysroot> install` as missing, whose printed fix is the
/// command that just passed.
fn incomplete_install_error(label: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Installed the {label} sysroot's packages, but the install did not complete: {reason}. \
         No install stamp was recorded, so `avocado build` will keep reporting `{label} install` \
         as missing until this succeeds."
    )
}

/// Whether a configured version is an explicit pin rather than "follow the feed".
///
/// `"*"` (or nothing) asks for whatever the feed has; anything else is a
/// version the project chose and the sync must not walk away from.
pub(crate) fn is_explicit_version(version: Option<&str>) -> bool {
    matches!(version.map(str::trim), Some(v) if !v.is_empty() && v != "*")
}

/// The dnf pass that makes an existing sysroot follow the feed after
/// `avocado update` (or on a first install, where it is a no-op): a
/// distro-sync of everything already in the sysroot, same environment and
/// excludes as the install line. Empty when the lock carries pins - then the
/// lock, not the feed, says which versions belong in the sysroot.
fn dnf_sync_step(
    fresh_resolve: bool,
    sysroot_dir: &str,
    dnf_args_str: &str,
    yes: &str,
    exclude_str: &str,
) -> String {
    if !fresh_resolve {
        return String::new();
    }
    format!(
        r#"
# No version pins for this sysroot (first install, or `avocado update` cleared
# them): bring already-installed packages in line with the feed too.
RPM_NO_CHROOT_FOR_SCRIPTS=1 \
AVOCADO_EXT_INSTALLROOT=$AVOCADO_PREFIX/{sysroot_dir} \
AVOCADO_SYSROOT_SCRIPTS=1 \
PATH=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin:$PATH \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF \
    {dnf_args_str} --refresh {yes} {exclude_str} --installroot $AVOCADO_PREFIX/{sysroot_dir} distro-sync
"#
    )
}

/// Detect package removals by comparing the **effective** package set for
/// this sysroot against what the lockfile recorded. A non-empty result means
/// the sysroot must be cleaned and reinstalled from scratch, because dnf
/// install is additive-only and cannot remove packages.
///
/// `effective_names` is the config-declared packages *plus* the packages
/// [`install_sysroot`] auto-appends: the per-kernel module packagegroup and,
/// for rootfs, `kernel-image-<kver>`. Deriving the reference set from config
/// alone — as this did before — reads those auto-appended lock entries as
/// removals on every run after the first, which wipes both sysroots and,
/// via `remove_packages_from_sysroot`, drops their version pins so dnf
/// resolves newest-available instead of the locked NVR.
///
/// Returns the removed names sorted, or empty when the sysroot is consistent.
fn detect_sysroot_package_removals(
    effective_names: &HashSet<String>,
    sysroot_type: &SysrootType,
    target: &str,
    lock_file: &LockFile,
) -> Vec<String> {
    let locked_names = lock_file.get_locked_package_names(target, sysroot_type);

    if locked_names.is_empty() {
        return Vec::new();
    }

    let mut removed: Vec<String> = locked_names.difference(effective_names).cloned().collect();
    removed.sort();
    removed
}

/// `rm -rf` a sysroot (and its stamp) inside the SDK container.
///
/// Shares [`clean_sysroot_command`] with `avocado rootfs clean` so the
/// mid-install clean and the user-facing clean can't drift.
///
/// Best-effort by design, matching the inline copies this replaced: an
/// absent sysroot is the normal case on a first install and must not fail
/// the run.
/// Returns whether the wipe actually succeeded. The result used to be discarded
/// outright; it is reported now because a failed wipe means the reinstall lands
/// *alongside* the old contents (dnf install is additive), and writing a current
/// stamp over that latches the mixed state forever.
async fn clean_sysroot(params: &SysrootInstallParams<'_>, sysroot_dir: &str) -> bool {
    let clean_config = RunConfig {
        container_image: params.container_image.to_string(),
        target: params.target.to_string(),
        command: clean_sysroot_command(sysroot_dir),
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

    let outcome = if let Some(context) = params.runs_on_context {
        params
            .container_helper
            .run_in_container_with_context(&clean_config, context)
            .await
    } else {
        params.container_helper.run_in_container(clean_config).await
    };

    match outcome {
        Ok(true) => true,
        Ok(false) => {
            print_error(
                &format!("Failed to clean the {sysroot_dir} sysroot before reinstalling."),
                OutputLevel::Normal,
            );
            false
        }
        Err(e) => {
            print_error(
                &format!("Failed to clean the {sysroot_dir} sysroot before reinstalling: {e}"),
                OutputLevel::Normal,
            );
            false
        }
    }
}

/// The effective package set for a sysroot; `None` for a type with no
/// install of its own. The install and the stamp hash both read it through
/// here so they cannot pick different sets.
pub fn sysroot_packages(
    config: &Config,
    sysroot_type: &SysrootType,
    target: &str,
    parsed: Option<&serde_yaml::Value>,
) -> Option<HashMap<String, serde_yaml::Value>> {
    match sysroot_type {
        SysrootType::Rootfs => Some(config.get_rootfs_packages(parsed, target)),
        SysrootType::Initramfs => Some(config.get_initramfs_packages(parsed, target)),
        _ => None,
    }
}

/// The ingredients of a sysroot install stamp's input hash, for callers that
/// have not run an install. Both sides build it here, so a new input cannot
/// reach one side only.
pub struct SysrootStampContext<'a> {
    pub sysroot_type: &'a SysrootType,
    pub config: &'a Config,
    pub parsed: &'a serde_yaml::Value,
    /// `Config::project_root`, matching every install path.
    pub src_dir: &'a Path,
    pub target: &'a str,
    pub target_board: Option<&'a str>,
    pub repo_url: Option<&'a str>,
    pub repo_release: Option<&'a str>,
    pub dnf_args: Option<&'a [String]>,
    pub lock_file: &'a LockFile,
}

/// `None` for a sysroot type that has no install stamp.
pub fn compute_sysroot_install_inputs(
    ctx: &SysrootStampContext<'_>,
    packages: &HashMap<String, serde_yaml::Value>,
) -> Result<Option<StampInputs>> {
    let resolved = SysrootStampInputs {
        packages,
        repo_url: ctx.repo_url,
        repo_release: ctx.repo_release,
        disable_weak_dependencies: ctx.config.get_sdk_disable_weak_dependencies(),
        dnf_args: ctx.dnf_args,
        locked_packages: ctx
            .lock_file
            .get_sysroot_versions(ctx.target, ctx.sysroot_type),
    };

    let inputs = match ctx.sysroot_type {
        SysrootType::Rootfs => {
            compute_rootfs_input_hash(ctx.parsed, ctx.src_dir, ctx.target_board, &resolved)?
        }
        SysrootType::Initramfs => {
            compute_initramfs_input_hash(ctx.parsed, ctx.src_dir, ctx.target_board, &resolved)?
        }
        _ => return Ok(None),
    };

    Ok(Some(inputs))
}

/// Compute this sysroot's install-stamp inputs from the config, the SDK feed
/// identity, and the lockfile pins **as they stand at call time**.
///
/// Called twice per install: once up front to compare against the stamp on
/// record, and once after a successful install to write the stamp the next
/// run will compare against. The second call has to see the post-install
/// lock (the install re-pins packages), which is why this reads the lock
/// each time instead of caching a single value.
///
/// Returns `None` when there is no parsed config to hash, or for a sysroot
/// type that has no install stamp.
fn compute_install_stamp_inputs(
    params: &SysrootInstallParams<'_>,
    packages: &HashMap<String, serde_yaml::Value>,
) -> Result<Option<StampInputs>> {
    let Some(parsed) = params.parsed else {
        return Ok(None);
    };

    compute_sysroot_install_inputs(
        &SysrootStampContext {
            sysroot_type: &params.sysroot_type,
            config: params.config,
            parsed,
            src_dir: params.src_dir,
            target: params.target,
            target_board: params.target_board,
            repo_url: params.repo_url,
            repo_release: params.repo_release,
            dnf_args: params.dnf_args.as_deref(),
            lock_file: params.lock_file,
        },
        packages,
    )
}

/// Probe the target repo for `package_name` via repoquery (metadata-only, no
/// install).
///
/// `Ok(Some(true))` / `Ok(Some(false))` mean the probe ran and answered.
/// `Ok(None)` means the probe itself failed, which is *not* the same as
/// "absent from the feed" and must not be collapsed into one.
async fn package_exists_in_target_repo(
    params: &SysrootInstallParams<'_>,
    package_name: &str,
) -> Result<Option<bool>> {
    // Append '*' to force glob mode: without it DNF parses the versioned name
    // (e.g. packagegroup-avocado-rootfs-modules-5.15.185-l4t-r36.5-1033.33)
    // as a NEVRA spec and splits on dashes to find NAME-VERSION-RELEASE,
    // causing a false-empty result even when the package is present.
    // The `|| true` this replaces made a feed or metadata blip indistinguishable
    // from "not in the feed" — and answering "absent" to a failed probe drops the
    // module packagegroup, which then reads as a removal, wipes the sysroot, and
    // reinstalls without it. Print a sentinel on the success branch so the caller
    // can tell the two apart.
    let command = format!(
        "if out=$($DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF repoquery --qf '%{{NAME}}' '{package_name}*' 2>/dev/null); then \
             printf 'AVOCADO_PROBE_OK\\n%s\\n' \"$out\"; \
         else \
             printf 'AVOCADO_PROBE_FAILED\\n'; \
         fi"
    );
    let run_config = RunConfig {
        container_image: params.container_image.to_string(),
        target: params.target.to_string(),
        command,
        verbose: params.verbose,
        source_environment: false,
        interactive: false,
        repo_url: params.repo_url.map(|s| s.to_string()),
        repo_release: params.repo_release.map(|s| s.to_string()),
        container_args: params.merged_container_args.clone(),
        dnf_args: params.dnf_args.clone(),
        sdk_arch: params.sdk_arch.cloned(),
        tui_context: params.tui_context.clone(),
        ..Default::default()
    };
    let output = if let Some(ctx) = params.runs_on_context {
        params
            .container_helper
            .run_in_container_with_output_remote(&run_config, ctx)
            .await
            .context("failed to probe target repo for package existence")?
    } else {
        params
            .container_helper
            .run_in_container_with_output(run_config)
            .await
            .context("failed to probe target repo for package existence")?
    };
    Ok(output.as_deref().and_then(parse_probe_output))
}

/// Interpret [`package_exists_in_target_repo`]'s probe output.
///
/// `Some(true)`/`Some(false)` only when the `AVOCADO_PROBE_OK` sentinel is
/// present, meaning repoquery exited zero and the lines after it are its
/// answer. Anything else — `AVOCADO_PROBE_FAILED`, or output mangled by a
/// shell banner — is `None`: unknown, not absent.
fn parse_probe_output(text: &str) -> Option<bool> {
    let idx = text.lines().position(|l| l.trim() == "AVOCADO_PROBE_OK")?;
    let names = text.lines().skip(idx + 1).collect::<Vec<_>>().join("\n");
    Some(!names.trim().is_empty())
}

/// Write this sysroot's install stamp.
///
/// Split out of [`install_sysroot`] so its failures can be reported without
/// `?`-ing out of the caller: the install has already landed and its lock pins
/// are recorded in memory by this point, and the caller only persists them when
/// `install_sysroot` returns `Ok`.
/// Run the var-key attestation check inside the SDK container.
///
/// Separate from the build-time tiers on purpose: those run in BitBake and
/// never see this sysroot, because avocado-cli composes from feed RPMs rather
/// than building a Yocto image.
async fn verify_var_key_attestation(
    params: &SysrootInstallParams<'_>,
    sysroot_dir: &str,
) -> Result<()> {
    let config = RunConfig {
        container_image: params.container_image.to_string(),
        target: params.target.to_string(),
        command: generate_var_key_attestation_script(
            sysroot_dir,
            matches!(params.sysroot_type, SysrootType::Initramfs),
        ),
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

    let outcome = if let Some(context) = params.runs_on_context {
        params
            .container_helper
            .run_in_container_with_context(&config, context)
            .await
    } else {
        params.container_helper.run_in_container(config).await
    };

    match outcome {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow::anyhow!(
            "The composed {sysroot_dir} declares encrypted-var but one or more \
             scripts on its /var unlock path failed attestation. See the \
             diagnostic above, which names the file and the reason."
        )),
        Err(e) => Err(e).context("could not run the var-key attestation check"),
    }
}

async fn write_install_stamp(
    params: &SysrootInstallParams<'_>,
    packages: &HashMap<String, serde_yaml::Value>,
    label: &str,
) -> Result<()> {
    let Some(inputs) = compute_install_stamp_inputs(params, packages)? else {
        return Ok(());
    };

    let stamp = match params.sysroot_type {
        SysrootType::Rootfs => {
            Stamp::rootfs_install(params.target, inputs, StampOutputs::default())
        }
        SysrootType::Initramfs => {
            Stamp::initramfs_install(params.target, inputs, StampOutputs::default())
        }
        _ => unreachable!("sysroot type was validated at entry"),
    };

    let stamp_config = RunConfig {
        container_image: params.container_image.to_string(),
        target: params.target.to_string(),
        command: generate_write_stamp_script(&stamp)?,
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
    Ok(())
}

/// Shell that verifies the composed sysroot's var-key provider against the
/// attestation the build wrote beside it.
///
/// meta-avocado gates this at three tiers, and all three hang off a Yocto
/// *image* recipe or the cryptsetup-var recipe's own datastore. avocado-cli
/// composes a sysroot from the feed's RPMs and builds no image, so none of
/// them runs on the path a device actually receives - the tree says so itself
/// in avocado-security-capabilities.bb, which exists because the image-scope
/// artifact never reached a jetson-orin-nano.
///
/// What survives into the RPM is every script on the unlock path plus the
/// `.sha256` that cryptsetup-var's do_install writes beside each one AFTER
/// every deliverability check passes. Checking the pairs here is what re-arms
/// the gate for this path: a script swapped after validation no longer
/// matches, and a missing attestation means the build-time check never
/// completed over that file.
///
/// The set is deliberately wider than the provider. `var-key.sh` only derives
/// 64 bytes; `cryptsetup-var.sh` decides whether it is called, what happens to
/// the result, and whether to refuse - so a constant key file substituted there
/// ships a fleet-wide /var key with `var-key.sh.sha256` still matching. It
/// mirrors `avocado_var_key_attested_components()` in meta-avocado's
/// avocado-security-capabilities.bbclass, which carries the reasoning and the
/// reason `avocado-posture-publish.sh` is not on the list. The two lists are
/// separate copies in separate repos; a component added there and not here
/// silently stops being checked on this path.
///
/// `require_provider` mirrors the build-side rule - only the initramfs must
/// carry a provider, while a rootfs legitimately ships the udev and posture
/// packages without one, and validates it when it does have one.
///
/// ROLLOUT ORDER IS NOT OPTIONAL. A feed built before meta-avocado wrote the
/// attestation has no `.sha256`, and this refuses it rather than warning -
/// measured, rc=1, against a sysroot composed from a real pre-change RPM. So
/// meta-avocado ships first and feeds rebuild before this check goes live.
/// Softening the missing-attestation case to a warning would reintroduce
/// exactly the silent pass the check exists to remove, and would do it on the
/// louder of the two signals: the build writes the attestation LAST, so its
/// absence means the deliverability checks did not complete.
///
/// The utilities below are present in the SDK container and verified there,
/// not assumed: sha256sum, readlink, grep, cat and cut all resolve under
/// /usr/bin in avocadolinux/sdk:2024-edge, `readlink -f` resolves, and
/// sha256sum emits the two-field output `cut -d' ' -f1` expects.
fn generate_var_key_attestation_script(sysroot_dir: &str, require_provider: bool) -> String {
    let missing_provider = if require_provider {
        r#"    echo "avocado: this sysroot declares encrypted-var but ships no" >&2
    echo "avocado: $DIR - cryptsetup-var.sh reads the declaration at boot," >&2
    echo "avocado: finds no way to derive a key, and /var never unlocks." >&2
    exit 1"#
    } else {
        r#"    exit 0"#
    };

    format!(
        r#"
set -eu
# Canonicalised once, because the parent-directory check below compares a
# resolved path against this one. Comparing against the RAW prefix refused a
# perfectly good sysroot whenever $AVOCADO_PREFIX carried a trailing slash or a
# symlinked component - measured rc=1 on an untampered tree under both. The
# build-side half never had this bug: it realpath()s both sides and
# prefix-compares.
RAW="$AVOCADO_PREFIX/{sysroot_dir}"

# Nothing composed at this path: nothing to check, and NOT a refusal. Tested
# before the canonicalisation below because `readlink -f` exits non-zero on a
# dangling path, which under `set -e` killed the script with rc=1 and an empty
# stderr - a refusal the caller reports as a failed attestation with no
# diagnostic to show for it. Observed in the SDK container against a prefix
# whose symlink resolved outside the mount.
[ -d "$RAW" ] || exit 0
SYSROOT=$(readlink -f "$RAW")
CAPS="$SYSROOT/etc/avocado-security-capabilities"
DIR="usr/libexec/cryptsetup-var"
UNITDIR="usr/lib/systemd/system"

# One entry per attested component, mirroring
# avocado_var_key_attested_components() in meta-avocado's
# avocado-security-capabilities.bbclass. Two directories, because the unit that
# RUNS the unlock path does not live beside the scripts - and binding the
# scripts while leaving the unit unbound is the same mistake one level up as
# binding var-key.sh while leaving cryptsetup-var.sh unbound.
COMPONENTS="\
$DIR/cryptsetup-var.sh:yes \
$DIR/var-key.sh:yes \
$DIR/var-hwkey.sh:no \
$UNITDIR/cryptsetup-var.service:yes"

# Undeclared, or unmigrated: nothing to check.
[ -f "$CAPS" ] || exit 0
grep -qw 'encrypted-var' "$CAPS" || exit 0

# Decide on the SET, not on the provider. Gating the component checks on
# var-key.sh made the higher-value target checkable only when the smaller one
# was present: measured, a rootfs sysroot with a substituted cryptsetup-var.sh,
# a stale digest and no var-key.sh passed with rc=0 and no stderr. Restoring an
# untouched var-key.sh flipped the same tree to rc=1.
#
# `-L` as well as `-e` throughout, because a DANGLING symlink satisfies neither
# `-e` nor `-f` and would count as absent. A link named var-key.sh is something
# shipped, not something missing; check_component is what refuses it.
shipped=
for _entry in $COMPONENTS; do
    _rel=${{_entry%:*}}
    if [ -e "$SYSROOT/$_rel" ] || [ -L "$SYSROOT/$_rel" ]; then
        shipped="$shipped $_rel"
    fi
done

# Nothing from cryptsetup-var reached this sysroot. Only the initramfs is
# required to carry the unlock path; a rootfs legitimately ships the udev and
# posture packages without it.
if [ -z "$shipped" ]; then
{missing_provider}
fi

# Every refusal calls `exit`, never `return`. A `return 1` leaves the refusal
# depending on `set -e` still being armed at the call site, and this is a
# security gate: a later `|| true`, a wrapping `if`, or a caller that drops
# errexit would turn every refusal into an unread warning on stderr while the
# script still exited 0 - because the last call, `var-hwkey.sh no`, returns 0
# on every machine that ships no hardware backend. `exit` inside a function
# terminates the script regardless of how it was called, which is the property
# a gate needs.
check_component() {{
    rel="$1"
    required="$2"
    path="$SYSROOT/$rel"

    # Before the existence test, not after. A dangling symlink fails `-e`, so
    # testing existence first reports it as an absent file and an optional
    # component would then be skipped outright - the one shape where "not
    # there" and "there and wrong" are the same syscall away.
    if [ -L "$path" ]; then
        echo "avocado: $rel is a symlink. This check would read the link's" >&2
        echo "avocado: target on the build host and the device would read" >&2
        echo "avocado: whatever the same path resolves to at boot, so a" >&2
        echo "avocado: match here says nothing. Ship it as a regular file." >&2
        exit 1
    fi

    if [ ! -e "$path" ]; then
        if [ "$required" = no ]; then
            # A machine with no key-wrapping engine ships no var-hwkey.sh and
            # that is correct. One that DOES ship it still has to attest it,
            # which is why absence is skipped here and not the whole component.
            return 0
        fi
        echo "avocado: this sysroot ships$shipped but no $rel. They install" >&2
        echo "avocado: from one package, so the directory was edited after" >&2
        echo "avocado: packaging. Nothing calls the provider without" >&2
        echo "avocado: cryptsetup-var.sh, and cryptsetup-var.sh has nothing" >&2
        echo "avocado: to call without var-key.sh, so /var never unlocks." >&2
        exit 1
    fi

    # The leaf is a regular file by here, so what this still catches is a
    # symlinked PARENT DIRECTORY - the shape a leaf-only guard reads straight
    # through, and the one the build-side tiers had to be widened for too.
    case "$(readlink -f "$path")" in
        "$SYSROOT"/*) : ;;
        *)
        echo "avocado: $rel resolves outside the sysroot; refusing to ship it" >&2
        exit 1 ;;
    esac

    if [ ! -f "$path.sha256" ]; then
        echo "avocado: $rel has no attestation beside it. cryptsetup-var" >&2
        echo "avocado: writes one for every script on the unlock path, only" >&2
        echo "avocado: after its deliverability checks pass, so its absence" >&2
        echo "avocado: means those checks did not run over this file." >&2
        exit 1
    fi

    recorded=$(cat "$path.sha256")
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    if [ "$recorded" != "$actual" ]; then
        echo "avocado: $rel is NOT the file the build validated." >&2
        echo "avocado: attested $recorded" >&2
        echo "avocado: shipped  $actual" >&2
        echo "avocado: whatever it declares about itself, it has not been" >&2
        echo "avocado: shown to derive a device-unique key." >&2
        exit 1
    fi
    return 0
}}

# Only ever one component is reported: every refusal exits, so the first one
# wrong is the last thing printed. Order therefore decides WHICH failure an
# operator sees, and cryptsetup-var.sh is the script an attacker substitutes.
for _entry in $COMPONENTS; do
    check_component "${{_entry%:*}}" "${{_entry##*:}}"
done

# Attesting the unit is not enough on its own: the same edit that repoints
# ExecStart can instead delete the symlink that pulls the unit into the initrd,
# which leaves every digest matching and the unit simply never started. The
# build stages that link by hand because the systemd preset does not create it
# for a WantedBy=initrd-root-fs.target unit, so its absence is never
# legitimate when the unit itself is present.
LINK="$UNITDIR/initrd-root-fs.target.wants/cryptsetup-var.service"
if [ -e "$SYSROOT/$UNITDIR/cryptsetup-var.service" ] \
    && [ ! -e "$SYSROOT/$LINK" ] && [ ! -L "$SYSROOT/$LINK" ]; then
    echo "avocado: this sysroot ships cryptsetup-var.service but not the" >&2
    echo "avocado: $LINK symlink" >&2
    echo "avocado: that pulls it into the initrd. Every digest still matches" >&2
    echo "avocado: and /var is simply never unlocked." >&2
    exit 1
fi
"#
    )
}

/// Install a sysroot (rootfs or initramfs) via DNF into the SDK container volume.
///
/// This is the shared implementation used by `avocado rootfs install`,
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

    // Get packages from config (the effective set — absent config yields the
    // default meta-package).
    let Some(packages) = sysroot_packages(
        params.config,
        &params.sysroot_type,
        params.target,
        params.parsed,
    ) else {
        unreachable!("sysroot_type was narrowed to rootfs/initramfs above")
    };

    // Short-circuit: nothing to do when the stamp on record still matches the
    // current inputs. This is checked before any container call, so an
    // unchanged project pays nothing here — no kernel repoquery, no dnf
    // transaction, no lock rewrite.
    //
    // Escape hatches: `--no-stamps` skips the read (and the write) so it
    // always reinstalls, and `avocado {rootfs,initramfs} clean` removes the
    // stamp along with the sysroot, so a cleaned sysroot never skips.
    if !params.no_stamps {
        if let Some(stamp) = params.prefetched_stamp.as_ref() {
            if let Some(inputs) = compute_install_stamp_inputs(params, &packages)? {
                if stamp.is_current(&inputs) {
                    print_success(
                        &format!("{label} sysroot is up to date."),
                        OutputLevel::Normal,
                    );
                    return Ok(());
                }
            }
        }
    }

    print_info(&format!("Installing {label} sysroot."), OutputLevel::Normal);

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

    let (resolved_kver, off_kernel_excludes) = {
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
        let kver = resolve_and_pin_kernel_version(&mut resolve_params).await?;
        let excludes = match kver.as_deref() {
            Some(k) => off_kernel_dnf_excludes(&resolve_params, k).await?,
            None => Vec::new(),
        };
        (kver, excludes)
    };

    // Build package specs for all configured packages. When we have a
    // resolved kernel version, substitute any `{{ avocado.kernel.version }}`
    // templates in package keys so BSP yamls can produce fully-versioned
    // kernel-family names without silent rewriting.
    let resolve_name = |name: &str| -> String {
        match resolved_kver.as_deref() {
            Some(kver) => substitute_kernel_version(name, kver),
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
            match package_exists_in_target_repo(params, &name).await? {
                Some(true) => {
                    print_info(
                        &format!("Auto-including {name} for pinned kernel {kver}"),
                        OutputLevel::Normal,
                    );
                    Some(name)
                }
                Some(false) => {
                    print_info(
                        &format!(
                            "Skipping {name}: not found in feed (feed predates per-kernel module packagegroups)"
                        ),
                        OutputLevel::Normal,
                    );
                    None
                }
                // Probe failed. Keeping the name is the safe answer: it stays in
                // the effective set, so a package the lock already records does
                // not read as a removal and trigger a wipe that reinstalls
                // without it. If it genuinely is absent, dnf says so and fails
                // loudly instead of silently shipping a module-less kernel.
                None => {
                    print_warning(
                        &format!(
                            "Could not probe the feed for {name} (repoquery failed). \
                             Keeping it in the install set rather than assuming it is absent."
                        ),
                        OutputLevel::Normal,
                    );
                    Some(name)
                }
            }
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

    // The set of names a completed install of this sysroot is expected to
    // have recorded in the lockfile: config-declared packages plus whatever
    // was auto-appended above. Used as the reference set for removal
    // detection and, after the install, as the lock query list.
    let mut effective_names: Vec<String> = if packages.is_empty() {
        vec![default_pkg.to_string()]
    } else {
        packages.keys().cloned().collect()
    };
    effective_names.extend(auto_module_pkg.iter().cloned());
    effective_names.extend(auto_kernel_image_pkg.iter().cloned());
    effective_names.sort();
    effective_names.dedup();
    let effective_name_set: HashSet<String> = effective_names.iter().cloned().collect();

    // Decide whether this install has to start from an empty sysroot. dnf
    // install is additive-only, so anything that makes the *existing*
    // sysroot contents wrong — rather than merely incomplete — needs a wipe
    // first. Two things qualify, and both resolve to the same single clean.
    let kernel_pin_change = resolved_kver.as_deref().and_then(|new_kver| {
        prev_pinned_kver
            .filter(|prev| prev != new_kver)
            .map(|prev| (prev, new_kver))
    });
    let removed_packages = if kernel_pin_change.is_some() {
        // Moot: the whole package map is about to be cleared below.
        Vec::new()
    } else {
        detect_sysroot_package_removals(
            &effective_name_set,
            &params.sysroot_type,
            params.target,
            params.lock_file,
        )
    };

    let needs_clean_reinstall = kernel_pin_change.is_some() || !removed_packages.is_empty();

    if let Some((prev, new_kver)) = kernel_pin_change {
        // A plain re-install would land the new kernel-image and module
        // packagegroup *alongside* the prior pin's packages, leaving
        // /lib/modules/<old-kver>/, the old kernel-image and stale module
        // packages behind.
        print_info(
            &format!(
                "{label}: kernel pin changed ({prev} -> {new_kver}); cleaning sysroot for fresh install"
            ),
            OutputLevel::Normal,
        );

        // Wipe the package state for this sysroot so a failed re-install
        // can't leave a stale package map pointing at a now-empty sysroot.
        match params.sysroot_type {
            SysrootType::Rootfs => params.lock_file.clear_rootfs(params.target),
            SysrootType::Initramfs => params.lock_file.clear_initramfs(params.target),
            _ => {}
        }
        // Remove and immediately re-pin the new kver. Remove first so the
        // entry is correct even if the install below fails (empty sysroot +
        // correct kver = retry without re-clean). Re-pin so the
        // sdk/install.rs merge site can see the new kver after a successful
        // install — without it the `if let Some(kver)` check in that merge
        // finds nothing and the old kver from the initial clone bleeds
        // through into the saved lockfile.
        params
            .lock_file
            .remove_kernel_version(params.target, &params.sysroot_type);
        params
            .lock_file
            .set_kernel_version(params.target, &params.sysroot_type, new_kver);
    } else if !removed_packages.is_empty() {
        print_info(
            &format!(
                "Packages removed from {label}: {}. Cleaning sysroot for fresh install.",
                removed_packages.join(", ")
            ),
            OutputLevel::Normal,
        );
        // Drop only the stale entries, preserving version pins for the
        // packages that remain.
        params.lock_file.remove_packages_from_sysroot(
            params.target,
            &params.sysroot_type,
            &removed_packages,
        );
    }

    // A failed wipe is not fatal — the reinstall below still repairs the common
    // case — but it must not be latched by a current stamp, since dnf installs
    // additively and the old contents (an old kernel's modules, say) survive.
    let clean_ok = if needs_clean_reinstall {
        clean_sysroot(params, sysroot_dir).await
    } else {
        true
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

    let yes = if params.force { "-y" } else { "" };
    let dnf_args_str = if let Some(args) = &params.dnf_args {
        format!(" {} ", args.join(" "))
    } else {
        String::new()
    };

    // Build optional overlay snippet — appended to the install command so it
    // runs in the same container invocation immediately after DNF finishes.
    let overlay_snippet = {
        let overlay_value = params.parsed.and_then(|parsed| {
            let key = match params.sysroot_type {
                SysrootType::Rootfs => "rootfs",
                SysrootType::Initramfs => "initramfs",
                _ => return None,
            };
            parsed.get(key)?.get("overlay").cloned()
        });
        if let (Some(v), Some(parsed)) = (overlay_value, params.parsed) {
            let (dir, opaque) = parse_overlay_config(&v);
            // Opt-in preprocessing: materialize an interpolated copy of the
            // overlay on the host under `.avocado/overlay-staging/` and copy
            // from there, so `{{ ... }}` in overlay files is substituted without
            // mutating the working tree.
            let spec = crate::utils::overlay_preprocess::PreprocessSpec::from_overlay_value(&v);
            let effective_dir = if spec.is_enabled() {
                let context = crate::utils::interpolation::AvocadoContext::from_main_config(
                    parsed,
                    Some(params.target),
                    params.target_board,
                );
                match crate::utils::overlay_preprocess::materialize_preprocessed_overlay(
                    params.src_dir,
                    &dir,
                    sysroot_dir,
                    &spec,
                    parsed,
                    &context,
                )? {
                    Some(staging_rel_dir) => staging_rel_dir,
                    None => dir,
                }
            } else {
                dir
            };
            build_overlay_script(&effective_dir, opaque, sysroot_dir)
        } else {
            String::new()
        }
    };

    let exclude_str = if off_kernel_excludes.is_empty() {
        String::new()
    } else {
        off_kernel_excludes.join(" ")
    };

    // No version pins recorded for this sysroot means "resolve fresh": the
    // first install, or `avocado update` just cleared them so the next install
    // moves to the newest feed contents. `dnf install` alone cannot do that on
    // an existing sysroot - it is additive and leaves already-installed
    // dependencies at whatever version they have - so follow it with a
    // distro-sync in the same container run. With pins present the lock is
    // authoritative and nothing is synced.
    let fresh_resolve = params
        .lock_file
        .get_locked_package_names(params.target, &params.sysroot_type)
        .is_empty();
    // dnf keeps its own expiry bookkeeping (default 48 h) in the SDK's
    // persistdir, so a feed whose contents changed under the same URL - a dev
    // repo rebuilt in place, a channel head between snapshots - stays invisible
    // however the cache directory is groomed: every run reports "Last metadata
    // expiration check: 6:55:04 ago" and resolves against the old package set.
    // When there is nothing pinned to be reproducible about, ask dnf to re-read
    // the metadata instead of guessing whether it is stale.
    let refresh = if fresh_resolve { "--refresh" } else { "" };
    // The sync follows the feed for everything already installed, which would
    // walk a package the config pins to an explicit version straight to the
    // repo's latest, one line after the install placed the requested one. Hold
    // those out of the sync; a wildcard is a request to follow the feed and
    // stays in. Sorted so the command is stable across runs.
    let mut sync_excludes: Vec<String> = off_kernel_excludes.clone();
    let mut pinned: Vec<String> = packages
        .iter()
        .filter(|(_, version)| is_explicit_version(version.as_str()))
        .map(|(name, _)| format!("--exclude={}", resolve_name(name)))
        .collect();
    pinned.sort();
    sync_excludes.extend(pinned);
    let sync_exclude_str = sync_excludes.join(" ");
    let sync_snippet = dnf_sync_step(
        fresh_resolve,
        sysroot_dir,
        &dnf_args_str,
        yes,
        &sync_exclude_str,
    );
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
    {dnf_args_str} {refresh} {yes} {exclude_str} --installroot $AVOCADO_PREFIX/{sysroot_dir} install {pkg}
{sync_snippet}{overlay_snippet}"#
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
        // Packages are on disk: the lock's kernel pin and cleared sections now
        // describe it, whether or not the version query below succeeds. Tell
        // the caller before anything else can fail.
        params.pins_recorded = true;

        // Query installed versions for ALL config packages and update lock file
        let installed_versions = params
            .container_helper
            .query_installed_packages(
                &params.sysroot_type,
                &effective_names,
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

        // The stamp must not outlive the two things it implicitly claims: that the
        // lock records what landed, and that the kernel sysroot was staged. Both
        // can fail without failing the install -- `query_installed_packages`
        // returns Ok(empty) on error, and staging only prints. Writing a current
        // stamp anyway latches the broken state: every later run reports "up to
        // date" and the user is wedged until they guess --no-stamps.
        let versions_recorded = !installed_versions.is_empty();
        let mut kernel_staging_error: Option<String> = None;
        let install_is_clean = clean_ok;

        if versions_recorded {
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
            // Persisting is the caller's job. The rootfs and initramfs tasks
            // run concurrently on separate lockfile clones, so saving a clone
            // here is last-writer-wins against the sibling task; the caller
            // merges both and saves once.
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
                    kernel_staging_error = Some(e.to_string());
                }
            }
        }

        // Write install stamp (unless --no-stamps or no parsed config available).
        //
        // Computed fresh rather than reusing the value the skip check above
        // derived: the install just re-pinned this sysroot's packages, and the
        // stamp has to record the lock state the *next* run will compare
        // against.
        // Fail rather than return a success the stamp contradicts.
        //
        // Each of these means the sysroot on disk is not the one the config
        // asked for, so no stamp is written -- and `runtime build` refuses to
        // build without one. Reporting the step as succeeded left the two
        // commands disagreeing: a green `install` followed by `build` naming
        // `rootfs install` as missing, with `avocado rootfs install` as the
        // suggested remedy -- the command that had just "succeeded". Nothing in
        // that loop names the actual fault, so it reads as user error, and the
        // reason (a `print_warning`) is invisible whenever the TUI is active,
        // which is the default for `avocado install`.
        //
        // Failing here surfaces the reason through the failed-task rendering
        // and stops the loop at the command that can still explain itself.
        // `--no-stamps` does not suppress it: these are install failures, and
        // the stamp is only how they became visible.
        if let Some(reason) = incomplete_install_reason(
            install_is_clean,
            versions_recorded,
            kernel_staging_error.as_deref(),
        ) {
            return Err(incomplete_install_error(label, &reason));
        }

        // This one IS `?`, unlike the stamp write below. A sysroot whose
        // var-key provider does not match the attestation the build wrote is
        // not a recording problem - it is a sysroot that must not ship, and
        // letting it through here is the whole gap this check exists to close.
        // Placed before the stamp so a refused sysroot cannot be recorded
        // fresh and skipped on the next run.
        verify_var_key_attestation(params, sysroot_dir).await?;

        // Deliberately not `?`. Unlike the checks above -- which report a
        // sysroot that is not what the config asked for -- a stamp-write
        // failure leaves a correct sysroot that simply is not recorded, and the
        // next run reinstalls. Failing here would add nothing and cost the
        // teardown the callers run on the way out.
        //
        // Reaching this line means `incomplete_install_reason` returned None.
        if !params.no_stamps {
            if let Err(e) = write_install_stamp(params, &packages, label).await {
                print_warning(
                    &format!(
                        "Installed {label} sysroot but could not record its install stamp: {e}. \
                         The next run will reinstall rather than report it up to date."
                    ),
                    OutputLevel::Normal,
                );
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
    target_board: Option<String>,
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

        let container_helper = SdkContainer::from_config(&self.config_path, config)?
            .verbose(self.verbose)
            .with_cli_target_board(self.target_board.clone());

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
            &SysrootType::Rootfs,
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
                    sysroot_type: SysrootType::Rootfs,
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

        // Persist the lockfile the install updated. `install_sysroot` no
        // longer saves for itself — under `avocado sdk install` it runs on a
        // clone that the caller merges and saves once. Folded into `result`
        // rather than `?` so a save failure still reaches the teardown below.
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
                    // stderr, not print_error: under --json print_error is
                    // suppressed and this notice must survive.
                    print_warning_stderr(&format!("Failed to save lock file: {save_err}"));
                }
                Err(e)
            }
            (Err(e), false) => Err(e),
        };

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

#[cfg(test)]
mod tests {
    use super::generate_var_key_attestation_script;

    /// The initramfs must carry a provider; a rootfs legitimately ships the
    /// udev and posture packages without one. Getting this backwards fails
    /// every rootfs build on a declaring machine, so the two scripts must
    /// actually differ on that branch and agree everywhere else.
    #[test]
    fn attestation_script_requires_a_provider_only_for_the_initramfs() {
        let initramfs = generate_var_key_attestation_script("initramfs", true);
        let rootfs = generate_var_key_attestation_script("rootfs", false);

        assert!(initramfs.contains("declares encrypted-var but ships no"));
        assert!(!rootfs.contains("declares encrypted-var but ships no"));

        // Everything after the absent-provider branch is shared, and is what
        // validates a provider that IS present - so a rootfs that ships one is
        // still checked rather than waved through.
        for script in [&initramfs, &rootfs] {
            assert!(script.contains("is NOT the file the build validated"));
            assert!(script.contains("has no attestation beside it"));
            assert!(script.contains("resolves outside the sysroot"));
            assert!(script.contains("grep -qw 'encrypted-var'"));
        }

        assert!(initramfs.contains("$AVOCADO_PREFIX/initramfs"));
        assert!(rootfs.contains("$AVOCADO_PREFIX/rootfs"));
    }

    /// Everything above asserts on the TEXT of the generated script, which is
    /// worth exactly as much as a mutation can prove. Measured: replacing the
    /// whole `check_component` body with `return 0` left both text tests green,
    /// so the gate could be gutted without failing anything.
    ///
    /// This one runs it. `sh` against real fixture trees, one case per decision
    /// branch, because every defect found in this script - the dangling-symlink
    /// misclassification, the non-canonical prefix false refusal, the provider
    /// gating the whole check - was found by executing it and none of them was
    /// visible in its text.
    #[test]
    fn attestation_script_refuses_each_tampered_sysroot() {
        use std::os::unix::fs::symlink;

        // (case name, sysroot kind, mutate, expected refusal)
        // `None` expects acceptance.
        /// One executing case: what it is called, whether the sysroot is an
        /// initramfs, how to damage it, and the refusal it must produce
        /// (`None` meaning the sysroot must be accepted).
        type Case = (
            &'static str,
            bool,
            fn(&std::path::Path),
            Option<&'static str>,
        );

        let cases: Vec<Case> = vec![
            ("clean initramfs", true, |_| {}, None),
            (
                "unlock script tampered",
                true,
                |d| {
                    std::fs::write(d.join("cryptsetup-var.sh"), "#!/bin/sh\nevil\n").unwrap();
                },
                Some("is NOT the file the build validated"),
            ),
            (
                "unlock script unattested",
                true,
                |d| std::fs::remove_file(d.join("cryptsetup-var.sh.sha256")).unwrap(),
                Some("has no attestation beside it"),
            ),
            (
                "provider tampered",
                true,
                |d| {
                    std::fs::write(d.join("var-key.sh"), "#!/bin/sh\nevil\n").unwrap();
                },
                Some("is NOT the file the build validated"),
            ),
            (
                "optional backend present and unattested",
                true,
                |d| std::fs::write(d.join("var-hwkey.sh"), "#!/bin/sh\n").unwrap(),
                Some("has no attestation beside it"),
            ),
            (
                "unit file tampered",
                true,
                |d| {
                    let units = d
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("usr/lib/systemd/system");
                    std::fs::write(
                        units.join("cryptsetup-var.service"),
                        "[Unit]\nConditionPathExists=/nowhere\n",
                    )
                    .unwrap();
                },
                Some("is NOT the file the build validated"),
            ),
            (
                "unit enabled-symlink deleted",
                true,
                |d| {
                    let units = d
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("usr/lib/systemd/system");
                    std::fs::remove_file(
                        units.join("initrd-root-fs.target.wants/cryptsetup-var.service"),
                    )
                    .unwrap();
                },
                Some("never unlocked"),
            ),
            (
                "component is a symlink",
                true,
                |d| {
                    std::fs::remove_file(d.join("var-key.sh")).unwrap();
                    symlink("/nowhere/var-key.sh", d.join("var-key.sh")).unwrap();
                },
                Some("is a symlink"),
            ),
        ];

        for (name, initramfs, mutate, want) in cases {
            let root = tempfile::tempdir().unwrap();
            let dir = build_attestation_fixture(root.path(), initramfs);
            mutate(&dir);
            let (code, err) = run_attestation_script(root.path(), initramfs);
            match want {
                None => assert_eq!(code, 0, "{name}: expected accept, stderr: {err}"),
                Some(needle) => {
                    assert_eq!(code, 1, "{name}: expected refusal, stderr: {err}");
                    assert!(err.contains(needle), "{name}: wrong branch, stderr: {err}");
                }
            }
        }
    }

    /// A rootfs shipping the unlock script but no provider must still be
    /// checked. The first version gated every component on `var-key.sh`, so
    /// deleting it disarmed the `cryptsetup-var.sh` check entirely - measured
    /// rc=0 on a sysroot whose unlock script had been substituted.
    #[test]
    fn a_rootfs_unlock_script_is_checked_without_a_provider() {
        let root = tempfile::tempdir().unwrap();
        let dir = build_attestation_fixture(root.path(), false);
        std::fs::remove_file(dir.join("var-key.sh")).unwrap();
        std::fs::remove_file(dir.join("var-key.sh.sha256")).unwrap();

        let (code, err) = run_attestation_script(root.path(), false);
        assert_eq!(code, 1, "expected refusal, stderr: {err}");
        assert!(err.contains("but no"), "wrong branch, stderr: {err}");

        // And a rootfs shipping none of it is the legitimate case.
        let bare = tempfile::tempdir().unwrap();
        let caps = bare.path().join("rootfs/etc");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(
            caps.join("avocado-security-capabilities"),
            "encrypted-var\n",
        )
        .unwrap();
        let (code, err) = run_attestation_script(bare.path(), false);
        assert_eq!(code, 0, "bare rootfs should pass, stderr: {err}");
    }

    /// A non-canonical `AVOCADO_PREFIX` must not refuse a clean sysroot. The
    /// parent-directory check compared `readlink -f` output against the raw
    /// path, so a symlinked component or a trailing slash refused every
    /// component with "resolves outside the sysroot" - measured on a tree with
    /// no symlink in it.
    #[test]
    fn a_non_canonical_prefix_does_not_refuse_a_clean_sysroot() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        build_attestation_fixture(&real, true);

        let (code, err) = run_attestation_script(&real, true);
        assert_eq!(code, 0, "canonical prefix, stderr: {err}");

        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        let (code, err) = run_attestation_script(&link, true);
        assert_eq!(code, 0, "symlinked prefix, stderr: {err}");

        // A prefix with no sysroot under it must exit 0 silently, not die on
        // `readlink -f`. Under `set -e` that produced rc=1 with an empty
        // stderr, which the caller renders as a failed attestation carrying no
        // reason - strictly worse than the silent pass it replaced.
        let absent = tempfile::tempdir().unwrap();
        let (code, err) = run_attestation_script(absent.path(), true);
        assert_eq!(code, 0, "absent sysroot, stderr: {err}");
        assert!(err.is_empty(), "absent sysroot should be silent: {err}");

        let dangling = root.path().join("dangling");
        symlink(root.path().join("nowhere"), &dangling).unwrap();
        let (code, err) = run_attestation_script(&dangling, true);
        assert_eq!(code, 0, "dangling prefix, stderr: {err}");

        let trailing = format!("{}/", real.display());
        let (code, err) = run_attestation_script(std::path::Path::new(&trailing), true);
        assert_eq!(code, 0, "trailing slash, stderr: {err}");
    }

    /// Writes a sysroot the script should accept: the capability declared, and
    /// both required scripts present with matching attestations.
    fn build_attestation_fixture(prefix: &std::path::Path, initramfs: bool) -> std::path::PathBuf {
        let sysroot = prefix.join(if initramfs { "initramfs" } else { "rootfs" });
        let dir = sysroot.join("usr/libexec/cryptsetup-var");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(sysroot.join("etc")).unwrap();
        std::fs::write(
            sysroot.join("etc/avocado-security-capabilities"),
            "encrypted-var\n",
        )
        .unwrap();
        for name in ["cryptsetup-var.sh", "var-key.sh"] {
            let body = format!("#!/bin/sh\n# {name}\n");
            std::fs::write(dir.join(name), &body).unwrap();
            std::fs::write(
                dir.join(format!("{name}.sha256")),
                format!("{}\n", sha256_hex(body.as_bytes())),
            )
            .unwrap();
        }

        // The unit and the symlink that enables it. The build stages the link
        // by hand because the preset does not create one for a
        // WantedBy=initrd-root-fs.target unit.
        let units = sysroot.join("usr/lib/systemd/system");
        let wants = units.join("initrd-root-fs.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        let unit = "[Unit]\nConditionPathExists=/etc/avocado/var-encrypt\n";
        std::fs::write(units.join("cryptsetup-var.service"), unit).unwrap();
        std::fs::write(
            units.join("cryptsetup-var.service.sha256"),
            format!("{}\n", sha256_hex(unit.as_bytes())),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../cryptsetup-var.service",
            wants.join("cryptsetup-var.service"),
        )
        .unwrap();
        dir
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn run_attestation_script(prefix: &std::path::Path, initramfs: bool) -> (i32, String) {
        let script = generate_var_key_attestation_script(
            if initramfs { "initramfs" } else { "rootfs" },
            initramfs,
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("AVOCADO_PREFIX", prefix)
            .output()
            .expect("sh");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// The provider is the smaller target. cryptsetup-var.sh decides whether
    /// the provider is called at all and what happens to the 64 bytes it
    /// returns, so a check that binds only var-key.sh passes a sysroot whose
    /// unlock script was replaced with one that writes a constant key.
    ///
    /// Asserting on the invocation lines rather than on the shared function
    /// body: the body is written once, so a single `contains` there stays true
    /// after a component is dropped from the call list, which is exactly the
    /// regression this pins.
    #[test]
    fn attestation_script_checks_every_script_on_the_unlock_path() {
        let script = generate_var_key_attestation_script("initramfs", true);

        assert!(script.contains("$DIR/cryptsetup-var.sh:yes"));
        assert!(script.contains("$DIR/var-key.sh:yes"));
        // Optional in one direction only: a machine with no key-wrapping
        // engine ships no var-hwkey.sh, but one that ships it must attest it.
        assert!(script.contains("$DIR/var-hwkey.sh:no"));
        // The unit decides whether any of the above runs at all.
        assert!(script.contains("$UNITDIR/cryptsetup-var.service:yes"));
    }

    use super::{
        build_overlay_script, detect_sysroot_package_removals, incomplete_install_error,
        incomplete_install_reason, parse_probe_output,
    };
    use crate::utils::lockfile::{LockFile, SysrootType};
    use std::collections::{HashMap, HashSet};

    // A sysroot install that places its packages but cannot finish the job must
    // not report success: no stamp is written for it, and `runtime build`
    // refuses to build without one. Returning Ok here is what produced a green
    // `avocado install` followed by `avocado build` reporting `rootfs install`
    // as missing and suggesting the very command that had just passed.
    #[test]
    fn a_complete_install_has_no_reason_to_fail() {
        assert_eq!(incomplete_install_reason(true, true, None), None);
    }

    #[test]
    fn kernel_staging_failure_is_reported_with_its_cause() {
        let reason = incomplete_install_reason(
            true,
            true,
            Some("Failed to stage kernel sysroot for kernel-version '6.18.37'"),
        )
        .expect("staging failure must not be silent");
        // The underlying error is carried, not swallowed — it is the only part
        // that names which kernel version had no image.
        assert!(reason.contains("6.18.37"), "{reason}");
        assert!(reason.contains("/boot"), "{reason}");
    }

    #[test]
    fn an_unreadable_package_list_and_an_unclean_sysroot_each_fail() {
        assert!(incomplete_install_reason(true, false, None)
            .expect("unread versions must not be silent")
            .contains("package versions"));
        assert!(incomplete_install_reason(false, true, None)
            .expect("an unclean sysroot must not be silent")
            .contains("cleaned"));
    }

    #[test]
    fn an_unclean_sysroot_outranks_the_later_signals() {
        // Order matters: a sysroot that could not be cleaned may hold packages
        // from a previous config, which makes the other two signals describe
        // the wrong tree. Report the cause, not a symptom of it.
        let reason = incomplete_install_reason(false, false, Some("staging blew up"))
            .expect("must not be silent");
        assert!(reason.contains("cleaned"), "{reason}");
        assert!(!reason.contains("staging blew up"), "{reason}");
    }

    #[test]
    fn the_failure_names_the_sysroot_and_the_remedy_loop_it_breaks() {
        let msg = incomplete_install_error("rootfs", "the kernel sysroot could not be staged")
            .to_string();
        assert!(msg.contains("rootfs"), "{msg}");
        // Says the packages did land — otherwise it reads as "nothing worked".
        assert!(msg.contains("Installed the rootfs sysroot"), "{msg}");
        // And ties the install-time fault to the build-time symptom, so the
        // user does not go looking for a mistake of their own.
        assert!(msg.contains("No install stamp"), "{msg}");
        assert!(msg.contains("avocado build"), "{msg}");
    }

    const KVER: &str = "6.8.12-l4t-r39.2.0-1021.21";
    const TARGET: &str = "jetson-agx-thor";

    fn lock_with(sysroot: &SysrootType, names: &[&str]) -> LockFile {
        let mut lock = LockFile::new();
        let versions: HashMap<String, String> = names
            .iter()
            .map(|n| (n.to_string(), "2026.9-r0.0".to_string()))
            .collect();
        lock.update_sysroot_versions(TARGET, sysroot, versions);
        lock
    }

    fn name_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn fresh_resolve_adds_a_distro_sync_after_install_and_pins_do_not() {
        use super::dnf_sync_step;
        use super::is_explicit_version;

        // A configured `foo: "1.0"` must be held out of the sync, or the sync
        // moves it to the feed's latest one line after the install placed 1.0.
        assert!(is_explicit_version(Some("1.0")));
        assert!(is_explicit_version(Some("1.0-r2.0")));
        assert!(
            !is_explicit_version(Some("*")),
            "a wildcard follows the feed"
        );
        assert!(!is_explicit_version(Some(" * ")), "whitespace is not a pin");
        assert!(!is_explicit_version(Some("")));
        assert!(!is_explicit_version(None));

        let fresh = dnf_sync_step(true, "rootfs", "--best", "-y", "--exclude=foo");
        assert!(
            fresh.contains("--installroot $AVOCADO_PREFIX/rootfs distro-sync"),
            "{fresh}"
        );
        assert!(
            fresh.contains("--best --refresh -y --exclude=foo"),
            "same args and excludes as the install: {fresh}"
        );
        assert!(
            fresh.contains("RPM_ETCCONFIGDIR=\"$DNF_SDK_TARGET_PREFIX\""),
            "same environment: {fresh}"
        );
        assert!(
            dnf_sync_step(false, "rootfs", "--best", "-y", "").is_empty(),
            "pins present: the lock rules"
        );
    }

    #[test]
    fn removal_detection_ignores_auto_appended_kernel_packages() {
        // Regression test for the bug that made `avocado sdk install` wipe and
        // reinstall both sysroots on every run. The lock shape below is
        // verbatim from references/jetson-trt/avocado.lock: a default config
        // (which declares only the meta-package) plus the two packages
        // install_sysroot auto-appends for a pinned kernel. Comparing the lock
        // against *config* names alone reads those two as removed forever.
        let rootfs_lock = lock_with(
            &SysrootType::Rootfs,
            &[
                "avocado-pkg-rootfs",
                &format!("kernel-image-{KVER}"),
                &format!("packagegroup-avocado-rootfs-modules-{KVER}"),
            ],
        );
        let rootfs_effective = name_set(&[
            "avocado-pkg-rootfs",
            &format!("kernel-image-{KVER}"),
            &format!("packagegroup-avocado-rootfs-modules-{KVER}"),
        ]);
        assert!(
            detect_sysroot_package_removals(
                &rootfs_effective,
                &SysrootType::Rootfs,
                TARGET,
                &rootfs_lock,
            )
            .is_empty(),
            "steady-state rootfs must not report removals"
        );

        // Initramfs gets the module packagegroup but no kernel-image.
        let initramfs_lock = lock_with(
            &SysrootType::Initramfs,
            &[
                "avocado-pkg-initramfs",
                &format!("packagegroup-avocado-initramfs-modules-{KVER}"),
            ],
        );
        let initramfs_effective = name_set(&[
            "avocado-pkg-initramfs",
            &format!("packagegroup-avocado-initramfs-modules-{KVER}"),
        ]);
        assert!(
            detect_sysroot_package_removals(
                &initramfs_effective,
                &SysrootType::Initramfs,
                TARGET,
                &initramfs_lock,
            )
            .is_empty(),
            "steady-state initramfs must not report removals"
        );
    }

    #[test]
    fn removal_detection_flags_genuinely_dropped_config_package() {
        let lock = lock_with(
            &SysrootType::Rootfs,
            &["avocado-pkg-rootfs", "vim", &format!("kernel-image-{KVER}")],
        );
        // `vim` was removed from rootfs.packages; the auto-appends still apply.
        let effective = name_set(&["avocado-pkg-rootfs", &format!("kernel-image-{KVER}")]);

        assert_eq!(
            detect_sysroot_package_removals(&effective, &SysrootType::Rootfs, TARGET, &lock),
            vec!["vim".to_string()],
        );
    }

    #[test]
    fn removal_detection_flags_stale_kernel_auto_appends() {
        // A kernel repin leaves the previous kver's auto-appended packages in
        // the lock. Those genuinely are stale and must force a clean, so the
        // new pin isn't installed alongside the old one.
        let lock = lock_with(
            &SysrootType::Rootfs,
            &[
                "avocado-pkg-rootfs",
                &format!("kernel-image-{KVER}"),
                &format!("packagegroup-avocado-rootfs-modules-{KVER}"),
            ],
        );
        let new_kver = "6.8.12-l4t-r39.2.0-9999.99";
        let effective = name_set(&[
            "avocado-pkg-rootfs",
            &format!("kernel-image-{new_kver}"),
            &format!("packagegroup-avocado-rootfs-modules-{new_kver}"),
        ]);

        assert_eq!(
            detect_sysroot_package_removals(&effective, &SysrootType::Rootfs, TARGET, &lock),
            vec![
                format!("kernel-image-{KVER}"),
                format!("packagegroup-avocado-rootfs-modules-{KVER}"),
            ],
        );
    }

    #[test]
    fn removal_detection_is_noop_on_first_install() {
        // Nothing locked yet — every config package is about to be installed,
        // not removed.
        let lock = LockFile::new();
        let effective = name_set(&["avocado-pkg-rootfs"]);

        assert!(
            detect_sysroot_package_removals(&effective, &SysrootType::Rootfs, TARGET, &lock)
                .is_empty()
        );
    }

    /// The empty-lock case above passes for the wrong reason — it early-returns
    /// before comparing anything, so it survives inverting the `difference`
    /// operands. This one seeds the lock so the comparison actually runs, and
    /// pins the direction: locked-minus-effective, never the reverse.
    #[test]
    fn removal_detection_compares_locked_against_effective_not_the_reverse() {
        let lock = lock_with(&SysrootType::Rootfs, &["avocado-pkg-rootfs", "vim"]);

        // `vim` dropped from config: locked but no longer effective => removed.
        let effective = name_set(&["avocado-pkg-rootfs"]);
        assert_eq!(
            detect_sysroot_package_removals(&effective, &SysrootType::Rootfs, TARGET, &lock),
            vec!["vim".to_string()],
        );

        // The mirror image: a package that is effective but not yet locked is an
        // *addition*, and must never be reported. Inverting the operands makes
        // this arm return `curl` and fail.
        let effective = name_set(&["avocado-pkg-rootfs", "vim", "curl"]);
        assert!(
            detect_sysroot_package_removals(&effective, &SysrootType::Rootfs, TARGET, &lock)
                .is_empty(),
            "a newly added package must not read as a removal",
        );
    }

    /// A failed probe must never read as "absent from the feed": answering
    /// absent drops the module packagegroup, which then reads as a removal,
    /// wipes the sysroot, and reinstalls a kernel with no modules.
    #[test]
    fn probe_output_distinguishes_failure_from_absence() {
        assert_eq!(
            parse_probe_output("AVOCADO_PROBE_OK\npackagegroup-avocado-rootfs-modules-6.6.1\n"),
            Some(true),
        );
        // Ran, found nothing — genuinely absent.
        assert_eq!(parse_probe_output("AVOCADO_PROBE_OK\n\n"), Some(false));
        assert_eq!(parse_probe_output("AVOCADO_PROBE_OK\n"), Some(false));
        // repoquery failed — unknown, and must not collapse to `Some(false)`.
        assert_eq!(parse_probe_output("AVOCADO_PROBE_FAILED\n"), None);
        assert_eq!(parse_probe_output(""), None);
        // A login banner ahead of the sentinel must not defeat it, and one
        // ahead of a failure must not invent an answer.
        assert_eq!(
            parse_probe_output("Welcome to the builder!\nAVOCADO_PROBE_OK\nsome-pkg\n"),
            Some(true),
        );
        assert_eq!(
            parse_probe_output("Welcome to the builder!\nAVOCADO_PROBE_FAILED\n"),
            None,
        );
    }

    #[test]
    fn overlay_script_uses_cp_a_in_merge_mode() {
        // Merge mode now uses `cp -a` + `chown -R` (universally available
        // POSIX tools) instead of `rsync -a --chown` — the SDK doesn't
        // always ship rsync.
        let script = build_overlay_script("overlays/dev", false, "initramfs");
        assert!(
            script.contains("cp -a"),
            "merge-mode overlay script must use cp -a; got:\n{script}"
        );
        assert!(
            script.contains("chown -R root:root"),
            "merge-mode overlay script must chown the result; got:\n{script}"
        );
        assert!(script.contains("(merge mode)"));
        assert!(!script.contains("rsync"), "rsync should be gone");
    }

    #[test]
    fn overlay_script_uses_cp_r_in_opaque_mode() {
        let script = build_overlay_script("overlays/dev", true, "rootfs");
        assert!(script.contains("cp -r"));
        assert!(script.contains("(opaque mode)"));
    }

    #[test]
    fn overlay_script_renders_for_all_sysroots_and_modes() {
        // Cover the full {rootfs, initramfs} × {merge, opaque} matrix —
        // smoke test that the script renders with the right banner for
        // each combination.
        for sysroot_dir in ["rootfs", "initramfs"] {
            for opaque in [false, true] {
                let script = build_overlay_script("overlays/dev", opaque, sysroot_dir);
                assert!(script.contains(&format!(" to {sysroot_dir} sysroot ")));
            }
        }
    }
}
