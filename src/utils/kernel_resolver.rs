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

    // 2. Compute the effective spec (runtime-level overrides top-level;
    //    absent means "latest"). Done before the lockfile-pin check so we
    //    can validate that any existing pin still satisfies the user's
    //    current spec — if they changed `kernel.version`, the prior pin
    //    may no longer match and must be re-resolved.
    let spec = params.config.effective_kernel_spec(params.runtime_name)?;

    // 3. Lockfile already pinned for this sysroot — honor it ONLY if the
    //    pinned version still satisfies the configured spec. cargo/npm
    //    semantics: a lock pin holds across runs for reproducibility, but
    //    a spec change that invalidates the pin must trigger re-resolve.
    if let Some(existing) = params
        .lock_file
        .get_kernel_version(params.target, &params.sysroot)
    {
        let still_matches = match spec.as_ref() {
            // No spec configured ⇒ "latest" — any existing pin is acceptable.
            None => true,
            Some(s) => s.matches(existing),
        };
        if still_matches {
            return Ok(Some(existing.clone()));
        }
        if params.verbose {
            print_info(
                &format!(
                    "Lockfile-pinned kernel '{existing}' no longer satisfies the configured \
                     kernel spec; re-resolving (sysroot: {})",
                    params.sysroot.lock_key()
                ),
                OutputLevel::Normal,
            );
        }
        // Fall through; we'll overwrite the pin below with a freshly-
        // resolved version. The caller (rootfs/initramfs install) will
        // separately detect that the kver changed and trigger a
        // clean+reinstall of the sysroot.
    }

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

/// Enumerate available kernel versions from the feed. Runs two repoqueries in
/// one container invocation and unions the results:
///
/// 1. **Primary — `--whatprovides 'avocado-kernel-*' --provides`.** The linux
///    kernel bbappends in `meta-avocado-nvidia/recipes-kernel/linux/` emit
///    `RPROVIDES += avocado-kernel-${KERNEL_VERSION}` on
///    `${KERNEL_PACKAGE_NAME}-base`, a clean explicit contract.
/// 2. **Fallback — `repoquery 'kernel-*' --qf '%{NAME}'`.** Catches kernels
///    built before the `avocado-kernel-*` virtual landed: the renamed
///    `kernel-<KERNEL_VERSION>` base package is still visible by NAME, and
///    [`parse_kernel_names_and_provides`] filters to just the base kernels
///    by requiring the first post-prefix char to be a digit.
///
/// Both outputs are written to the same stdout; the parser recognizes each
/// format independently, so forward- and backward-built kernels compose in a
/// single rolling feed without changes to either query.
async fn query_available_kernel_versions(params: &ResolveParams<'_>) -> Result<Vec<String>> {
    // Two repoqueries share the warm DNF metadata cache inside one container
    // invocation, so the second call is effectively free. `2>/dev/null` hides
    // the "Last metadata expiration" noise; real errors still surface via
    // non-zero exit on either line (pipefail).
    //
    // `|| true` on each query keeps going if one returns no matches — both
    // empty is legit on a fresh-cache container and the parser handles it.
    let command = r#"
set -eo pipefail
{
    $DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF repoquery --whatprovides 'avocado-kernel-*' --provides 2>/dev/null || true
    $DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF repoquery 'kernel-*' --qf '%{NAME}\n' 2>/dev/null || true
} | sort -u
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
    Ok(parse_kernel_names_and_provides(&raw))
}

