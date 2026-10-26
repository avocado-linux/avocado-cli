//! `avocado config schema` and `avocado config validate`, as avocado-mcp
//! calls them: outside a project, with nothing on stdout but the payload.
//! `get-config-schema` shows the schema, and `validate-yaml` pipes YAML to
//! `config validate -c - --output json`.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn config_schema_prints_the_embedded_schema() {
    // A directory with no avocado.yaml: the command must not load a config.
    let output = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .args(["config", "schema"])
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .output()
        .expect("failed to run `avocado config schema`");

    assert!(
        output.status.success(),
        "`avocado config schema` exited with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = include_str!("../schemas/avocado-config.json");
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

/// Pipe `yaml` to `avocado config validate -c - --output json`.
fn validate_stdin(yaml: &str) -> (Option<i32>, serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .args(["config", "validate", "-c", "-", "--output", "json"])
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run `avocado config validate`");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}): {stdout}"));
    (output.status.code(), payload)
}

#[test]
fn config_validate_reads_stdin_and_prints_json() {
    let (code, payload) = validate_stdin("default_target: qemux86-64\nrutimes: {}\n");
    assert_eq!(code, Some(0), "{payload}");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["errors"], serde_json::json!([]));
    assert_eq!(
        payload["warnings"].as_array().unwrap().len(),
        1,
        "{payload}"
    );
}

#[test]
fn config_validate_exits_1_and_lists_every_error() {
    // An unset env var must not print its warning on stdout.
    let (code, payload) = validate_stdin(
        "default_target: \"{{ env.AVOCADO_TEST_UNSET_VAR }}\"\nruntimes:\n  dev:\n    extensions: 5\n    var: {hardware: nope}\n",
    );
    assert_eq!(code, Some(1), "{payload}");
    assert_eq!(payload["ok"], false);
    let paths: Vec<&str> = payload["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"/runtimes/dev/extensions"), "{payload}");
    assert!(paths.contains(&"/runtimes/dev/var/hardware"), "{payload}");
}

/// A value read from the environment must not reach any output: not the
/// JSON report, not the human report, not stderr. The template the user
/// wrote is shown instead.
#[test]
fn config_validate_never_prints_an_env_value() {
    const MARKER: &str = "AVOCADO_LEAK_MARKER_0123456789";
    let yaml = "source_date_epoch: \"{{ env.AVOCADO_TEST_MARKER }}\"\nruntimes:\n  dev:\n    extensions:\n      app: \"{{ env.AVOCADO_TEST_MARKER }}\"\n";
    for json in [true, false] {
        let mut args = vec!["config", "validate", "-c", "-"];
        if json {
            args.extend(["--output", "json"]);
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_avocado"))
            .args(&args)
            .env("AVOCADO_TEST_MARKER", MARKER)
            .current_dir(env!("CARGO_TARGET_TMPDIR"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run `avocado config validate`");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(yaml.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stdout}{stderr}");
        assert!(!stdout.contains(MARKER), "json={json}: {stdout}");
        assert!(!stderr.contains(MARKER), "json={json}: {stderr}");
        assert!(
            format!("{stdout}{stderr}").contains("{{ env.AVOCADO_TEST_MARKER }}"),
            "json={json}: {stdout}{stderr}"
        );
    }
}
