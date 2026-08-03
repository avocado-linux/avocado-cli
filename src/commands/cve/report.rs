//! `avocado cve report` — correlate installed packages with known CVEs.
//!
//! Reads the JSON published by the Yocto side (`bitbake avocado-cve-report`),
//! which maps every package a `bitbake world` produced to its recipe and that
//! recipe's unpatched CVEs. Then it asks RPM what is actually installed in each
//! sysroot of the project and joins the two on the package name.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::lockfile::{SysrootType, RPM_QUERY_ALL_FORMAT};
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};
use crate::utils::output_format::{emit_json_object, JsonOutputGuard, OutputFormat};
use crate::utils::target::resolve_target_required;

/// Schema version of the document this command emits. Independent of the
/// version of the source document produced by bitbake.
const REPORT_VERSION: &str = "1";

/// Marker separating one sysroot's package list from the next in the container
/// output. Tab-separated so it cannot collide with an RPM package name.
const SCOPE_MARKER: &str = "##SCOPE\t";

/// Marker emitted when a scope's `rpm -qa` exited non-zero. The query cannot
/// simply be allowed to fail: the container entrypoint runs under `set -e`, so
/// an unguarded failure would abort the whole script and lose every later
/// scope. Recording the failure keeps the script going and keeps an unreadable
/// sysroot from looking like an empty one.
const FAILED_MARKER: &str = "##FAILED\t";

/// Walks every sysroot that can hold installed target packages and dumps its
/// RPM database. `__SDK_QUERY__` is replaced with the host-SDK query, which is
/// the one case that has no `--root` (its database is found through custom
/// RPM_CONFIGDIR macros instead).
///
/// Roots are de-duplicated by realpath: `$AVOCADO_PREFIX/extensions` is a compat
/// symlink to the runtime-scoped tree, so it would otherwise be counted twice.
const DISCOVER_SCRIPT: &str = r#"
set -u
SEEN=""

query_root() {
    scope="$1"
    root="$2"
    [ -d "$root" ] || return 0
    real=$(readlink -f "$root")
    case " $SEEN " in
        *" $real "*) return 0 ;;
    esac
    SEEN="$SEEN $real"
    printf '##SCOPE\t%s\t%s\n' "$scope" "$real"
    # rpm's exit status is reported rather than discarded: unlike `rpm -q`,
    # which returns non-zero merely because a named package is absent, a
    # failing `rpm -qa` is always a real error. Guarded so the entrypoint's
    # `set -e` does not abort the remaining scopes.
    if ! (unset RPM_ETCCONFIGDIR RPM_CONFIGDIR; rpm -qa --root="$root" __QUERY_FORMAT__); then
        printf '##FAILED\t%s\n' "$scope"
    fi
}

sdk_query() {
__SDK_QUERY__
}

# Guarded like every other scope. Without the check an uninstalled project
# reports the SDK as a failed scan rather than as nothing to scan, which buries
# the actionable "run avocado install" message: the SDK database is reached
# through RPM_CONFIGDIR macros, so its absence shows up as an rpm error rather
# than a missing --root.
if [ -d "$AVOCADO_SDK_PREFIX/usr/lib/rpm" ]; then
    printf '##SCOPE\t%s\t%s\n' "sdk" "$AVOCADO_SDK_PREFIX"
    if ! sdk_query; then
        printf '##FAILED\t%s\n' "sdk"
    fi
fi

query_root "rootfs" "$AVOCADO_PREFIX/rootfs"
query_root "initramfs" "$AVOCADO_PREFIX/initramfs"
query_root "target-sysroot" "$AVOCADO_PREFIX/sdk/target-sysroot"
query_root "includes" "$AVOCADO_PREFIX/includes"
# A remote extension advertising `avocado-ext-layout(nested)` installs into the
# shared includes root queried above; one without it gets an installroot, and so
# an RPM database, of its own (utils/ext_fetch.rs). Both layouts land their
# content in includes/<name>/, so the per-extension roots have to be queried too
# or a project whose extensions are all legacy-layout scans nothing here. Nested
# ones hold no database of their own and drop out as an empty scope.
for inc_dir in "$AVOCADO_PREFIX"/includes/*/; do
    [ -d "$inc_dir" ] || continue
    query_root "includes:$(basename "$inc_dir")" "$inc_dir"
done

for runtime_dir in "$AVOCADO_PREFIX"/runtimes/*/; do
    [ -d "$runtime_dir" ] || continue
    runtime_name=$(basename "$runtime_dir")
    query_root "runtime:$runtime_name" "$runtime_dir"
    for ext_dir in "$runtime_dir"extensions/*/; do
        [ -d "$ext_dir" ] || continue
        query_root "ext:$runtime_name/$(basename "$ext_dir")" "$ext_dir"
    done
done

for ext_dir in "$AVOCADO_PREFIX"/extensions/*/; do
    [ -d "$ext_dir" ] || continue
    query_root "ext:$(basename "$ext_dir")" "$ext_dir"
done
"#;

// ---------------------------------------------------------------------------
// The document produced by `bitbake avocado-cve-report`
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SourceReport {
    #[serde(default)]
    generated: String,
    #[serde(default)]
    machine: Option<String>,
    // Deliberately not `#[serde(default)]`, for the same reason as the two maps
    // below: an absent key would default to "" and slip past the Unpatched
    // check, so a document produced with AVOCADO_CVE_REPORT_STATUS=Patched but
    // with the key trimmed would be reported as a list of live findings.
    status: String,
    // Deliberately not #[serde(default)]: a document missing either map would
    // otherwise deserialize into empty ones and correlate to zero CVEs, which
    // reads exactly like a clean scan. An explicitly empty map still parses.
    recipes: HashMap<String, SourceRecipe>,
    packages: HashMap<String, SourcePackage>,
}

#[derive(Debug, Deserialize)]
struct SourceRecipe {
    #[serde(default)]
    cves: Vec<SourceCve>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceCve {
    id: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    scorev2: Option<String>,
    #[serde(default)]
    scorev3: Option<String>,
    #[serde(default)]
    vector: Option<String>,
    #[serde(default)]
    link: Option<String>,
}

impl SourceCve {
    /// Best available score, preferring CVSS v3 over v2. cve-check writes "0.0"
    /// when a score is absent, so that reads as "unscored" rather than "harmless".
    fn score(&self) -> Option<f64> {
        self.resolved_score().map(|(value, _)| value)
    }

    /// Which CVSS version the resolved score came from, so the JSON document
    /// says what it ranked by instead of leaving a consumer to guess.
    fn score_source(&self) -> Option<&'static str> {
        self.resolved_score().map(|(_, source)| source)
    }

    fn resolved_score(&self) -> Option<(f64, &'static str)> {
        for (raw, source) in [(&self.scorev3, "v3"), (&self.scorev2, "v2")] {
            if let Some(value) = raw.as_ref().and_then(|s| s.parse::<f64>().ok()) {
                // `is_finite` as well as `> 0.0`: "inf" and "1e400" parse, pass
                // a bare positive test, and then serialize to JSON `null`
                // (serde_json cannot represent them) while `score_source` still
                // says "v3" — and trip every --fail-on-score threshold at once.
                // "NaN" already falls through, since no comparison holds.
                if value > 0.0 && value.is_finite() {
                    return Some((value, source));
                }
            }
        }
        None
    }
}

#[derive(Debug, Deserialize)]
struct SourcePackage {
    recipe: String,
    #[serde(default)]
    version: String,
}

// ---------------------------------------------------------------------------
// What RPM reports as installed
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct InstalledPackage {
    name: String,
    version: String,
    arch: String,
}

/// Every sysroot the scan found, plus what the container wrote to stderr.
///
/// The stderr is carried rather than dropped because the `##FAILED` guard makes
/// the script exit 0 by design: rpm's own diagnosis of an unreadable database
/// is on stderr and nowhere else, so a bail naming the scope would otherwise be
/// all the user ever gets.
#[derive(Debug)]
struct Sysroots {
    scopes: Vec<Sysroot>,
    stderr: String,
}

