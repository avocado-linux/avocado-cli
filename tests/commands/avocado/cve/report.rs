//! Tests for cve report command.
//!
//! These cover the checks that run before any container is started, so they
//! need neither Docker nor an installed project. What they guard is that a
//! report which cannot be trusted is refused outright: a vulnerability scanner
//! that exits 0 having read nothing is worse than one that fails.
//!
//! Each asserts the message it expects, not merely a non-zero exit. An
//! exit-code-only assertion proves nothing here: the command also fails for
//! want of Docker, and `run_cli_in_temp_*` runs `cargo run` from a directory
//! outside the workspace, so cargo itself exits non-zero without the CLI ever
//! starting. These run in the workspace instead, and every check they cover
//! happens in `load_source`, before any config or container work.

use crate::common;
use std::io::Write;

fn write_report(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

/// Run `cve report` against a report file and return its combined output.
fn run_with_report(name: &str, body: &str) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = write_report(dir.path(), name, body);
    let result = common::run_cli(&["cve", "report", "-f", path.to_str().unwrap()]);
    let output = format!("{}{}", result.stdout, result.stderr);
    (result.exit_code, output)
}

fn assert_fails_with(name: &str, body: &str, expected: &str) {
    let (exit_code, output) = run_with_report(name, body);
    assert_ne!(exit_code, 0, "must not succeed; output was:\n{output}");
    assert!(
        output.contains(expected),
        "expected the failure to mention {expected:?}, got:\n{output}"
    );
}

#[test]
fn test_long_help() {
    common::assert_cmd(&["cve", "report", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["cve", "report", "-h"], None, None);
}

#[test]
fn test_file_is_required() {
    common::refute_cmd(&["cve", "report"], None, None);
}

#[test]
fn test_fail_on_score_is_advertised() {
    let result = common::run_cli(&["cve", "report", "--help"]);
    let output = format!("{}{}", result.stdout, result.stderr);
    assert!(
        output.contains("--fail-on-score"),
        "the release-gate flag must be discoverable, got:\n{output}"
    );
}

#[test]
fn test_missing_report_file_fails() {
    let result = common::run_cli(&["cve", "report", "-f", "no-such.json"]);
    let output = format!("{}{}", result.stdout, result.stderr);
    assert_ne!(result.exit_code, 0, "a missing report must not succeed");
    assert!(
        output.contains("Failed to read CVE report"),
        "expected the read error, got:\n{output}"
    );
}

#[test]
fn test_report_without_packages_map_fails() {
    // Neither map carries #[serde(default)], so an absent one is a parse
    // error rather than a silently empty map correlating to zero CVEs.
    assert_fails_with(
        "no-packages.json",
        r#"{"version": "1", "status": "Unpatched", "recipes": {}}"#,
        "missing field `packages`",
    );
}

#[test]
fn test_report_with_empty_packages_map_fails() {
    assert_fails_with(
        "empty-packages.json",
        r#"{"version": "1", "status": "Unpatched", "recipes": {}, "packages": {}}"#,
        "has no 'packages' map",
    );
}

#[test]
fn test_report_without_recipes_map_fails() {
    // A missing recipes map would report zero CVEs for everything.
    assert_fails_with(
        "no-recipes.json",
        r#"{"version": "1", "status": "Unpatched", "packages": {}}"#,
        "missing field `recipes`",
    );
}

#[test]
fn test_report_with_empty_recipes_map_fails() {
    // The shape a producer emits when cve-check was never inherited: every
    // package present, nothing to correlate against, zero CVEs found.
    assert_fails_with(
        "empty-recipes.json",
        r#"{"version": "1", "status": "Unpatched", "recipes": {},
            "packages": {"libssl3": {"recipe": "openssl", "version": "3.5.7-r0.0"}}}"#,
        "empty 'recipes' map",
    );
}

#[test]
fn test_report_generated_for_another_status_fails() {
    // Patched issues are not live findings; correlating them would present
    // already-fixed CVEs as if they still affected the image.
    assert_fails_with(
        "patched.json",
        r#"{"version": "1", "status": "Patched",
            "recipes": {"openssl": {"cves": [{"id": "CVE-2026-1"}]}},
            "packages": {"libssl3": {"recipe": "openssl", "version": "3.5.7-r0.0"}}}"#,
        "not 'Unpatched'",
    );
}

#[test]
fn test_malformed_json_fails() {
    assert_fails_with("broken.json", "{not json", "Failed to parse CVE report");
}

/// The flag spellings the rest of the CLI uses.
///
/// Asserted against the parser rather than by reading the source: the failure
/// mode is that a value clap rejects — or silently splits — never reaches the
/// command, and only running the binary shows that.
#[test]
fn test_config_flag_matches_its_76_siblings() {
    // `-c` was the outlier. `insert_config_flag` in tests/common hardcodes
    // `-C`, so the shared helper could not drive this subcommand at all.
    let result = common::run_cli(&["cve", "report", "-C", "avocado.yaml", "-f", "no-such.json"]);
    assert!(
        result.stderr.contains("Failed to read CVE report"),
        "-C should be accepted and the run should reach load_source; got:\n{}{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn test_container_arg_takes_a_hyphenated_value() {
    // `--container-args` with `value_delimiter = ' '` rejected `--privileged`
    // outright: clap read it as an unknown flag. The 36 sibling uses spell it
    // `--container-arg` with `allow_hyphen_values`.
    //
    // The other half of that finding — that the old spelling split
    // `-v /my dir:/b` into three arguments — is fixed by the same attributes
    // but is not assertable here: `run_cli` shells out through `cargo run`,
    // which re-splits a value containing a space before the CLI ever sees it.
    let result = common::run_cli(&[
        "cve",
        "report",
        "-f",
        "no-such.json",
        "--container-arg",
        "--privileged",
    ]);
    assert!(
        result.stderr.contains("Failed to read CVE report"),
        "--privileged should parse and reach load_source; got:\n{}{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn test_fail_on_score_off_the_cvss_scale_is_refused() {
    // `99` (the 0-100 confusion) and `nan` used to parse and then match no
    // CVE, so CI exited 0 against a report full of 9.8s.
    for bad in ["99", "nan", "inf"] {
        let result = common::run_cli(&[
            "cve",
            "report",
            "-f",
            "no-such.json",
            "--fail-on-score",
            bad,
        ]);
        assert_ne!(result.exit_code, 0, "{bad} must be refused");
        assert!(
            result.stderr.contains("is not a CVSS score"),
            "expected a range error for {bad}, got:\n{}{}",
            result.stdout,
            result.stderr
        );
    }
}

#[test]
fn test_runs_on_is_refused_rather_than_ignored() {
    // The container helper this command uses has no remote branch, so honouring
    // --runs-on would silently scan the local machine and attribute the report
    // to the remote one.
    let result = common::run_cli(&[
        "--runs-on",
        "user@buildbox",
        "cve",
        "report",
        "-f",
        "no-such.json",
    ]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("--runs-on"),
        "expected the refusal to name the flag, got:\n{}{}",
        result.stdout,
        result.stderr
    );
}
