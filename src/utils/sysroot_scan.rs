//! Dump the RPM database of every sysroot a project installed.
//!
//! Kept apart from its one caller, `avocado sbom`, because the answer to "what
//! is installed" is a single fact about a project: anything else that has to
//! know — an inventory, a package diff between two builds — must reach the same
//! set from the same walk, and differ only in which RPM tags it asks for and
//! what it does with the answer. The query format is therefore the caller's.

use anyhow::Result;

use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::lockfile::SysrootType;
use crate::utils::output::{print_info, OutputLevel};

/// Marker separating one sysroot's package list from the next in the container
/// output. Tab-separated so it cannot collide with an RPM package name.
pub const SCOPE_MARKER: &str = "##SCOPE\t";

/// Marker emitted when a scope's `rpm -qa` exited non-zero. The query cannot
/// simply be allowed to fail: the container entrypoint runs under `set -e`, so
/// an unguarded failure would abort the whole script and lose every later
/// scope. Recording the failure keeps the script going and keeps an unreadable
/// sysroot from looking like an empty one.
pub const FAILED_MARKER: &str = "##FAILED\t";

/// Walks every sysroot that can hold installed target packages and dumps its
/// RPM database. `__SDK_QUERY__` is replaced with the host-SDK query, which is
/// the one case that has no `--root` (its database is found through custom
/// RPM_CONFIGDIR macros instead), and `__QUERY_FORMAT__` with the caller's
/// `--qf`.
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
    # `rpm -qa --root=DIR` *creates* DIR/var/lib/rpm when it finds no database
    # there, so querying a directory that holds none leaves an empty one behind.
    # A reporting command must not write to the state volume, and the scan walks
    # directories it does not own — see the includes loop below, which globs the
    # shared root's own content directories. Checked for the database file
    # rather than its directory so a root already carrying one of these
    # leftovers is not mistaken for a sysroot.
    [ -f "$root/var/lib/rpm/rpmdb.sqlite" ] || [ -f "$root/var/lib/rpm/Packages" ] || return 0
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
# ones hold no database of their own and drop out before being queried.
#
# The glob also matches the shared root's own content directories - etc/, opt/
# and var/ all sit beside the per-extension ones - so an extension is
# identified by the manifest `ext fetch` writes next to its content. Without
# that check the scan reports `includes:etc` as a scope and, worse, names a
# root that rpm would then seed a database into.
for inc_dir in "$AVOCADO_PREFIX"/includes/*/; do
    [ -d "$inc_dir" ] || continue
    [ -f "$inc_dir/avocado.yaml" ] || continue
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

/// One sysroot's raw dump: the rows exactly as `--qf` produced them, split on
/// tabs and trimmed but not otherwise interpreted. Each caller knows which tags
/// it asked for, so the mapping to a typed package belongs there.
#[derive(Debug, Default)]
pub struct ScopeDump {
    pub scope: String,
    pub root: String,
    pub rows: Vec<Vec<String>>,
    /// Its `rpm -qa` exited non-zero. Kept rather than dropped: a sysroot that
    /// could not be read must never be indistinguishable from one that holds
    /// no packages.
    pub failed: bool,
}

/// Split the container output into per-scope dumps.
///
/// A row is kept when its first field is non-empty. Anything before the first
/// `##SCOPE` is entrypoint noise and is discarded.
pub fn parse_scopes(output: &str) -> Vec<ScopeDump> {
    let mut scopes: Vec<ScopeDump> = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(SCOPE_MARKER) {
            let mut parts = rest.splitn(2, '\t');
            let scope = parts.next().unwrap_or_default().trim().to_string();
            let root = parts.next().unwrap_or_default().trim().to_string();
            if !scope.is_empty() {
                scopes.push(ScopeDump {
                    scope,
                    root,
                    ..Default::default()
                });
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix(FAILED_MARKER) {
            let scope = rest.trim();
            if let Some(s) = scopes.iter_mut().find(|s| s.scope == scope) {
                s.failed = true;
            }
            continue;
        }

        let Some(current) = scopes.last_mut() else {
            continue;
        };

        let row: Vec<String> = line.split('\t').map(|f| f.trim().to_string()).collect();
        if row.first().is_some_and(|f| !f.is_empty()) {
            current.rows.push(row);
        }
    }

    scopes
}

/// What `run_discovery` needs from its caller. Gathered into a struct because
/// the two commands pass it through from different shapes of their own.
pub struct ScanRequest<'a> {
    pub config_path: &'a str,
    pub target: &'a str,
    pub verbose: bool,
    pub container_args: Option<&'a Vec<String>>,
    pub sdk_arch: Option<String>,
    /// The `--qf` both the per-root queries and the SDK query are built with.
    pub query_format: &'a str,
}

/// What the scan container produced.
///
/// stderr is carried rather than dropped because the `##FAILED` guard makes the
/// script exit 0 by design: rpm's own diagnosis of an unreadable database is on
/// stderr and nowhere else, so a caller bailing on a failed scope would
/// otherwise have nothing to show for it.
pub struct ScanOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Run the discovery script in the SDK container and return its raw output.
///
/// Uses `run_in_container_capture` rather than `run_in_container_with_output`,
/// which the sibling commands use, for two reasons that both matter to a
/// security artifact:
///
/// - `_with_output` returns `Ok(None)` on a non-zero exit and discards the
///   captured stdout, so docker being down or the entrypoint aborting would
///   arrive as an empty scan — indistinguishable from an uninstalled project,
///   and answered with "run `avocado install`".
/// - it also drops stderr on success, which is where rpm's "cannot open
///   Packages database" lives.
pub async fn run_discovery(config: &Config, req: ScanRequest<'_>) -> Result<ScanOutput> {
    let container_image = config.get_sdk_image().cloned().ok_or_else(|| {
        anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
    })?;

    let sdk_query = SysrootType::Sdk(req.target.to_string())
        .get_rpm_query_config()
        .build_query_all_command(req.query_format);
    let command = DISCOVER_SCRIPT
        .replace("__SDK_QUERY__", &sdk_query)
        .replace("__QUERY_FORMAT__", req.query_format);

    if req.verbose {
        print_info(
            "Querying installed packages in every sysroot.",
            OutputLevel::Normal,
        );
    }

    let container = SdkContainer::from_config(req.config_path, config)?.verbose(req.verbose);
    let run_config = RunConfig {
        container_image,
        target: req.target.to_string(),
        command,
        verbose: req.verbose,
        // The RPM queries need only the entrypoint's base env vars
        // ($AVOCADO_PREFIX, $AVOCADO_SDK_PREFIX), not the full SDK env.
        source_environment: false,
        use_entrypoint: true,
        interactive: false,
        repo_url: config.get_sdk_repo_url(),
        repo_release: config.get_sdk_repo_release(),
        container_args: config.merge_sdk_container_args(req.container_args),
        sdk_arch: req.sdk_arch,
        ..Default::default()
    };

    let out = container.run_in_container_capture(run_config).await?;
    if !out.success {
        let target = req.target;
        anyhow::bail!(
            "Could not query the sysroots of target '{target}': the SDK container exited \
             non-zero. This is a container or SDK failure, not an empty project.{}",
            stderr_tail(&out.stderr)
        );
    }
    Ok(ScanOutput {
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// The last few lines of the container's stderr, ready to append to a bail.
///
/// Quoted rather than summarised: rpm's message is the diagnosis, and any
/// paraphrase of it here would be a guess. Empty when there is nothing to show,
/// so the caller's message reads normally in the ordinary case.
pub fn stderr_tail(stderr: &str) -> String {
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

/// The discovery script, with its placeholders left in. Exposed so tests can
/// assert on its shape without running a container.
#[cfg(test)]
pub fn discover_script() -> &'static str {
    DISCOVER_SCRIPT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_split_and_trimmed_per_scope() {
        let scopes = parse_scopes(concat!(
            "entrypoint noise before any marker\n",
            "##SCOPE\trootfs\t/opt/_avocado/x86_64/rootfs\n",
            "libssl3\t3.5.7-r0.0\tcore2_64\n",
            "##SCOPE\text:dev/app\t/opt/_avocado/x86_64/runtimes/dev/extensions/app\n",
            "##FAILED\text:dev/app\n",
        ));

        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0].scope, "rootfs");
        assert_eq!(
            scopes[0].rows,
            vec![vec!["libssl3", "3.5.7-r0.0", "core2_64"]]
        );
        assert!(!scopes[0].failed);
        assert!(scopes[1].rows.is_empty());
        assert!(scopes[1].failed);
    }

    #[test]
    fn the_script_reports_rpm_failures_instead_of_swallowing_them() {
        // Pinned as an absence rather than as a spelling: the guard may be
        // rewritten, but `|| true` must never come back — it is what made a
        // failed query indistinguishable from an empty sysroot.
        assert!(!discover_script().contains("|| true"));
        assert!(discover_script().contains("##FAILED"));
        assert!(discover_script().contains("printf '##FAILED\\t%s\\n'"));
    }

    #[test]
    fn the_script_walks_the_per_extension_includes_roots() {
        // A legacy-layout remote extension gets its own installroot under
        // includes/<name> (utils/ext_fetch.rs), so querying only the shared
        // includes database scans nothing for a project made of those.
        let s = discover_script();
        assert!(s.contains(r#"query_root "includes" "$AVOCADO_PREFIX/includes""#));
        assert!(s.contains(r#"for inc_dir in "$AVOCADO_PREFIX"/includes/*/"#));
        assert!(s.contains(r#"query_root "includes:$(basename "$inc_dir")""#));
        // The same glob matches the shared root's own etc/, opt/ and var/, so
        // the loop must discriminate rather than query everything it lists.
        assert!(s.contains(r#"[ -f "$inc_dir/avocado.yaml" ] || continue"#));
    }

    #[test]
    fn the_scan_never_seeds_a_database_into_what_it_is_reading() {
        // `rpm -qa --root=DIR` creates DIR/var/lib/rpm when none is there, so
        // an unguarded query writes into the state volume - observed leaving
        // empty rpmdbs under includes/etc, includes/opt and includes/var.
        // `avocado sbom` reports; it does not install.
        let s = discover_script();
        let guard = s
            .find(r#"[ -f "$root/var/lib/rpm/rpmdb.sqlite" ]"#)
            .expect("query_root checks for a database before querying");
        // The invocation, not the prose about it a few lines above.
        let query = s
            .find(r#"rpm -qa --root="$root""#)
            .expect("query_root queries rpm");
        assert!(guard < query, "the check has to run before the query");
    }
}
