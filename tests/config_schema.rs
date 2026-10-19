//! `avocado config schema` prints the avocado.yaml schema this binary embeds.
//! avocado-mcp reads it from here for `validate-yaml` and `get-config-schema`,
//! so the output must be the schema file byte for byte, with nothing else on
//! stdout, and it must work outside a project.

use std::process::Command;

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
