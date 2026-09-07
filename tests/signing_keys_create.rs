//! `signing-keys create` must reject a duplicate name before generating anything.

use avocado_cli::commands::signing_keys::SigningKeysCreateCommand;
use serial_test::serial;
use std::collections::BTreeSet;
use std::env;

/// `read_dir` has no ordering guarantee, so compare the directory as a set.
fn entries(dir: &std::path::Path) -> BTreeSet<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect()
}

fn create(name: &str, algorithm: &str) -> anyhow::Result<()> {
    SigningKeysCreateCommand::new(
        Some(name.to_string()),
        None,
        None,
        None,
        None,
        false,
        "prompt".to_string(),
        algorithm.to_string(),
    )
    .execute()
}

#[test]
#[serial] // AVOCADO_SIGNING_KEYS_DIR is process-global
fn duplicate_name_does_not_generate_a_key() {
    let dir = tempfile::tempdir().unwrap();
    env::set_var("AVOCADO_SIGNING_KEYS_DIR", dir.path());

    create("dup", "ed25519").unwrap();
    let after_first = entries(dir.path());

    // rsa2048 shells out to openssl and writes a PEM pair; the duplicate name
    // has to be caught before any of that happens.
    let err = create("dup", "rsa2048").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    assert_eq!(
        after_first,
        entries(dir.path()),
        "second create left files behind"
    );
}