#[derive(Debug)]
struct Sysroot {
    scope: String,
    root: String,
    packages: Vec<InstalledPackage>,
    /// Its `rpm -qa` exited non-zero. Kept rather than dropped: a sysroot that
    /// could not be read must never be indistinguishable from one that holds
    /// no packages.
    failed: bool,
}

/// One installed package that a recipe with CVEs produced.
#[derive(Debug)]
struct Affected {
    package: String,
    /// VERSION-RELEASE as RPM reports it.
    installed_version: String,
    arch: String,
    /// PKGV-PKGR verbatim from the report, i.e. before the `-` -> `+` rewrite
    /// `package_rpm.bbclass` applies on the way into an RPM. Diffing this
    /// against `installed_version` raw therefore reports a mismatch for every
    /// hyphenated PKGV; run `rpm_pkgv` over the PKGV half first, which is what
    /// the scope-level version_mismatch list already reports the verdict of.
    report_version: String,
    recipe: String,
    cves: Vec<String>,
}

/// A CVE with every place it was found, so the same CVE shared by several
/// scopes is counted once.
#[derive(Debug)]
struct CveHit {
    cve: SourceCve,
    recipes: BTreeSet<String>,
    packages: BTreeSet<String>,
    scopes: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct ScopeResult {
    root: String,
    scanned: usize,
    /// Packages an extension sysroot only sees because its installroot was
    /// seeded with the rootfs RPM database. Counted, never attributed here.
    inherited: usize,
    affected: Vec<Affected>,
    unknown: Vec<String>,
    /// Held by an extension or runtime at a version the rootfs holds
    /// differently, so whether the scope really ships it cannot be told from
    /// here. Counted, and attributed to the scope anyway.
    baseline_divergent: Vec<String>,
    /// Installed at a different upstream version than the report records.
    version_mismatch: Vec<String>,
    /// Same upstream version, different packaging revision (PKGR).
    revision_mismatch: Vec<String>,
    /// Matched by name, but one side carried no version, so the cross-check
    /// could not run. Not the same as having passed it.
    version_unchecked: Vec<String>,
    /// The package names a recipe the report has no entry for. The producer
    /// only writes recipes that carry CVEs, so this is normally the clean
    /// case — but a truncated document looks identical, hence the counter.
    recipe_missing: Vec<String>,
}

impl ScopeResult {
    fn own_packages(&self) -> usize {
        self.scanned.saturating_sub(self.inherited)
    }
}

/// Split a Yocto package version into (upstream version, packaging revision).
///
/// `2.39.3-r0.5` -> ("2.39.3", "r0.5"). The revision is what a rebuild bumps,
/// so telling the two apart separates "different sources" from "same sources,
/// repackaged".
fn split_version(version: &str) -> (&str, &str) {
    match version.rsplit_once('-') {
        Some((pkgv, pkgr)) => (pkgv, pkgr),
        None => (version, ""),
    }
}

/// Rewrite a PKGV the way RPM packaging does before comparing it with what
/// `rpm -qa` reports.
///
/// RPM forbids "-" in VERSION, so oe-core's `package_rpm.bbclass` writes
/// `PKGV.replace('-', '+')` into the package. The report records PKGV verbatim,
/// so `4-7.1` there is `4+7.1` in the RPM database. Comparing them raw reports
/// a version mismatch for every such package — 146 of them in a scarthgap
/// `bitbake world` (blktool, cargo-c, cni, ...).
fn rpm_pkgv(pkgv: &str) -> String {
    pkgv.replace('-', "+")
}

pub struct CveReportCommand {
    config_path: String,
    /// The global `--runs-on`, carried only so the command can refuse it.
    /// `run_in_container_capture` has no remote branch — unlike
    /// `run_in_container`, which routes on `config.runs_on` — so honouring it
    /// would take work in `utils::container`, not a field here.
    runs_on: Option<String>,
    file: String,
    target: Option<String>,
    verbose: bool,
    container_args: Option<Vec<String>>,
    output: OutputFormat,
    /// Exit non-zero when any CVE scores at or above this. Off by default: a
    /// report is worth reading whatever it finds, and a threshold belongs to
    /// the policy consuming it, not to the scan.
    fail_on_score: Option<f64>,
    sdk_arch: Option<String>,
    composed_config: Option<Arc<ComposedConfig>>,
}

impl CveReportCommand {
    pub fn new(
        config_path: String,
        file: String,
        target: Option<String>,
        verbose: bool,
        container_args: Option<Vec<String>>,
        output: OutputFormat,
        fail_on_score: Option<f64>,
    ) -> Self {
        Self {
            config_path,
            file,
            target,
            verbose,
            container_args,
            output,
            fail_on_score,
            runs_on: None,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Record the global `--runs-on` so `execute` can refuse it rather than
    /// silently scanning the local volume.
    pub fn with_runs_on(mut self, runs_on: Option<String>) -> Self {
        self.runs_on = runs_on;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Refused rather than ignored. `--runs-on` is a global flag, and the
        // container helper this command uses reads the local volume with no
        // remote branch, so honouring it would take work in utils::container.
        // Until then, accepting it would emit a confident report describing
        // the wrong host — and a security artifact attributed to a machine it
        // does not describe is worse than an error.
        if let Some(host) = &self.runs_on {
            anyhow::bail!(
                "--runs-on {host} is not supported by `cve report`: it would read this machine's \
                 sysroots and attribute the result to {host}. Run the command on that host."
            );
        }

        // Silences the print_* helpers so --output json puts nothing but the
        // document on stdout; without it a warning or --verbose note lands
        // ahead of the JSON and breaks any consumer parsing it.
        let _json_guard = self.output.is_json().then(JsonOutputGuard::enable);

        let source = self.load_source()?;

        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .context("Failed to load composed config")?,
            ),
        };
        let config = &composed.config;
        let target = resolve_target_required(self.target.as_deref(), config)?;
        self.warn_on_machine_mismatch(&source, &target);

        let Sysroots {
            scopes: sysroots,
            stderr,
        } = self.query_sysroots(config, &target).await?;

        // A scope whose RPM database could not be read contributes no packages
        // and therefore no CVEs. Continuing would report the remaining scopes
        // as if they were the whole picture. Checked before the empty case
        // below so a failed query is named as one rather than reported as
        // nothing to scan.
        let failed: Vec<&str> = sysroots
            .iter()
            .filter(|s| s.failed)
            .map(|s| s.scope.as_str())
            .collect();
        if !failed.is_empty() {
            anyhow::bail!(
                "Could not read the RPM database of {} scope(s): {}. Those sysroots were not \
                 scanned, so any CVE they carry would be missing from this report.{}",
                failed.len(),
                failed.join(", "),
                stderr_tail(&stderr)
            );
        }

        // Reporting zero CVEs here would be indistinguishable from a clean
        // scan. Tested on packages rather than on the scope list: `rpm -qa`
        // over a wiped database exits 0 with no output, so a project whose
        // sysroots all exist but hold nothing produces scopes that are present
        // and empty — the one shape that reached the report as a clean exit 0.
        if sysroots.iter().all(|s| s.packages.is_empty()) {
            anyhow::bail!(
                "No installed package was found in any sysroot for target '{target}', so nothing \
                 was scanned. Run `avocado install` first — and if the project is installed, its \
                 RPM databases are unreadable or empty. Note that this command reads the state \
                 volume of the current directory, not of --config's directory."
            );
        }

        let (results, cves) = correlate(&source, &sysroots);

        if self.output.is_json() {
            emit_json_object(&self.build_json(&source, &target, &results, &cves));
        } else {
            self.print_human(&source, &results, &cves);
        }

        // After the report is emitted either way: a gate that suppresses the
        // findings it gates on would be useless.
        if let Some(threshold) = self.fail_on_score {
            let over = cves_at_or_above(&cves, threshold);
            if !over.is_empty() {
                anyhow::bail!(
                    "{} CVE(s) score {threshold:.1} or higher: {}.",
                    over.len(),
                    over.join(", ")
                );
            }
        }

        Ok(())
    }

