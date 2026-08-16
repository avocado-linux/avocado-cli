//! Rootfs sysroot install command and shared install logic for rootfs/initramfs.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
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
    output::{print_error, print_info, print_success, print_warning, OutputLevel},
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

    let resolved = SysrootStampInputs {
        packages,
        repo_url: params.repo_url,
        repo_release: params.repo_release,
        disable_weak_dependencies: params.config.get_sdk_disable_weak_dependencies(),
        dnf_args: params.dnf_args.as_deref(),
        locked_packages: params
            .lock_file
            .get_sysroot_versions(params.target, &params.sysroot_type),
    };

    let inputs = match params.sysroot_type {
        SysrootType::Rootfs => {
            compute_rootfs_input_hash(parsed, params.src_dir, params.target_board, &resolved)?
        }
        SysrootType::Initramfs => {
            compute_initramfs_input_hash(parsed, params.src_dir, params.target_board, &resolved)?
        }
        _ => return Ok(None),
    };

    Ok(Some(inputs))
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
    let packages = match params.sysroot_type {
        SysrootType::Rootfs => params.config.get_rootfs_packages(),
        SysrootType::Initramfs => params.config.get_initramfs_packages(),
        _ => unreachable!(),
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
    {dnf_args_str} {yes} {exclude_str} --installroot $AVOCADO_PREFIX/{sysroot_dir} install {pkg}
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
        let mut kernel_staging_ok = true;
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
                    print_error(
                        &format!(
                            "Kernel sysroot staging failed: {e}. \
                             provision may fall back to reading the Image from the rootfs sysroot."
                        ),
                        OutputLevel::Normal,
                    );
                    kernel_staging_ok = false;
                }
            }
        }

        // Write install stamp (unless --no-stamps or no parsed config available).
        //
        // Computed fresh rather than reusing the value the skip check above
        // derived: the install just re-pinned this sysroot's packages, and the
        // stamp has to record the lock state the *next* run will compare
        // against.
        let stamp_is_trustworthy = versions_recorded && kernel_staging_ok && install_is_clean;

        if !params.no_stamps && !stamp_is_trustworthy {
            print_warning(
                &format!(
                    "Not recording an install stamp for {label}: {}. \
                     The next run will reinstall rather than report it up to date.",
                    if !install_is_clean {
                        "the sysroot could not be cleaned first, so stale contents may remain"
                    } else if !versions_recorded {
                        "the installed package versions could not be read"
                    } else {
                        "kernel sysroot staging failed"
                    }
                ),
                OutputLevel::Normal,
            );
        }

        // Deliberately not `?` anywhere below. The pins recorded above are only
        // persisted by the caller when this function returns Ok, so propagating a
        // stamp-write failure would discard the pins for an install that already
        // landed — the next run would re-resolve the kernel against feed head with
        // no prev_pinned_kver to compare against and install additively on top.
        // A missing stamp is the benign outcome: the next run reinstalls.
        if !params.no_stamps && stamp_is_trustworthy {
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

        let src_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
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
        let result = match prefetched_stamp {
            Err(e) => Err(e),
            Ok(prefetched_stamp) => {
                install_sysroot(&mut SysrootInstallParams {
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
                })
                .await
            }
        };

        // Persist the lockfile the install updated. `install_sysroot` no
        // longer saves for itself — under `avocado sdk install` it runs on a
        // clone that the caller merges and saves once. Folded into `result`
        // rather than `?` so a save failure still reaches the teardown below.
        let result = match result {
            Ok(()) => lock_file.save(src_dir),
            Err(e) => Err(e),
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
    use super::{build_overlay_script, detect_sysroot_package_removals, parse_probe_output};
    use crate::utils::lockfile::{LockFile, SysrootType};
    use std::collections::{HashMap, HashSet};

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
