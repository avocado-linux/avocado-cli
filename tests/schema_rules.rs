//! The cross-field rules in `schemas/avocado-config.json` must reject what the
//! CLI rejects and accept what it accepts. Editors and the MCP `validate-yaml`
//! check configs against this file, so a rule that is too loose lets a broken
//! config through and a rule that is too strict flags a working one.
//!
//! `config_lint` cannot see these rules: its walker follows a `$ref` and drops
//! the keywords next to it, which is where the var rules live.

use serde_json::Value;

fn validator() -> jsonschema::Validator {
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/avocado-config.json")).unwrap();
    jsonschema::validator_for(&schema).expect("schema compiles")
}

fn config(yaml: &str) -> Value {
    serde_yaml::from_str(yaml).unwrap()
}

fn accepts(yaml: &str) -> bool {
    validator().is_valid(&config(yaml))
}

#[test]
fn fit_key_with_fit_unsigned_is_rejected() {
    assert!(!accepts(
        "runtimes:\n  dev:\n    signing:\n      fit_key: a\n      fit_unsigned: true\n"
    ));
}

#[test]
fn fit_key_alone_or_blank_is_accepted() {
    assert!(accepts(
        "runtimes:\n  dev:\n    signing:\n      fit_key: a\n"
    ));
    assert!(accepts(
        "runtimes:\n  dev:\n    signing:\n      fit_key: a\n      fit_unsigned: false\n"
    ));
    // `get_runtime_fit_signing` treats a blank key as absent.
    assert!(accepts(
        "runtimes:\n  dev:\n    signing:\n      fit_key: \"\"\n      fit_unsigned: true\n"
    ));
    assert!(accepts(
        "runtimes:\n  dev:\n    signing:\n      fit_key: \" \"\n      fit_unsigned: true\n"
    ));
}

#[test]
fn recovery_without_encrypt_is_rejected() {
    assert!(!accepts(
        "runtimes:\n  dev:\n    var:\n      recovery: master\n"
    ));
    assert!(!accepts(
        "runtimes:\n  dev:\n    var:\n      encrypt: false\n      recovery: master\n"
    ));
    assert!(accepts(
        "runtimes:\n  dev:\n    var:\n      encrypt: true\n      recovery: master\n"
    ));
}

#[test]
fn hardware_none_needs_a_non_blank_recovery() {
    assert!(!accepts(
        "runtimes:\n  dev:\n    var:\n      encrypt: true\n      hardware: none\n"
    ));
    assert!(!accepts(
        "runtimes:\n  dev:\n    var:\n      encrypt: true\n      hardware: none\n      recovery: \" \"\n"
    ));
}

/// A `target-<name>:` override is merged onto the base runtime by the CLI, so
/// it may leave out fields the base already sets.
#[test]
fn an_override_inherits_recovery_and_encrypt_from_the_base() {
    let base = "runtimes:\n  dev:\n    var:\n      encrypt: true\n      recovery: master\n";
    assert!(accepts(&format!(
        "{base}    target-qemux86-64:\n      var:\n        hardware: none\n"
    )));
    assert!(accepts(&format!(
        "{base}    target-qemux86-64:\n      var:\n        recovery: other\n"
    )));
}