    fn load_source(&self) -> Result<SourceReport> {
        let raw = std::fs::read_to_string(&self.file)
            .with_context(|| format!("Failed to read CVE report '{}'", self.file))?;
        let source: SourceReport = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse CVE report '{}'", self.file))?;

        if source.packages.is_empty() {
            anyhow::bail!(
                "CVE report '{}' has no 'packages' map; is it an avocado-cve-report document?",
                self.file
            );
        }
        // An empty map correlates to zero CVEs and reads exactly like a clean
        // image, and it is what the producer emits when cve-check was not
        // inherited or its NVD fetch failed. Refuse it as an input rather than
        // pass its emptiness through as a result.
        if source.recipes.is_empty() {
            anyhow::bail!(
                "CVE report '{}' has an empty 'recipes' map, so nothing could be correlated. \
                 The producing build most likely ran without cve-check inherited.",
                self.file
            );
        }
        // The producer records which cve-check status it filtered on. Anything
        // but Unpatched describes issues that are not live findings, and
        // correlating it would present them as if they were.
        if source.status != "Unpatched" {
            anyhow::bail!(
                "CVE report '{}' was generated with status '{}', not 'Unpatched'. Those are not \
                 live findings; regenerate the report with AVOCADO_CVE_REPORT_STATUS unset.",
                self.file,
                source.status
            );
        }
        Ok(source)
    }

    /// The report describes one Yocto MACHINE. Correlating it against a
    /// different target matches on package name alone and produces a confident
    /// wrong answer, so say so — but only warn: the MACHINE names a build
    /// configuration, and more than one can legitimately serve a target.
    fn warn_on_machine_mismatch(&self, source: &SourceReport, target: &str) {
        if machine_matches_target(source.machine.as_deref(), target) {
            return;
        }
        let Some(machine) = source.machine.as_deref() else {
            return;
        };
        print_warning(
            &format!(
                "Report was generated for MACHINE '{machine}' but this project targets \
                 '{target}'. Packages are correlated by name only, so the result may not \
                 describe this image."
            ),
            OutputLevel::Normal,
        );
    }

    /// Run one container that dumps every sysroot's RPM database.
    ///
    /// A single invocation rather than one per sysroot: container startup
    /// dominates the cost of an `rpm -qa`.
    async fn query_sysroots(&self, config: &Config, target: &str) -> Result<Sysroots> {
        let container_image = config.get_sdk_image().cloned().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
        })?;

        let sdk_query = SysrootType::Sdk(target.to_string())
            .get_rpm_query_config()
            .build_query_all_command();
        let command = DISCOVER_SCRIPT
            .replace("__SDK_QUERY__", &sdk_query)
            .replace("__QUERY_FORMAT__", RPM_QUERY_ALL_FORMAT);

        if self.verbose {
            print_info(
                "Querying installed packages in every sysroot.",
                OutputLevel::Normal,
            );
        }

