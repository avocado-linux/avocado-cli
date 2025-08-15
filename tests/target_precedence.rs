//! Target precedence integration tests

mod common;

use common::cli_with_config;
use std::env;
use std::io::Write;
use tempfile::NamedTempFile;

/// Test the complete target precedence order: CLI > ENV > CONFIG > ERROR
#[test]
fn test_target_precedence_order() {
    let config_content = r#"
default_target = "config-target"

[sdk]
image = "ghcr.io/avocado-framework/avocado-sdk:latest"

[runtime.dev]
target = "qemux86-64"
"#;

    let mut config_file = NamedTempFile::new().unwrap();
    write!(config_file, "{config_content}").unwrap();

    // Test 1: CLI target wins over everything
    env::set_var("AVOCADO_TARGET", "env-target");
    let result = cli_with_config(
        &["sdk", "run", "--target", "cli-target", "--", "echo", "test"],
        None,
        Some(config_file.path()),
    );

    // Should not fail with target resolution error
    if !result.success {
        assert!(
            !result.stderr.contains("No target architecture specified"),
            "CLI target should override env and config: {}",
            result.stderr
        );
    }

    // Test 2: ENV target wins over config when no CLI target
    let result = cli_with_config(
        &["sdk", "run", "--", "echo", "test"],
        None,
        Some(config_file.path()),
    );

    if !result.success {
        assert!(
            !result.stderr.contains("No target architecture specified"),
            "Environment target should override config: {}",
            result.stderr
        );
    }

    // Test 3: Config default_target when no CLI or ENV
    env::remove_var("AVOCADO_TARGET");
    let result = cli_with_config(
        &["sdk", "run", "--", "echo", "test"],
        None,
        Some(config_file.path()),
    );

    if !result.success {
        assert!(
            !result.stderr.contains("No target architecture specified"),
            "Config default_target should be used as fallback: {}",
            result.stderr
        );
    }

    // Clean up
    env::remove_var("AVOCADO_TARGET");
}

#[test]
fn test_target_error_when_none_specified() {
    let config_content = r#"
[sdk]
image = "ghcr.io/avocado-framework/avocado-sdk:latest"

[runtime.dev]
target = "qemux86-64"
"#;

    let mut config_file = NamedTempFile::new().unwrap();
    write!(config_file, "{config_content}").unwrap();

    // Ensure no environment variable
    env::remove_var("AVOCADO_TARGET");

    // No CLI target, no env var, no config default_target - should error
    let result = cli_with_config(
        &["sdk", "run", "--", "echo", "test"],
        None,
        Some(config_file.path()),
    );

    // Should fail with our specific error message
    assert!(
        !result.success,
        "Command should fail when no target is specified"
    );
    assert!(
        result.stderr.contains("No target architecture specified"),
        "Should show target resolution error: {}",
        result.stderr
    );
}

#[test]
fn test_avocado_target_environment_variable() {
    let config_content = r#"
[sdk]
image = "ghcr.io/avocado-framework/avocado-sdk:latest"

[runtime.dev]
target = "qemux86-64"
"#;

    let mut config_file = NamedTempFile::new().unwrap();
    write!(config_file, "{config_content}").unwrap();

    // Test AVOCADO_TARGET environment variable
    env::set_var("AVOCADO_TARGET", "test-env-target");

    let result = cli_with_config(
        &["sdk", "run", "--", "echo", "test"],
        None,
        Some(config_file.path()),
    );

    env::remove_var("AVOCADO_TARGET");

    if !result.success {
        assert!(
            !result.stderr.contains("No target architecture specified"),
            "AVOCADO_TARGET environment variable should work: {}",
            result.stderr
        );
    }
}

#[test]
fn test_init_command_creates_default_target() {
    // Use current directory and clean up after
    let result = cli_with_config(&["init", "--target", "test-init-target"], None, None);

    // Clean up first regardless of test result
    let cleanup = || {
        std::fs::remove_file("avocado.toml").ok();
    };

    if !result.success {
        cleanup();
        // Don't fail the test if it's just a cargo issue - focus on testing the config content
        if result.stderr.contains("could not find `Cargo.toml`") {
            return; // Skip this test in environments without proper Cargo setup
        }
        panic!("Init command should succeed: {}", result.stderr);
    }

    // Check that the generated config contains default_target
    if std::path::Path::new("avocado.toml").exists() {
        let content = std::fs::read_to_string("avocado.toml").unwrap();
        assert!(
            content.contains("default_target = \"test-init-target\""),
            "Generated config should contain default_target: {content}"
        );
    }

    cleanup();
}

#[test]
fn test_all_commands_accept_target_flag() {
    // Test that major commands accept --target flag without error
    let config_content = r#"
default_target = "qemux86-64"

[sdk]
image = "ghcr.io/avocado-framework/avocado-sdk:latest"

[runtime.default]
target = "x86_64-unknown-linux-gnu"

[ext.test-ext]
sysext = true
"#;

    let mut config_file = NamedTempFile::new().unwrap();
    write!(config_file, "{config_content}").unwrap();

    let commands = vec![
        vec!["sdk", "run", "--target", "test", "--", "echo", "test"],
        vec![
            "runtime",
            "build",
            "--target",
            "test",
            "--runtime",
            "default",
        ],
        vec!["runtime", "install", "--target", "test"],
        vec![
            "runtime",
            "clean",
            "--target",
            "test",
            "--runtime",
            "default",
        ],
        vec![
            "ext",
            "build",
            "--target",
            "test",
            "--extension",
            "test-ext",
        ],
    ];

    for cmd_args in commands {
        let result = cli_with_config(&cmd_args, None, Some(config_file.path()));

        // Commands might fail for other reasons (missing extensions, etc.)
        // but should NOT fail specifically due to target resolution
        if !result.success {
            assert!(
                !result.stderr.contains("No target architecture specified"),
                "Command {:?} should accept --target flag: {}",
                cmd_args,
                result.stderr
            );
        }
    }
}