/// Parse combined output of the two repoquery calls into a deduped list of
/// KERNEL_VERSIONs. Handles two line shapes independently — either can
/// appear (or neither, or both for the same version) and we union them.
///
/// 1. **`avocado-kernel-<VERSION>`** — an explicit Provide line emitted by
///    the avocado kernel bbappends. May be followed by `= <EVR>` on some dnf
///    versions; we strip anything after the first whitespace.
/// 2. **`kernel-<VERSION>`** where VERSION starts with a digit — the renamed
///    base kernel NAME from the fallback `'kernel-*'` query. The digit-prefix
///    check filters out subpackages (`kernel-module-*`, `kernel-devsrc-*`,
///    `kernel-image-*`, etc.) whose first post-prefix char is alpha.
///
/// Other lines from `--provides` (generated `config(...)`, file provides, the
/// kernel-base's own name Provide, etc.) are ignored.
fn parse_kernel_names_and_provides(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("avocado-kernel-") {
            // Format 1: avocado-kernel-<VERSION> [= <EVR>]
            let version = rest.split_whitespace().next().unwrap_or(rest);
            if !version.is_empty() {
                out.push(version.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("kernel-") {
            // Format 2: kernel-<VERSION> NAME. KERNEL_VERSION always starts
            // with a digit; subpackage names start with an alpha character
            // after `kernel-` (module, modules, devsrc, image, dev, dbg, ...).
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                let version = rest.split_whitespace().next().unwrap_or(rest);
                if !version.is_empty() {
                    out.push(version.to_string());
                }
            }
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
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(
            parsed,
            vec![
                "5.15.185-l4t-r36.5-1033.33".to_string(),
                "6.6.123".to_string(),
            ]
        );
    }

    #[test]
    fn parse_filters_non_matching_lines() {
        // `dnf repoquery --provides` emits every Provide of every matching
        // package, not just the one that matched `--whatprovides`. Filter.
        // Also note `config(...)` and `kernel-base = <EVR>` lines must be
        // skipped — neither is a kernel version.
        let raw = "\
config(kernel-6.6.123-yocto-standard) = 6.6.123-r0
avocado-kernel-6.6.123
kernel-base = 6.6.123-r0
";
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(parsed, vec!["6.6.123".to_string()]);
    }

    #[test]
    fn parse_handles_evr_suffix() {
        let raw = "avocado-kernel-6.6.123 = 6.6.123-r0.avocado_qemux86_64\n";
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(parsed, vec!["6.6.123".to_string()]);
    }

    #[test]
    fn parse_dedupes_across_packages_and_queries() {
        // Two kernel-base packages in different arches provide the same
        // avocado-kernel name. And the same KERNEL_VERSION also appears in
        // the fallback NAME query output — both queries use ${KERNEL_VERSION}
        // identically, so for a kernel built with the new bbappend the two
        // queries return the same string. Collapse to one.
        let raw = "\
avocado-kernel-6.6.123-yocto-standard
avocado-kernel-6.6.123-yocto-standard
kernel-6.6.123-yocto-standard
";
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(parsed, vec!["6.6.123-yocto-standard".to_string()]);
    }

    #[test]
    fn parse_falls_back_to_name_glob_for_kernels_without_virtual() {
        // An older kernel in the feed that predates the avocado-kernel-*
        // bbappend: only the NAME query surfaces it. Mix with a newer kernel
        // that has both. Both should appear.
        let raw = "\
kernel-6.6.111-yocto-standard
kernel-6.6.123-yocto-standard
avocado-kernel-6.6.123-yocto-standard
";
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(
            parsed,
            vec![
                "6.6.111-yocto-standard".to_string(),
                "6.6.123-yocto-standard".to_string(),
            ]
        );
    }

    #[test]
    fn parse_ignores_kernel_subpackages() {
        // The fallback `kernel-*` glob picks up every kernel subpackage;
        // the digit-prefix rule keeps only base kernels.
        let raw = "\
kernel-6.6.123-yocto-standard
kernel-module-host1x-6.6.123-yocto-standard
kernel-devsrc-6.6.123-yocto-standard
kernel-image-6.6.123-yocto-standard
kernel-dev
";
        let parsed = parse_kernel_names_and_provides(raw);
        assert_eq!(parsed, vec!["6.6.123-yocto-standard".to_string()]);
    }

    #[test]
    fn parse_empty_input() {
        assert_eq!(parse_kernel_names_and_provides(""), Vec::<String>::new());
        assert_eq!(
            parse_kernel_names_and_provides("\n\n"),
            Vec::<String>::new()
        );
    }
}