        let container = SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);
        let run_config = RunConfig {
            container_image,
            target: target.to_string(),
            command,
            verbose: self.verbose,
            // The RPM queries need only the entrypoint's base env vars
            // ($AVOCADO_PREFIX, $AVOCADO_SDK_PREFIX), not the full SDK env.
            source_environment: false,
            use_entrypoint: true,
            interactive: false,
            repo_url: config.get_sdk_repo_url(),
            repo_release: config.get_sdk_repo_release(),
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        // `run_in_container_capture` rather than `run_in_container_with_output`,
        // which the sibling commands use. Two things follow from the difference,
        // and both matter here:
        //
        // - `_with_output` returns `Ok(None)` on a non-zero exit and discards
        //   the captured stdout, so docker being down or the entrypoint
        //   aborting would arrive as an empty scan — indistinguishable from an
        //   uninstalled project, and answered with "run `avocado install`".
        // - it also drops stderr on success, and the `##FAILED` guard makes the
        //   script exit 0 by design. So rpm's own "cannot open Packages
        //   database" had nowhere to go, and the bail below telling the user to
        //   re-run with --verbose promised something --verbose could not do.
        let out = container.run_in_container_capture(run_config).await?;
        if !out.success {
            anyhow::bail!(
                "Could not query the sysroots of target '{target}': the SDK container exited \
                 non-zero. This is a container or SDK failure, not an empty project.{}",
                stderr_tail(&out.stderr)
            );
        }

        Ok(Sysroots {
            scopes: parse_sysroots(&out.stdout),
            stderr: out.stderr,
        })
    }

    fn build_json(
        &self,
        source: &SourceReport,
        target: &str,
        results: &BTreeMap<String, ScopeResult>,
        cves: &BTreeMap<String, CveHit>,
    ) -> serde_json::Value {
        let scopes: serde_json::Map<String, serde_json::Value> = results
            .iter()
            .map(|(scope, result)| {
                let affected: Vec<serde_json::Value> = result
                    .affected
                    .iter()
                    .map(|a| {
                        let (pkgv, pkgr) = split_version(&a.report_version);
                        serde_json::json!({
                            "package": a.package,
                            "installed_version": a.installed_version,
                            "arch": a.arch,
                            "report_version": a.report_version,
                            // report_version put through the same PKGV rewrite
                            // this command compares with, so a consumer can
                            // diff it against installed_version directly. Left
                            // alongside the raw field rather than replacing it:
                            // report_version is the key that looks the package
                            // back up in the source document, and normalizing
                            // it would break that lookup.
                            "report_version_rpm": if pkgr.is_empty() {
                                rpm_pkgv(pkgv)
                            } else {
                                format!("{}-{pkgr}", rpm_pkgv(pkgv))
                            },
                            "recipe": a.recipe,
                            "cves": a.cves,
                        })
                    })
                    .collect();
                (
                    scope.clone(),
                    serde_json::json!({
                        "root": result.root,
                        "packages_scanned": result.scanned,
                        "packages_inherited": result.inherited,
                        "packages_own": result.own_packages(),
                        "packages_affected": result.affected.len(),
                        "packages_unknown": result.unknown.len(),
                        "packages_baseline_divergent": result.baseline_divergent.len(),
                        "packages_version_mismatch": result.version_mismatch.len(),
                        "packages_revision_mismatch": result.revision_mismatch.len(),
                        "packages_version_unchecked": result.version_unchecked.len(),
                        "recipes_missing": result.recipe_missing.len(),
                        "affected": affected,
                        "unknown": result.unknown,
                        "baseline_divergent": result.baseline_divergent,
                        "version_mismatch": result.version_mismatch,
                        "revision_mismatch": result.revision_mismatch,
                        "version_unchecked": result.version_unchecked,
                    }),
                )
            })
            .collect();

        let cve_index: serde_json::Map<String, serde_json::Value> = cves
            .iter()
            .map(|(id, hit)| {
                (
                    id.clone(),
                    serde_json::json!({
                        // The resolved score this command itself ranks by, so a
                        // consumer gating on severity does not have to
                        // reimplement the prefer-v3 rule and the "0.0 means
                        // unscored, not harmless" convention — and disagree
                        // with the human output when it gets them wrong.
                        "score": hit.cve.score(),
                        "score_source": hit.cve.score_source(),
                        "scorev2": hit.cve.scorev2,
                        "scorev3": hit.cve.scorev3,
                        "vector": hit.cve.vector,
                        "link": hit.cve.link,
                        "summary": hit.cve.summary,
                        "recipes": hit.recipes,
                        "packages": hit.packages,
                        "scopes": hit.scopes,
                    }),
                )
            })
            .collect();

        serde_json::json!({
            "version": REPORT_VERSION,
            "target": target,
            "source": {
                "file": self.file,
                "machine": source.machine,
                "status": source.status,
                "generated": source.generated,
                // The verdict, not just the two strings it is derived from.
                // JsonOutputGuard silences the human warning under --output
                // json, so without this a consumer either re-implements the
                // suffix rule — the one already gotten wrong once — or gates
                // CI on a correlation nothing vouched for.
                "machine_mismatch": !machine_matches_target(source.machine.as_deref(), target),
            },
            "counts": {
                "scopes": results.len(),
                "packages_scanned": results.values().map(|r| r.scanned).sum::<usize>(),
                "packages_inherited": results.values().map(|r| r.inherited).sum::<usize>(),
                "packages_own": results.values().map(|r| r.own_packages()).sum::<usize>(),
                "packages_affected": results.values().map(|r| r.affected.len()).sum::<usize>(),
                "packages_unknown": results.values().map(|r| r.unknown.len()).sum::<usize>(),
                "packages_baseline_divergent":
                    results.values().map(|r| r.baseline_divergent.len()).sum::<usize>(),
                "packages_version_mismatch":
                    results.values().map(|r| r.version_mismatch.len()).sum::<usize>(),
                "packages_revision_mismatch":
                    results.values().map(|r| r.revision_mismatch.len()).sum::<usize>(),
                "packages_version_unchecked":
                    results.values().map(|r| r.version_unchecked.len()).sum::<usize>(),
                "recipes_missing":
                    results.values().map(|r| r.recipe_missing.len()).sum::<usize>(),
                "cves": cves.len(),
                // Carried explicitly: these are the CVEs no --fail-on-score
                // threshold can match, so a consumer gating on severity has to
                // be able to see how much the gate could not speak for.
                "cves_unscored": cves.values().filter(|h| h.cve.score().is_none()).count(),
            },
            "scopes": scopes,
            "cves": cve_index,
        })
    }

    fn print_human(
        &self,
        source: &SourceReport,
        results: &BTreeMap<String, ScopeResult>,
        cves: &BTreeMap<String, CveHit>,
    ) {
        let any_inherited = results.values().any(|r| r.inherited > 0);

        println!(
            "{:<34} {:>9} {:>9} {:>6}",
            "SCOPE", "PACKAGES", "AFFECTED", "CVES"
        );
        for (scope, result) in results {
            let scope_cves: BTreeSet<&String> =
                result.affected.iter().flat_map(|a| a.cves.iter()).collect();
            println!(
                "{:<34} {:>9} {:>9} {:>6}",
                scope,
                result.own_packages(),
                result.affected.len(),
                scope_cves.len()
            );
        }
        if any_inherited {
            println!(
                "\nExtension and runtime counts exclude packages inherited from the rootfs \
                 database; those are attributed to the rootfs scope alone."
            );
        }
        // The subtraction above needs a rootfs scope with packages in it to
        // subtract. Without one — a project with no rootfs installed, or one
        // whose RPM database was wiped, which `rpm -qa` reports as an empty
        // scan rather than as an error — every seeded scope keeps the whole
        // base system as its own. Silence there would read as "these scopes
        // really do ship all this". A present-but-empty rootfs counts as
        // absent here: it is the wiped-database case, and it is the one that
        // used to be dropped before this warning could see it.
        let seeded_without_baseline = results.get("rootfs").is_none_or(|r| r.scanned == 0)
            && results
                .keys()
                .any(|s| s.starts_with("ext:") || s.starts_with("runtime:"));
        if seeded_without_baseline {
            print_warning(
                "No rootfs scope was scanned, so packages the extensions and runtimes inherited \
                 from the rootfs database cannot be told apart from what they ship: their counts \
                 include the whole base system.",
                OutputLevel::Normal,
            );
        }

        // Highest-scoring CVEs first — that ordering is the reason to read this
        // at all, and the full list belongs in --output json.
        let mut ranked: Vec<&CveHit> = cves.values().collect();
        ranked.sort_by(|a, b| {
            b.cve
                .score()
                .partial_cmp(&a.cve.score())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cve.id.cmp(&b.cve.id))
        });

        if !ranked.is_empty() {
            println!();
            for hit in ranked.iter().take(10) {
                let score = hit
                    .cve
                    .score()
                    .map(|s| format!("{s:.1}"))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<18} {:>5}  {:<10} {}",
                    hit.cve.id,
                    score,
                    hit.cve.vector.as_deref().unwrap_or("-"),
                    hit.recipes.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            if ranked.len() > 10 {
                println!("... and {} more (use --output json)", ranked.len() - 10);
            }
        }

        // An unscored CVE sorts last and so never reaches the ten rows above,
        // and --fail-on-score compares against a score it does not have. Both
        // are the right behaviours — a threshold cannot rank what carries no
        // rank — but together they make these invisible, which is the one
        // reading the "0.0 is unscored, not harmless" rule exists to prevent.
        let unscored = cves.values().filter(|h| h.cve.score().is_none()).count();
        if unscored > 0 {
            print_info(
                &format!(
                    "{unscored} CVE(s) carry no CVSS score. They are not ranked above and no \
                     --fail-on-score threshold can match them; review them from --output json."
                ),
                OutputLevel::Normal,
            );
        }

        let version_mismatch: usize = results.values().map(|r| r.version_mismatch.len()).sum();
        let revision_mismatch: usize = results.values().map(|r| r.revision_mismatch.len()).sum();
        let unknown: usize = results.values().map(|r| r.unknown.len()).sum();
        let version_unchecked: usize = results.values().map(|r| r.version_unchecked.len()).sum();

        if version_unchecked > 0 {
            print_warning(
                &format!(
                    "{version_unchecked} package(s) carry no version on one side, so the version \
                     cross-check could not run for them — they are matched by name alone."
                ),
                OutputLevel::Normal,
            );
        }
        if version_mismatch > 0 {
            print_warning(
                &format!(
                    "{version_mismatch} package(s) are installed at a different upstream version \
                     than the report records, so its CVE list may not describe them. The image was \
                     built from another feed release than the report."
                ),
                OutputLevel::Normal,
            );
        }
        if revision_mismatch > 0 {
            print_info(
                &format!(
                    "{revision_mismatch} package(s) share the upstream version but differ in \
                     packaging revision; patches may differ between the two."
                ),
                OutputLevel::Normal,
            );
        }
        let baseline_divergent: usize = results
            .values()
            .map(|r| r.baseline_divergent.len())
            .sum();
        if baseline_divergent > 0 {
            print_warning(
                &format!(
                    "{baseline_divergent} package(s) are attributed to an extension or runtime \
                     that the rootfs also holds, at a different version. Their installroots are \
                     seeded once from the rootfs RPM database and never refreshed, so these are \
                     either the scope's own build or a stale copy of a rootfs package that has \
                     since moved on — the two cannot be told apart here. Re-installing the scope \
                     resolves it."
                ),
                OutputLevel::Normal,
            );
        }
        if unknown > 0 {
            print_warning(
                &format!(
                    "{unknown} installed package(s) are absent from the report and therefore \
                     unchecked, not known to be clean."
                ),
                OutputLevel::Normal,
            );
        }
        // Info rather than a warning, unlike `unknown` above: the producer only
        // writes recipes that carry CVEs, so a missing entry is the ordinary
        // clean case and the count is large on any healthy report — 195 of the
        // 221 recipes on the avocado-qemuarm64 build this was measured against.
        // A warning that fires on every run stops being read. What the number
        // is good for is its shape: a truncated `recipes` map looks identical
        // from here, so a sudden jump is worth chasing.
        let recipes_missing: usize = results.values().map(|r| r.recipe_missing.len()).sum();
        if recipes_missing > 0 {
            print_info(
                &format!(
                    "{recipes_missing} recipe(s) named by installed packages have no entry in the \
                     report, which normally means they carry no unpatched CVE. A truncated report \
                     is indistinguishable from here; compare against counts.recipes_missing in \
                     --output json."
                ),
                OutputLevel::Normal,
            );
        }

        // `status` is required by the schema and load_source has already
        // rejected anything but Unpatched, so this is always "unpatched" —
        // spelled from the document rather than hardcoded so the two cannot
        // drift if the accepted set ever widens.
        let status = source.status.to_lowercase();
        print_success(
            &format!(
                "{} {} CVE(s) across {} scope(s).",
                cves.len(),
                status,
                results.len()
            ),
            OutputLevel::Normal,
        );
    }
}

/// Parse a `--fail-on-score` threshold, rejecting anything off the CVSS scale.
///
/// Without this clap takes any `f64`, and the values it then accepts all fail
/// open: `99` (the 0-100 confusion) and `nan` match no CVE, so the gate passes
/// a report full of 9.8s and says nothing. A release gate that silently stops
/// gating is worse than no gate.
pub fn parse_cvss_score(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() || !(0.0..=10.0).contains(&value) {
        return Err(format!(
            "`{raw}` is not a CVSS score; expected a value from 0.0 to 10.0"
        ));
    }
    Ok(value)
}

