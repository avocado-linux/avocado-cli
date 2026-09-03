//! `signing-keys create` must reject a duplicate name before generating anything.

use avocado_cli::commands::signing_keys::SigningKeysCreateCommand;
use std::env;

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
fn duplicate_name_does_not_generate_a_key() {
    let dir = tempfile::tempdir().unwrap();
    env::set_var("AVOCADO_SIGNING_KEYS_DIR", dir.path());

    create("dup", "ed25519").unwrap();
    let after_first: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    // rsa2048 shells out to openssl and writes a PEM pair; the duplicate name
    // has to be caught before any of that happens.
    let err = create("dup", "rsa2048").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    let after_second: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(after_first, after_second, "second create left files behind");
}
