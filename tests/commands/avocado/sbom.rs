//! Tests for the sbom command.
//!
//! Everything this command reports comes from inside the SDK container, so what
//! is testable here is the layer in front of it: flag parsing, the refusals
//! that run before any container is started, and target resolution. That layer
//! is worth pinning on its own — a flag clap silently splits or rejects never
//! reaches the command, and only running the binary shows it.
//!
//! Each test asserts the message it expects rather than a non-zero exit. An
//! exit-code-only assertion proves nothing here: the command also fails for
//! want of Docker, so every check below has to be one that happens first.

use crate::common;
use serial_test::serial;

/// The message target resolution produces, which is as far as a run gets in a
/// workspace whose config names no default target. Reaching it means the flags
/// parsed and the config loaded — everything this file can observe.
///
/// Every test that asserts on this string is `#[serial]`, whether it asserts
/// the string is present or absent. tests/interpolation.rs compiles into the
/// same binary and sets `AVOCADO_TARGET` process-globally;
/// `resolve_target_required` reads it after the CLI arg, so a run landing
/// inside the window between that set and its `remove_var` resolves a target
/// and never emits this message. That makes a test asserting its presence
/// flake, which is visible — and a test asserting its absence pass by
/// construction, which is not. `#[serial]` only excludes other `#[serial]`
/// tests, so opting out is what left the window open.
const REACHED_TARGET_RESOLUTION: &str = "No target architecture specified";

fn assert_parses(args: &[&str]) {
    let result = common::run_cli(args);
    assert!(
        result.stderr.contains(REACHED_TARGET_RESOLUTION),
        "{args:?} should parse and reach target resolution; got:\n{}{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn test_long_help() {
    common::assert_cmd(&["sbom", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["sbom", "-h"], None, None);
}

#[test]
#[serial]
fn test_config_flag_matches_its_siblings() {
    // `-C` is how every other command spells the config path; a new command
    // that spells it `-c` would be the outlier.
    assert_parses(&["sbom", "-C", "avocado.yaml"]);
}

#[test]
#[serial]
fn test_container_arg_takes_a_hyphenated_value() {
    // `value_delimiter = ' '` would reject `--privileged` as an unknown flag.
    // The sibling spelling is `--container-arg` with `allow_hyphen_values`.
    assert_parses(&["sbom", "--container-arg", "--privileged"]);
}

#[test]
#[serial]
fn test_include_sdk_is_opt_in() {
    // Both spellings have to parse; which one is the default is asserted in the
    // unit tests, where the scope list can be inspected without a container.
    assert_parses(&["sbom"]);
    assert_parses(&["sbom", "--include-sdk"]);

    let result = common::run_cli(&["sbom", "--help"]);
    assert!(
        result.stdout.contains("--include-sdk"),
        "the flag has to be discoverable from --help; got:\n{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("build host"),
        "--help should say why the SDK is excluded, not just that it is; got:\n{}",
        result.stdout
    );
}

#[test]
#[serial]
fn test_output_path_is_optional() {
    // Writing to stdout is the default so the document can be piped; -o exists
    // for the case where the summary lines would otherwise be mixed into it.
    assert_parses(&["sbom"]);
    assert_parses(&["sbom", "-o", "sbom.json"]);
}

#[test]
#[serial]
fn test_runs_on_is_refused_rather_than_ignored() {
    // The container helper has no remote branch, so honouring --runs-on would
    // describe this machine's sysroots in a document named for another host.
    // Asserted before target resolution, which is where the refusal sits.
    let result = common::run_cli(&["--runs-on", "user@buildbox", "sbom"]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("--runs-on"),
        "expected the refusal to name the flag, got:\n{}{}",
        result.stdout,
        result.stderr
    );
    assert!(
        !result.stderr.contains(REACHED_TARGET_RESOLUTION),
        "the refusal must come before any other work, got:\n{}",
        result.stderr
    );
}

#[test]
fn test_a_missing_config_is_refused_before_the_container() {
    let result = common::run_cli(&["sbom", "-C", "no-such-avocado.yaml"]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("config"),
        "expected a config error, got:\n{}{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn test_the_command_offers_no_way_to_upload_the_document() {
    // Validation is a thing the operator does, deliberately, to a document
    // they have read — not something a flag on this command can start. The
    // service that runs the reference SPDX tools keeps every upload for about
    // ten days and serves it back unauthenticated, and this document names
    // every package and version on the target. No flag here may put it there.
    let result = common::run_cli(&["sbom", "--help"]);
    for flag in ["--validate", "--validator-url", "--upload"] {
        assert!(
            !result.stdout.contains(flag),
            "{flag} would let the command publish the inventory; got:\n{}",
            result.stdout
        );
    }
}

#[test]
fn test_an_unknown_output_format_is_refused() {
    let result = common::run_cli(&["sbom", "--output", "yaml"]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("invalid value"),
        "expected clap to reject the value, got:\n{}{}",
        result.stdout,
        result.stderr
    );
}