/// The CVEs a `--fail-on-score` threshold catches, highest first.
///
/// Split out of `execute` so the gate has a test that runs it. Nothing else
/// covered the comparison, so flipping `>=` to `>` — which lets a CVE scoring
/// exactly the threshold through — left every test green.
fn cves_at_or_above(cves: &BTreeMap<String, CveHit>, threshold: f64) -> Vec<&str> {
    let mut over: Vec<&CveHit> = cves
        .values()
        .filter(|hit| hit.cve.score().is_some_and(|s| s >= threshold))
        .collect();
    over.sort_by(|a, b| {
        b.cve
            .score()
            .partial_cmp(&a.cve.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cve.id.cmp(&b.cve.id))
    });
    over.into_iter().map(|hit| hit.cve.id.as_str()).collect()
}

/// Whether the report's MACHINE can legitimately describe this target.
///
/// A plain `contains` was wrong: "avocado-qemuarm64" contains "qemuarm", as does
/// every foo/foo64, imx8mp/imx8mp-lpddr4 and raspberrypi4/raspberrypi4-64 pair
/// this project builds. Those images share package names but neither versions
/// nor patch sets, which is exactly what the check exists to catch. MACHINE
/// names are "avocado-<target>", so require that whole suffix.
///
/// A report carrying no MACHINE at all cannot be contradicted, so it matches.
/// One function rather than two so the human warning and the JSON flag cannot
/// disagree — the rule is subtle enough to have been gotten wrong once already.
fn machine_matches_target(machine: Option<&str>, target: &str) -> bool {
    match machine {
        None => true,
        Some(m) => m == target || m.ends_with(&format!("-{target}")),
    }
}

/// The last few lines of the container's stderr, ready to append to a bail.
///
/// Quoted rather than summarised: rpm's message is the diagnosis, and any
/// paraphrase of it here would be a guess. Empty when there is nothing to show,
/// so the caller's message reads normally in the ordinary case.
fn stderr_tail(stderr: &str) -> String {
    const LINES: usize = 10;
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n  ");
    format!(" rpm reported:\n  {tail}")
}

