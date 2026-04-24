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
//!
//! The repoquery result is memoized per-process keyed by
//! `(target, repo_url)` — within a single `avocado …` invocation the
//! available-kernel list cannot change, so the first sysroot pays the
//! container/dnf cost and every subsequent sysroot resolution is O(µs).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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

    // 4. Ask the repo what's available (cached per-process).
    let available = get_available_kernel_versions(params).await?;

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

/// Cache key pairing target and repo URL — within a single process these
/// uniquely identify the available-kernel list the resolver cares about.
type KernelCacheKey = (String, String);

/// Process-level cache type alias.
type KernelVersionCache = Mutex<HashMap<KernelCacheKey, Vec<String>>>;

/// Process-level cache of `(target, repo_url) -> available KERNEL_VERSIONs`.
///
/// Within one `avocado …` invocation the feed doesn't change, so the first
/// caller pays the full `dnf repoquery` cost (container spin-up + metadata
/// load + solv build, typically 10–20s on a cold cache) and every subsequent
/// sysroot resolution hits this cache in microseconds.
fn kernel_version_cache() -> &'static KernelVersionCache {
    static CACHE: OnceLock<KernelVersionCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached wrapper around [`query_available_kernel_versions`]. The key pairs
/// target and repo URL so two avocado commands with different repo configs in
/// the same process don't cross-pollinate.
async fn get_available_kernel_versions(params: &ResolveParams<'_>) -> Result<Vec<String>> {
    let cache_key = (
        params.target.to_string(),
        params.repo_url.unwrap_or("").to_string(),
    );

    // Fast path: someone else already ran the query in this process.
    if let Some(cached) = {
        let guard = kernel_version_cache().lock().unwrap();
        guard.get(&cache_key).cloned()
    } {
        return Ok(cached);
    }

    // Slow path: do the actual container repoquery (never hold the mutex
    // across an await).
    let versions = query_available_kernel_versions(params).await?;

    kernel_version_cache()
        .lock()
        .unwrap()
        .insert(cache_key, versions.clone());

    Ok(versions)
}

/// Run `dnf repoquery --whatprovides 'avocado-kernel-*' --provides` inside the
/// SDK container. The linux kernel bbappends in
/// `meta-avocado-nvidia/recipes-kernel/linux/` emit
/// `RPROVIDES += avocado-kernel-${KERNEL_VERSION}` on
/// `${KERNEL_PACKAGE_NAME}-base`, so this query returns the full set of
/// `avocado-kernel-<VERSION>` Provides across every base kernel in the feed —
/// one per KERNEL_VERSION available.
async fn query_available_kernel_versions(params: &ResolveParams<'_>) -> Result<Vec<String>> {
    // `--provides` expands to a multi-line list of every provide on each
    // matching package (not just the one that matched `--whatprovides`), so
    // we still need to grep client-side for the `avocado-kernel-` prefix.
    // Using `set -u` here would explode if DNF_SDK_HOST happens to be empty
    // so the script stays permissive.
    //
    // `2>/dev/null` hides the "Last metadata expiration" noise dnf writes to
    // stderr; real errors still surface via non-zero exit.
    let command = r#"
set -eo pipefail
$DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF repoquery --whatprovides 'avocado-kernel-*' --provides 2>/dev/null \
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
    Ok(parse_avocado_kernel_provides(&raw))
}

/// Parse `dnf repoquery --provides` output into KERNEL_VERSIONs. Each line is
/// a single Provide from a matching package; we keep only those that start
/// with `avocado-kernel-` (our published contract) and strip that prefix. A
/// trailing `= <EVR>` (emitted by older dnf versions for versioned provides)
/// is also stripped.
fn parse_avocado_kernel_provides(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("avocado-kernel-") else {
            continue;
        };
        // Some dnf output formats appear as `avocado-kernel-6.6.123 = 6.6.123-r0`
        // — the space-delimited tail is not part of the Provide name we care about.
        let version = rest.split_whitespace().next().unwrap_or(rest);
        if !version.is_empty() {
            out.push(version.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_avocado_kernel_provides_happy_path() {
        let raw = "\
avocado-kernel-5.15.185-l4t-r36.5-1033.33
avocado-kernel-6.6.123
";
        let parsed = parse_avocado_kernel_provides(raw);
        assert_eq!(
            parsed,
            vec![
                "5.15.185-l4t-r36.5-1033.33".to_string(),
                "6.6.123".to_string(),
            ]
        );
    }

    #[test]
    fn parse_avocado_kernel_provides_filters_non_matching_lines() {
        // `dnf repoquery --provides` emits every Provide of every matching
        // package, not just the one that matched `--whatprovides`. Filter to
        // just our contract.
        let raw = "\
kernel-6.6.123-yocto-standard
config(kernel-6.6.123-yocto-standard) = 6.6.123-r0
avocado-kernel-6.6.123
kernel-base = 6.6.123-r0
";
        let parsed = parse_avocado_kernel_provides(raw);
        assert_eq!(parsed, vec!["6.6.123".to_string()]);
    }

    #[test]
    fn parse_avocado_kernel_provides_handles_evr_suffix() {
        // Some dnf output variants include `= <EVR>` after the provide name.
        let raw = "avocado-kernel-6.6.123 = 6.6.123-r0.avocado_qemux86_64\n";
        let parsed = parse_avocado_kernel_provides(raw);
        assert_eq!(parsed, vec!["6.6.123".to_string()]);
    }

    #[test]
    fn parse_avocado_kernel_provides_dedupes_across_packages() {
        // Two kernel-base packages in different arches provide the same
        // avocado-kernel name — collapse to one.
        let raw = "\
avocado-kernel-6.6.123
avocado-kernel-6.6.123
";
        let parsed = parse_avocado_kernel_provides(raw);
        assert_eq!(parsed, vec!["6.6.123".to_string()]);
    }

    #[test]
    fn parse_avocado_kernel_provides_empty_input() {
        assert_eq!(parse_avocado_kernel_provides(""), Vec::<String>::new());
        assert_eq!(parse_avocado_kernel_provides("\n\n"), Vec::<String>::new());
    }
}
