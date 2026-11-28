//! `avocado rubicon show --output json`: stdout carries the JSON alone.
//! Resolving `rubicon.default` interpolates the project config, and an unset
//! `{{ env.* }}` warns through `print_warning`, a `println!`. Under JSON that
//! prose must not reach stdout.

use std::process::Command;

#[test]
fn rubicon_show_json_keeps_config_warnings_off_stdout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("avocado.yaml"),
        "default_target: qemux86-64\nsdk:\n  image: \"{{ env.AVOCADO_TEST_RUBICON_UNSET }}\"\n",
    )
    .unwrap();
    // No board named and no `rubicon.default`: the command fails after the
    // config is read, before any hub is asked, so this needs no network.
    let output = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .args(["rubicon", "show", "--output", "json"])
        .current_dir(dir.path())
        .env_remove("AVOCADO_TEST_RUBICON_UNSET")
        .output()
        .expect("failed to run `avocado rubicon show`");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("rubicon.default"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}