/// Split the container output into one entry per sysroot.
fn parse_sysroots(output: &str) -> Vec<Sysroot> {
    let mut sysroots: Vec<Sysroot> = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(SCOPE_MARKER) {
            let mut parts = rest.splitn(2, '\t');
            let scope = parts.next().unwrap_or_default().trim().to_string();
            let root = parts.next().unwrap_or_default().trim().to_string();
            if !scope.is_empty() {
                sysroots.push(Sysroot {
                    scope,
                    root,
                    packages: Vec::new(),
                    failed: false,
                });
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix(FAILED_MARKER) {
            let scope = rest.trim();
            if let Some(s) = sysroots.iter_mut().find(|s| s.scope == scope) {
                s.failed = true;
            }
            continue;
        }

        // Anything before the first marker is entrypoint noise, not packages.
        let Some(current) = sysroots.last_mut() else {
            continue;
        };

        let mut fields = line.split('\t');
        let (Some(name), Some(version)) = (fields.next(), fields.next()) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        current.packages.push(InstalledPackage {
            name: name.to_string(),
            version: version.trim().to_string(),
            arch: fields.next().unwrap_or_default().trim().to_string(),
        });
    }

    // An empty scope is a sysroot that exists but holds no RPM database, which
    // is ordinary. A failed one is kept so the caller can refuse to report on
    // a scan that did not happen.
    //
    // `rootfs` is kept even when empty. `query_root` returns before printing a
    // marker when the directory does not exist, so a scope only reaches here
    // once its directory is present — and a present rootfs holding no packages
    // is a wiped database, not an uninstalled project. Dropping it removed the
    // one row that says so, and left the JSON with no field a CI gate could
    // read: `scopes.rootfs.packages_scanned: 0` is that field.
    sysroots.retain(|s| !s.packages.is_empty() || s.failed || s.scope == "rootfs");
    sysroots
}

/// Join installed packages with the report, per scope and deduplicated by CVE.
fn correlate(
    source: &SourceReport,
    sysroots: &[Sysroot],
) -> (BTreeMap<String, ScopeResult>, BTreeMap<String, CveHit>) {
    let mut results: BTreeMap<String, ScopeResult> = BTreeMap::new();
    let mut cves: BTreeMap<String, CveHit> = BTreeMap::new();

    // An extension's installroot is seeded with a copy of the rootfs RPM
    // database so dnf can resolve against the base without reinstalling it.
    // Those entries are not what the extension ships, so they are attributed to
    // rootfs alone — otherwise every extension reports the whole base system.
    //
    // Runtimes are seeded by the identical `cp -rf $AVOCADO_PREFIX/rootfs/var/
    // lib/rpm` (runtime/install.rs, runtime/dnf.rs — the same copy ext/install.rs
    // does), so they are excluded on the same grounds. Every other scope holds a
    // database of its own and is counted whole.
    //
    // Keyed on (name, arch): a rootfs can hold two packages of the same name
    // for different architectures (multilib, or a mixed-arch feed), and keying
    // on name alone would let whichever was read last mask an extension's own
    // package of the other arch, dropping that scope's attribution.
    let baseline: HashMap<(&str, &str), &str> = sysroots
        .iter()
        .find(|s| s.scope == "rootfs")
        .map(|s| {
            s.packages
                .iter()
                .map(|p| ((p.name.as_str(), p.arch.as_str()), p.version.as_str()))
                .collect()
        })
        .unwrap_or_default();

    for sysroot in sysroots {
        let seeded_from_rootfs =
            sysroot.scope.starts_with("ext:") || sysroot.scope.starts_with("runtime:");
        let result = results.entry(sysroot.scope.clone()).or_default();
        result.root = sysroot.root.clone();
        result.scanned += sysroot.packages.len();

        for installed in &sysroot.packages {
            let baseline_version = baseline.get(&(installed.name.as_str(), installed.arch.as_str()));
            let inherited = baseline_version.is_some_and(|v| *v == installed.version);
            if seeded_from_rootfs && inherited {
                result.inherited += 1;
                continue;
            }
            // Held by a seeded scope at a version the rootfs holds differently.
            // The seed is a one-time `cp -rf` of rootfs/var/lib/rpm taken when
            // the installroot was created and never refreshed, so this is either
            // an extension that really installed its own build of the package,
            // or a stale copy of a rootfs entry that has since moved on. The two
            // are indistinguishable from an rpm database, so it is attributed to
            // the scope — the safe direction, since nothing is hidden — and
            // recorded, because attributing a rootfs package to an extension in
            // a published security artifact is still wrong.
            if seeded_from_rootfs && baseline_version.is_some() {
                result.baseline_divergent.push(installed.name.clone());
            }

            let Some(source_package) = source.packages.get(&installed.name) else {
                result.unknown.push(installed.name.clone());
                continue;
            };

            // Matched by name. A different upstream version means the report may
            // not describe this package at all; a different revision means the
            // same sources were repackaged, which can still change patches.
            if source_package.version.is_empty() || installed.version.is_empty() {
                // Recorded rather than skipped: "the cross-check did not run"
                // must not be silently presented as "the cross-check passed".
                result.version_unchecked.push(installed.name.clone());
            } else {
                let (installed_pkgv, installed_pkgr) = split_version(&installed.version);
                let (report_pkgv, report_pkgr) = split_version(&source_package.version);
                if installed_pkgv != rpm_pkgv(report_pkgv) {
                    result.version_mismatch.push(installed.name.clone());
                } else if installed_pkgr != report_pkgr {
                    result.revision_mismatch.push(installed.name.clone());
                }
            }

            let Some(recipe) = source.recipes.get(&source_package.recipe) else {
                // Normally means the recipe carries no CVE of the reported
                // status, which is the clean case. A truncated document is
                // indistinguishable from here, so count it.
                result.recipe_missing.push(source_package.recipe.clone());
                continue;
            };
            if recipe.cves.is_empty() {
                continue;
            }

            for cve in &recipe.cves {
                let hit = cves.entry(cve.id.clone()).or_insert_with(|| CveHit {
                    cve: cve.clone(),
                    recipes: BTreeSet::new(),
                    packages: BTreeSet::new(),
                    scopes: BTreeSet::new(),
                });
                // Two recipes can report the same CVE with different metadata:
                // cve-check writes "0.0" where it has no score, so a -native
                // recipe's unscored copy would otherwise mask the scored one
                // and drop a critical below the human output's cut. Keep the
                // higher-scoring payload regardless of iteration order.
                if cve.score() > hit.cve.score() {
                    hit.cve = cve.clone();
                }
                hit.recipes.insert(source_package.recipe.clone());
                hit.packages.insert(installed.name.clone());
                hit.scopes.insert(sysroot.scope.clone());
            }

            result.affected.push(Affected {
                package: installed.name.clone(),
                installed_version: installed.version.clone(),
                arch: installed.arch.clone(),
                report_version: source_package.version.clone(),
                recipe: source_package.recipe.clone(),
                cves: recipe.cves.iter().map(|c| c.id.clone()).collect(),
            });
        }

        result.affected.sort_by(|a, b| a.package.cmp(&b.package));
        result.baseline_divergent.sort();
        result.unknown.sort();
        result.version_mismatch.sort();
        result.revision_mismatch.sort();
        result.version_unchecked.sort();
        // `recipe_missing` is the only one of these that is deduped, and the
        // asymmetry is deliberate. It names recipes, and one recipe backs many
        // packages, so a repeat there is the same fact restated. The others
        // name packages, and a repeat is a second package: multilib puts
        // `libfoo` in a rootfs at both core2_64 and i686, and both are
        // genuinely unknown to the report. Deduping by name would drop one and
        // leave `scopes.rootfs.unknown.len()` disagreeing with
        // `counts.packages_unknown`, which counts packages.
        result.recipe_missing.sort();
        result.recipe_missing.dedup();
    }

    (results, cves)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_fixture() -> SourceReport {
        serde_json::from_str(
            r#"{
                "generated": "2026-08-03T00:00:00Z",
                "machine": "avocado-qemux86-64",
                "status": "Unpatched",
                "recipes": {
                    "openssl": {
                        "version": "3.5.7",
                        "packaged": true,
                        "cves": [
                            {"id": "CVE-2026-1", "scorev3": "9.1", "vector": "NETWORK"},
                            {"id": "CVE-2026-2", "scorev3": "5.3", "vector": "LOCAL"}
                        ]
                    },
                    "zlib": {
                        "version": "1.3.1",
                        "packaged": true,
                        "cves": [{"id": "CVE-2026-3", "scorev2": "4.0"}]
                    }
                },
                "packages": {
                    "libssl3": {"recipe": "openssl", "version": "3.5.7-r0.0"},
                    "openssl-bin": {"recipe": "openssl", "version": "3.5.7-r0.0"},
                    "libz1": {"recipe": "zlib", "version": "1.3.1-r0.0"},
                    "bash": {"recipe": "bash", "version": "5.2.21-r0.0"}
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_scopes_and_ignores_leading_noise() {
        let output = concat!(
            "entrypoint noise\n",
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
            "bash\t5.2.21-r0.0\tcore2_64\n",
            "##SCOPE\truntime:dev\t/opt/_avocado/x86_64/runtimes/dev\n",
            "libz1\t1.3.1-r0.0\tcore2_64\n",
        );

        let sysroots = parse_sysroots(output);
        assert_eq!(sysroots.len(), 2);
        assert_eq!(sysroots[0].scope, "rootfs");
        assert_eq!(sysroots[0].packages.len(), 2);
        assert_eq!(sysroots[1].scope, "runtime:dev");
        assert_eq!(sysroots[1].packages[0].name, "libz1");
    }

    #[test]
    fn empty_sysroots_are_dropped() {
        let output = "##SCOPE\tinitramfs\t/opt/_avocado/x86_64/initramfs\n";
        assert!(parse_sysroots(output).is_empty());
    }

    #[test]
    fn a_failed_scope_is_kept_so_it_cannot_look_empty() {
        let output = concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "##FAILED\trootfs\n",
            "##SCOPE\truntime:dev\t/opt/_avocado/x86_64/runtimes/dev\n",
            "libz1\t1.3.1-r0.0\tcore2_64\n",
        );

        let sysroots = parse_sysroots(output);
        assert_eq!(sysroots.len(), 2, "a failed scope must survive retain");

        let rootfs = sysroots.iter().find(|s| s.scope == "rootfs").unwrap();
        assert!(rootfs.failed);
        assert!(rootfs.packages.is_empty());

        let dev = sysroots.iter().find(|s| s.scope == "runtime:dev").unwrap();
        assert!(!dev.failed);
    }

    #[test]
    fn an_empty_rootfs_scope_is_kept_and_leaves_the_runtime_without_a_baseline() {
        // `rpm -qa` over a wiped database exits 0 with no output, so the scope
        // is empty rather than failed. Dropping it made a runtime's inherited
        // base system look like its own packages, with nothing saying so.
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "##SCOPE\truntime:dev\t/opt/_avocado/x86_64/runtimes/dev\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
        ));
        assert_eq!(sysroots.len(), 2, "an empty rootfs must survive retain");

        let (results, _) = correlate(&source, &sysroots);
        assert_eq!(results["rootfs"].scanned, 0);
        // Which is what the human path keys its warning on, and what a JSON
        // consumer reads as scopes.rootfs.packages_scanned.
        assert!(results.get("rootfs").is_some_and(|r| r.scanned == 0));
        assert_eq!(results["runtime:dev"].inherited, 0);
    }

    #[test]
    fn an_empty_non_rootfs_scope_is_still_dropped() {
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libz1\t1.3.1-r0.0\tcore2_64\n",
            "##SCOPE\text:dev/app\t/opt/_avocado/x86_64/runtimes/dev/extensions/app\n",
        ));
        assert_eq!(sysroots.len(), 1);
        assert_eq!(sysroots[0].scope, "rootfs");
    }

    #[test]
    fn the_discovery_script_reports_rpm_failures_instead_of_swallowing_them() {
        // The entrypoint runs under `set -e`, so the query must be guarded —
        // but the guard has to record the failure, not discard it.
        assert!(
            !DISCOVER_SCRIPT.contains("|| true"),
            "a swallowed rpm failure makes an unreadable sysroot look empty"
        );
        assert!(DISCOVER_SCRIPT.contains("printf '##FAILED\\t%s\\n'"));
    }

    #[test]
    fn the_discovery_script_walks_the_per_extension_includes_roots() {
        // A legacy-layout remote extension gets its own installroot under
        // includes/<name> (utils/ext_fetch.rs), so querying only the shared
        // includes database scans nothing for a project made of those.
        assert!(DISCOVER_SCRIPT.contains(r#"query_root "includes" "$AVOCADO_PREFIX/includes""#));
        assert!(DISCOVER_SCRIPT.contains(r#"for inc_dir in "$AVOCADO_PREFIX"/includes/*/"#));
        assert!(DISCOVER_SCRIPT.contains(r#"query_root "includes:$(basename "$inc_dir")""#));
    }

    #[test]
    fn rpm_pkgv_matches_the_hyphen_rewrite_rpm_packaging_applies() {
        // package_rpm.bbclass writes PKGV.replace('-', '+') because RPM
        // forbids "-" in VERSION; the report records PKGV verbatim.
        assert_eq!(rpm_pkgv("4-7.1"), "4+7.1");
        assert_eq!(rpm_pkgv("0.10.19+cargo-0.93.0"), "0.10.19+cargo+0.93.0");
        assert_eq!(rpm_pkgv("3.5.7"), "3.5.7");
    }

    #[test]
    fn a_hyphenated_pkgv_is_not_a_version_mismatch() {
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"blktool": {"cves": [{"id": "CVE-2026-9"}]}},
                "packages": {"blktool": {"recipe": "blktool", "version": "4-7.1-r0.0"}}
            }"#,
        )
        .unwrap();
        // What rpm actually reports for that package.
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "blktool\t4+7.1-r0.0\tcore2_64\n",
        ));

        let (results, cves) = correlate(&source, &sysroots);
        let rootfs = &results["rootfs"];
        assert!(
            rootfs.version_mismatch.is_empty(),
            "the hyphen rewrite is packaging, not drift: {:?}",
            rootfs.version_mismatch
        );
        assert!(rootfs.revision_mismatch.is_empty());
        assert_eq!(rootfs.affected.len(), 1);
        assert_eq!(cves.len(), 1);
    }

    #[test]
    fn the_highest_scoring_copy_of_a_cve_wins() {
        // cve-check writes "0.0" when it has no score; a -native recipe's
        // unscored copy must not mask the scored one.
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {
                    "aaa-native": {"cves": [{"id": "CVE-2026-7", "scorev3": "0.0"}]},
                    "zzz": {"cves": [{"id": "CVE-2026-7", "scorev3": "9.8"}]}
                },
                "packages": {
                    "aaa": {"recipe": "aaa-native", "version": "1.0-r0"},
                    "zzz": {"recipe": "zzz", "version": "1.0-r0"}
                }
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "aaa\t1.0-r0\tcore2_64\n",
            "zzz\t1.0-r0\tcore2_64\n",
        ));

        let (_, cves) = correlate(&source, &sysroots);
        assert_eq!(cves["CVE-2026-7"].cve.score(), Some(9.8));
        assert_eq!(cves["CVE-2026-7"].cve.score_source(), Some("v3"));
    }

    #[test]
    fn a_missing_version_is_recorded_rather_than_skipped() {
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"openssl": {"cves": [{"id": "CVE-2026-1"}]}},
                "packages": {"libssl3": {"recipe": "openssl"}}
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
        ));

        let (results, _) = correlate(&source, &sysroots);
        assert_eq!(results["rootfs"].version_unchecked, vec!["libssl3"]);
        assert!(results["rootfs"].version_mismatch.is_empty());
    }

    #[test]
    fn a_package_whose_recipe_is_absent_is_counted() {
        // `bash` is in packages with no recipes entry: normally "no CVEs",
        // but a truncated document looks the same, so it is counted.
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "bash\t5.2.21-r0.0\tcore2_64\n",
        ));

        let (results, _) = correlate(&source, &sysroots);
        assert_eq!(results["rootfs"].recipe_missing, vec!["bash"]);
        assert!(results["rootfs"].affected.is_empty());
        assert!(results["rootfs"].unknown.is_empty());
    }

    #[test]
    fn same_name_different_arch_is_not_treated_as_inherited() {
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"foo": {"cves": [{"id": "CVE-2026-5", "scorev3": "7.5"}]}},
                "packages": {"libfoo": {"recipe": "foo", "version": "1.0-r0"}}
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libfoo\t1.0-r0\tnoarch\n",
            "##SCOPE\text:dev/app\t/opt/_avocado/x86_64/runtimes/dev/extensions/app\n",
            "libfoo\t1.0-r0\tcore2_64\n",
        ));

        let (results, _) = correlate(&source, &sysroots);
        let ext = &results["ext:dev/app"];
        assert_eq!(ext.inherited, 0, "a different arch is the extension's own");
        assert_eq!(ext.affected.len(), 1);
    }

    #[test]
    fn correlates_and_deduplicates_cves_across_scopes() {
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
            "openssl-bin\t3.5.7-r0.0\tcore2_64\n",
            "bash\t5.2.21-r0.0\tcore2_64\n",
            "##SCOPE\truntime:dev\t/runtimes/dev\n",
            // Inherited from the rootfs database the runtime was seeded with.
            "bash\t5.2.21-r0.0\tcore2_64\n",
            // Installed into the runtime itself, at a version the rootfs does
            // not hold, so the baseline cannot claim it.
            "libssl3\t3.5.8-r0.0\tcore2_64\n",
        ));

        let (results, cves) = correlate(&source, &sysroots);

        // Two packages of the same recipe in rootfs, one in the runtime.
        assert_eq!(results["rootfs"].affected.len(), 2);
        assert_eq!(results["runtime:dev"].affected.len(), 1);
        assert_eq!(results["runtime:dev"].inherited, 1);
        // bash has no CVE entry, so it is neither affected nor unknown.
        assert_eq!(results["rootfs"].scanned, 3);
        assert!(results["rootfs"].unknown.is_empty());

        // The same two openssl CVEs seen in both scopes are counted once.
        assert_eq!(cves.len(), 2);
        let hit = &cves["CVE-2026-1"];
        assert_eq!(
            hit.scopes.iter().cloned().collect::<Vec<_>>(),
            vec!["rootfs", "runtime:dev"]
        );
        assert_eq!(
            hit.packages.iter().cloned().collect::<Vec<_>>(),
            vec!["libssl3", "openssl-bin"]
        );
    }

    #[test]
    fn flags_unknown_packages_and_version_mismatches() {
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libssl3\t3.5.6-r0.0\tcore2_64\n",
            "some-vendor-pkg\t1.0-r0\tcore2_64\n",
        ));

        let (results, cves) = correlate(&source, &sysroots);

        assert_eq!(results["rootfs"].version_mismatch, vec!["libssl3"]);
        assert_eq!(results["rootfs"].unknown, vec!["some-vendor-pkg"]);
        // A version mismatch does not suppress the CVEs.
        assert_eq!(cves.len(), 2);
    }

    #[test]
    fn separates_revision_from_version_mismatch() {
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            // Same upstream version, rebuilt: r0.5 vs the report's r0.0.
            "libssl3\t3.5.7-r0.5\tcore2_64\n",
            // Different upstream version.
            "libz1\t1.3.0-r0.0\tcore2_64\n",
        ));

        let (results, _) = correlate(&source, &sysroots);

        assert_eq!(results["rootfs"].revision_mismatch, vec!["libssl3"]);
        assert_eq!(results["rootfs"].version_mismatch, vec!["libz1"]);
    }

    #[test]
    fn extension_scopes_exclude_the_inherited_rootfs_baseline() {
        let source = source_fixture();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
            "libz1\t1.3.1-r0.0\tcore2_64\n",
            // Seeded with the rootfs database, plus one package of its own.
            "##SCOPE\text:dev/sshd\t/runtimes/dev/extensions/sshd\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
            "libz1\t1.3.1-r0.0\tcore2_64\n",
            "openssh\t9.6-r0.0\tcore2_64\n",
            // Same name as the baseline but a different version: the extension
            // really does ship this one, so it must not be treated as inherited.
            "##SCOPE\text:dev/pinned\t/runtimes/dev/extensions/pinned\n",
            "libssl3\t3.5.6-r0.0\tcore2_64\n",
        ));

        let (results, cves) = correlate(&source, &sysroots);

        assert_eq!(results["ext:dev/sshd"].inherited, 2);
        assert_eq!(results["ext:dev/sshd"].own_packages(), 1);
        assert!(results["ext:dev/sshd"].affected.is_empty());
        assert_eq!(results["ext:dev/sshd"].unknown, vec!["openssh"]);

        assert_eq!(results["ext:dev/pinned"].inherited, 0);
        assert_eq!(results["ext:dev/pinned"].affected.len(), 1);

        // openssl CVEs come from rootfs and the pinned extension, never from the
        // inherited copy in ext:dev/sshd.
        assert_eq!(
            cves["CVE-2026-1"]
                .scopes
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["ext:dev/pinned", "rootfs"]
        );
    }

    #[test]
    fn the_json_publishes_the_report_version_as_rpm_spells_it() {
        // A JSON consumer diffing installed_version against report_version raw
        // sees a mismatch on every hyphenated PKGV — icu 74-2, libedit
        // 20230828-3.1, 146 packages in a scarthgap world build. The companion
        // field spares it from reimplementing rpm_pkgv to read the report.
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"icu": {"cves": [{"id": "CVE-2026-7", "scorev3": "7.5"}]}},
                "packages": {"libicuuc74": {"recipe": "icu", "version": "74-2-r0.0"}}
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libicuuc74\t74+2-r0.0\tcore2_64\n",
        ));
        let (results, cves) = correlate(&source, &sysroots);
        assert!(
            results["rootfs"].version_mismatch.is_empty(),
            "the rewrite is what the command itself compares with"
        );

        let cmd = CveReportCommand::new(
            "avocado.yaml".to_string(),
            "report.json".to_string(),
            None,
            false,
            None,
            OutputFormat::Json,
            None,
        );
        let doc = cmd.build_json(&source, "qemux86-64", &results, &cves);
        let entry = &doc["scopes"]["rootfs"]["affected"][0];
        assert_eq!(entry["report_version"], "74-2-r0.0");
        assert_eq!(entry["report_version_rpm"], "74+2-r0.0");
        assert_eq!(entry["report_version_rpm"], entry["installed_version"]);
    }


    #[test]
    fn the_release_gate_catches_a_cve_scoring_exactly_the_threshold() {
        // The only coverage this gate had was a --help grep, so flipping >= to
        // > let a CVE scoring exactly the threshold through with every test
        // still green. For a release gate that is the one case that matters.
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"openssl": {"cves": [
                    {"id": "CVE-2026-EXACT", "scorev3": "7.0"},
                    {"id": "CVE-2026-OVER", "scorev3": "9.8"},
                    {"id": "CVE-2026-UNDER", "scorev3": "6.9"},
                    {"id": "CVE-2026-UNSCORED"}
                ]}},
                "packages": {"libssl3": {"recipe": "openssl", "version": "3.5.7-r0.0"}}
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
        ));
        let (_, cves) = correlate(&source, &sysroots);

        assert_eq!(
            cves_at_or_above(&cves, 7.0),
            vec!["CVE-2026-OVER", "CVE-2026-EXACT"],
            "the threshold is inclusive, and the list is ranked"
        );
        assert!(cves_at_or_above(&cves, 9.9).is_empty());
        // An unscored CVE can never match: there is nothing to compare.
        assert!(!cves_at_or_above(&cves, 0.0).contains(&"CVE-2026-UNSCORED"));
    }

    #[test]
    fn a_threshold_off_the_cvss_scale_is_refused_at_startup() {
        // Each of these used to parse and then match nothing, so CI exited 0
        // against a report full of 9.8s and said nothing about why.
        for bad in ["99", "nan", "NaN", "inf", "1e400", "-1"] {
            assert!(parse_cvss_score(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(parse_cvss_score("7.0"), Ok(7.0));
        assert_eq!(parse_cvss_score("0"), Ok(0.0));
        assert_eq!(parse_cvss_score("10"), Ok(10.0));
    }

    #[test]
    fn a_non_finite_score_is_treated_as_unscored() {
        // "inf" passes a bare `> 0.0`, and then serializes to JSON null while
        // score_source still says v3, and trips every threshold at once.
        let cve: SourceCve =
            serde_json::from_str(r#"{"id": "CVE-2026-INF", "scorev3": "inf"}"#).unwrap();
        assert_eq!(cve.score(), None);
        assert_eq!(cve.score_source(), None);

        let overflow: SourceCve =
            serde_json::from_str(r#"{"id": "CVE-2026-BIG", "scorev3": "1e400"}"#).unwrap();
        assert_eq!(overflow.score(), None);
    }

    #[test]
    fn an_absent_status_is_a_parse_error_not_an_empty_string() {
        // With #[serde(default)] the key could be omitted and skip the
        // Unpatched check entirely, so a Patched report would print its CVEs as
        // live findings. Its two sibling maps already refuse the same trick.
        let no_status = r#"{"recipes": {}, "packages": {}}"#;
        let err = serde_json::from_str::<SourceReport>(no_status).unwrap_err();
        assert!(err.to_string().contains("status"), "{err}");
    }

    #[test]
    fn the_machine_rule_is_one_function_for_both_outputs() {
        assert!(machine_matches_target(Some("avocado-qemuarm64"), "qemuarm64"));
        assert!(machine_matches_target(Some("qemuarm64"), "qemuarm64"));
        // The pairs the substring form got wrong.
        assert!(!machine_matches_target(Some("avocado-qemuarm64"), "qemuarm"));
        assert!(!machine_matches_target(
            Some("avocado-imx8mp-lpddr4"),
            "imx8mp"
        ));
        assert!(!machine_matches_target(
            Some("avocado-raspberrypi4-64"),
            "raspberrypi4"
        ));
        // Nothing to contradict.
        assert!(machine_matches_target(None, "qemuarm64"));
    }

    #[test]
    fn a_seeded_scope_holding_a_diverged_rootfs_package_is_recorded() {
        // The installroot is seeded once from rootfs/var/lib/rpm and never
        // refreshed. After the rootfs moves on, the stale copy looks like a
        // package the extension installed itself.
        let source: SourceReport = serde_json::from_str(
            r#"{
                "status": "Unpatched",
                "recipes": {"openssl": {"cves": [{"id": "CVE-2026-1", "scorev3": "9.8"}]}},
                "packages": {"libssl3": {"recipe": "openssl", "version": "3.5.8-r0.0"}}
            }"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libssl3\t3.5.8-r0.0\tcore2_64\n",
            "##SCOPE\text:dev/sshd\t/runtimes/dev/extensions/sshd\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
        ));

        let (results, _) = correlate(&source, &sysroots);
        let ext = &results["ext:dev/sshd"];
        assert_eq!(ext.inherited, 0, "the versions differ, so not inherited");
        assert_eq!(ext.baseline_divergent, vec!["libssl3"]);
        // Still attributed: nothing is hidden, the ambiguity is reported.
        assert_eq!(ext.affected.len(), 1);
        // A package the rootfs does not carry at all is unambiguous.
        assert!(results["rootfs"].baseline_divergent.is_empty());
    }

    #[test]
    fn a_stderr_tail_is_quoted_and_empty_when_there_is_nothing_to_say() {
        assert_eq!(stderr_tail(""), "");
        assert_eq!(stderr_tail("   \n\n"), "");
        let tail = stderr_tail("warming up\nerror: cannot open Packages database\n");
        assert!(tail.contains("cannot open Packages database"), "{tail}");
    }

    #[test]
    fn multilib_names_stay_in_the_lists_that_count_packages() {
        // Two arches of one name are two packages, so `unknown` carries both
        // and `counts.packages_unknown` reads 2. Deduping by name would make
        // the list disagree with the count it is the detail for.
        let source: SourceReport = serde_json::from_str(
            r#"{"status": "Unpatched", "recipes": {"x": {"cves": []}},
                "packages": {"other": {"recipe": "x", "version": "1.0-r0"}}}"#,
        )
        .unwrap();
        let sysroots = parse_sysroots(concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "libfoo\t1.0-r0\tcore2_64\n",
            "libfoo\t1.0-r0\ti686\n",
        ));
        let (results, _) = correlate(&source, &sysroots);
        assert_eq!(results["rootfs"].unknown, vec!["libfoo", "libfoo"]);
    }

    #[test]
    fn a_document_missing_either_map_is_rejected() {
        // Silently defaulting these to empty would correlate to zero CVEs and
        // read as a clean scan, so the parse itself has to fail.
        let no_recipes = r#"{"status": "Unpatched", "packages": {}}"#;
        assert!(serde_json::from_str::<SourceReport>(no_recipes).is_err());

        let no_packages = r#"{"status": "Unpatched", "recipes": {}}"#;
        assert!(serde_json::from_str::<SourceReport>(no_packages).is_err());

        // Both present but empty parses: the distinction this test draws is
        // between "the key is absent" and "the key is an empty map", and only
        // the first has to fail at the serde layer. `load_source` rejects the
        // empty `recipes` map separately, on the grounds that it is what a
        // build without cve-check emits.
        let empty_but_present = r#"{"status": "Unpatched", "recipes": {}, "packages": {}}"#;
        assert!(serde_json::from_str::<SourceReport>(empty_but_present).is_ok());
    }

    #[test]
    fn score_prefers_v3_and_treats_zero_as_absent() {
        let cve: SourceCve =
            serde_json::from_str(r#"{"id": "CVE-2026-9", "scorev2": "4.0", "scorev3": "0.0"}"#)
                .unwrap();
        assert_eq!(cve.score(), Some(4.0));

        let unscored: SourceCve =
            serde_json::from_str(r#"{"id": "CVE-2026-9", "scorev2": "0.0", "scorev3": "0.0"}"#)
                .unwrap();
        assert_eq!(unscored.score(), None);
    }
}
