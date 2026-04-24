//! Orchestrates kernel version resolution at install time.
//!
//! Ties together [`crate::utils::kernel_version`] (constraint parsing + rpm
//! compare) with a dnf-repoquery helper that runs inside the SDK container and
//! with the lockfile (for pinning results across runs).
//!
//! Resolution precedence for a given `(target, sysroot)`:
//! 1. If the lockfile already has a pinned kernel for this sysroot → use it.
//! 2. If the runtime uses `kernel.compile` (custom-built kernel) → skip repo
//!    resolution; callers should pass `None` through to the rewriter so
//!    kernel-family package names pass unchanged.
//! 3. Otherwise compute the effective spec, run `dnf repoquery`, pick the
//!    highest matching KERNEL_VERSION, and pin it in the lockfile.

use anyhow::{Context, Result};

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    kernel_version::resolve_kernel_version,
    lockfile::{LockFile, SysrootType},
    output::{print_info, OutputLevel},
    runs_on::RunsOnContext,
};

/// Parameters for a kernel version resolution pass.
pub struct ResolveParams<'a> {
    pub container_helper: &'a SdkContainer,
    pub container_image: &'a str,
    pub target: &'a str,
    pub sysroot: SysrootType,
    /// Runtime name if this sysroot belongs to a runtime (for `kernel.compile`
    /// detection and per-runtime `kernel.version` precedence).
    pub runtime_name: Option<&'a str>,
    pub config: &'a Config,
    pub lock_file: &'a mut LockFile,
    pub repo_url: Option<&'a str>,
    pub repo_release: Option<&'a str>,
    pub merged_container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub runs_on_context: Option<&'a RunsOnContext>,
    pub sdk_arch: Option<&'a String>,
    pub verbose: bool,
    pub tui_context: Option<crate::utils::container::TuiContext>,
}

/// Resolve and pin the kernel version for the given sysroot. Returns:
/// - `Ok(Some(kver))` — the resolved KERNEL_VERSION to pin kernel-family
///   package names against
/// - `Ok(None)` — resolution was skipped (runtime builds its own kernel, or
///   no kernel packages apply). Callers should not rewrite names.
pub async fn resolve_and_pin_kernel_version(
    params: &mut ResolveParams<'_>,
) -> Result<Option<String>> {
    // 1. Runtime with a compile-built kernel opts out of repo resolution.
    if let Some(runtime) = params.runtime_name {
        if params.config.runtime_kernel_is_compiled(runtime) {
            if params.verbose {
                print_info(
                    &format!(
                        "Runtime '{runtime}' uses a compile-built kernel; skipping repo-based resolution."
                    ),
                    OutputLevel::Normal,
                );
            }
            return Ok(None);
        }
    }

    // 2. Lockfile already pinned for this sysroot — honor it.
    if let Some(existing) = params
        .lock_file
        .get_kernel_version(params.target, &params.sysroot)
    {
        return Ok(Some(existing.clone()));
    }

    // 3. Compute effective spec (runtime-level overrides top-level; absent
    //    means "latest").
    let spec = params.config.effective_kernel_spec(params.runtime_name)?;

    // 4. Ask the repo what's available.
    let available = query_available_kernel_versions(params).await?;

    // 5. Pick the highest matching version.
    let picked = resolve_kernel_version(spec.as_ref(), &available)?;

    print_info(
        &format!(
            "Resolved kernel version: {picked} (sysroot: {})",
            params.sysroot.lock_key()
        ),
        OutputLevel::Normal,
    );

    // 6. Pin in the lockfile. Save happens via the normal install flow.
    params
        .lock_file
        .set_kernel_version(params.target, &params.sysroot, &picked);

    Ok(Some(picked))
}

/// Run `dnf repoquery --whatprovides kernel-base` inside the SDK container
/// and parse the output into a list of KERNEL_VERSIONs (the part after the
/// `kernel-` prefix on each package name).
async fn query_available_kernel_versions(params: &ResolveParams<'_>) -> Result<Vec<String>> {
    // oe-core's kernel.bbclass renames the `${KERNEL_PACKAGE_NAME}-base`
    // package to `${KERNEL_PACKAGE_NAME}-${KERNEL_VERSION_PKG_NAME}` and
    // explicitly emits `RPROVIDES += "${KERNEL_PACKAGE_NAME}-base"` on it —
    // so `--whatprovides kernel-base` is the exact virtual that resolves to
    // the renamed base kernel RPM (one row per KERNEL_VERSION available in
    // the feed). This avoids matching kernel-module-*, kernel-devsrc-*, or
    // other sibling subpackages.
    //
    // `parse_kernel_names` below strips the `kernel-` prefix from each NAME
    // to recover the KERNEL_VERSION string. `2>/dev/null` hides the "Last
    // metadata expiration" noise dnf writes to stderr.
    let command = r#"
set -euo pipefail
$DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF repoquery --whatprovides kernel-base --qf '%{NAME}\n' 2>/dev/null \
    | sort -u
"#
    .to_string();

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
            .context("failed to run dnf repoquery for kernel versions")?
    } else {
        params
            .container_helper
            .run_in_container_with_output(run_config)
            .await
            .context("failed to run dnf repoquery for kernel versions")?
    };

    let raw = output.unwrap_or_default();
    Ok(parse_kernel_names(&raw))
}

/// Parse dnf repoquery output into KERNEL_VERSIONs. Each line is an RPM
/// package NAME; keep only those that look like the renamed kernel base
/// package (`kernel-<KERNEL_VERSION>`) and strip the prefix.
fn parse_kernel_names(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // Drop subpackages that also provide 'kernel' transitively — we only
        // want the base kernel RPM whose name structure is `kernel-<VERSION>`.
        // Observed base kernels in feeds start with 'kernel-' followed by a
        // digit (since KERNEL_VERSION starts with the kernel series number).
        .filter_map(|line| {
            let rest = line.strip_prefix("kernel-")?;
            // First character of KERNEL_VERSION is a digit (e.g. "5.15", "6.6").
            // This filters out `kernel-module-*`, `kernel-devsrc-*`,
            // `kernel-image-*`, etc., which start with an alpha char after
            // the `kernel-` prefix.
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                Some(rest.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kernel_names_ignores_subpackages() {
        let raw = "\
kernel-5.15.185-l4t-r36.5-1033.33
kernel-module-host1x-5.15.185-l4t-r36.5-1033.33
kernel-devsrc-5.15.185-l4t-r36.5-1033.33
kernel-image-5.15.185-l4t-r36.5-1033.33
kernel-6.6.123
nv-kernel-module-host1x-5.15.185-l4t-r36.5-1033.33
";
        let parsed = parse_kernel_names(raw);
        assert_eq!(
            parsed,
            vec![
                "5.15.185-l4t-r36.5-1033.33".to_string(),
                "6.6.123".to_string(),
            ]
        );
    }

    #[test]
    fn parse_kernel_names_empty_input() {
        assert_eq!(parse_kernel_names(""), Vec::<String>::new());
        assert_eq!(parse_kernel_names("\n\n"), Vec::<String>::new());
    }
}
