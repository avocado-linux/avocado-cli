//! Integration tests for configuration interpolation

use serial_test::serial;
use std::env;
use std::fs;
use std::path::PathBuf;

fn get_interpolation_test_config() -> PathBuf {
    std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("with-interpolation.yaml")
}

#[test]
#[serial]
fn test_env_var_interpolation() {
    // Use unique env var names to avoid parallel test conflicts
    env::set_var("TEST_PKG_ENV_VAR_INTERP", "test-package-1.0");
    env::set_var("EXT_VERSION_ENV_VAR_INTERP", "2.5.1");

    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    // Verify that runtime dev dependencies include interpolated env var
    let runtime = config.runtimes.as_ref().unwrap();
    let dev = runtime.get("dev").unwrap();

    if let Some(deps) = &dev.dependencies {
        // Check that env var was interpolated
        if let Some(env_pkg) = deps.get("env-pkg") {
            let pkg_str = env_pkg.as_str().unwrap();
            assert_eq!(pkg_str, "test-package-1.0");
        }
    }

    // Clean up
    env::remove_var("TEST_PKG_ENV_VAR_INTERP");
    env::remove_var("EXT_VERSION_ENV_VAR_INTERP");
}

#[test]
#[serial]
fn test_missing_env_var_warning() {
    // Ensure the env vars used in the test config are not set
    env::remove_var("TEST_PKG_ENV_VAR_INTERP");
    env::remove_var("EXT_VERSION_ENV_VAR_INTERP");

    let config_path = get_interpolation_test_config();

    // Should succeed but replace with empty string
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    let runtime = config.runtimes.as_ref().unwrap();
    let dev = runtime.get("dev").unwrap();

    if let Some(deps) = &dev.dependencies {
        if let Some(env_pkg) = deps.get("env-pkg") {
            let pkg_str = env_pkg.as_str().unwrap();
            // Should be empty string since TEST_PKG_ENV_VAR_INTERP is not set in this test
            assert_eq!(pkg_str, "");
        }
    }
}

#[test]
fn test_config_self_reference() {
    let config_path = get_interpolation_test_config();
    let _config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    // Check that derived_image contains the interpolated base_image value
    let content = fs::read_to_string(&config_path).unwrap();
    let parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();

    // Apply interpolation manually to check
    let mut parsed_copy = parsed.clone();
    avocado_cli::utils::interpolation::interpolate_config(&mut parsed_copy, None).unwrap();

    let derived = parsed_copy.get("derived_image").unwrap().as_str().unwrap();
    assert_eq!(derived, "ghcr.io/avocado/base:latest");
}

#[test]
fn test_nested_config_reference() {
    let config_path = get_interpolation_test_config();
    let content = fs::read_to_string(&config_path).unwrap();
    let mut parsed: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();

    avocado_cli::utils::interpolation::interpolate_config(&mut parsed, None).unwrap();

    let reference = parsed.get("reference_nested").unwrap().as_str().unwrap();
    assert_eq!(reference, "nested_value");
}

#[test]
#[serial]
fn test_avocado_target_from_env() {
    env::set_var("AVOCADO_TARGET", "aarch64-unknown-linux-gnu");

    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    let runtime = config.runtimes.as_ref().unwrap();
    let dev = runtime.get("dev").unwrap();

    assert_eq!(dev.target.as_ref().unwrap(), "aarch64-unknown-linux-gnu");

    env::remove_var("AVOCADO_TARGET");
}

#[test]
#[serial]
fn test_avocado_target_from_config() {
    env::remove_var("AVOCADO_TARGET");

    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    let runtime = config.runtimes.as_ref().unwrap();
    let dev = runtime.get("dev").unwrap();

    // Should use default_target from config
    assert_eq!(dev.target.as_ref().unwrap(), "x86_64-unknown-linux-gnu");
}

#[test]
#[serial]
fn test_avocado_target_unavailable() {
    env::remove_var("AVOCADO_TARGET");

    // Create a test config without default_target
    let test_yaml = r#"
runtimes:
  dev:
    target: "{{ avocado.target }}"
"#;

    let mut parsed: serde_yaml::Value = serde_yaml::from_str(test_yaml).unwrap();
    avocado_cli::utils::interpolation::interpolate_config(&mut parsed, None).unwrap();

    // Should leave template as-is
    let runtime = parsed.get("runtimes").unwrap();
    let dev = runtime.get("dev").unwrap();
    let target = dev.get("target").unwrap().as_str().unwrap();

    assert_eq!(target, "{{ avocado.target }}");
}

/// Helper to get the full error chain as a string for assertions.
fn error_chain_string(err: &anyhow::Error) -> String {
    err.chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

#[test]
fn test_missing_config_path_error() {
    let test_yaml = r#"
base: "value"
reference: "{{ config.nonexistent.path }}"
"#;

    let mut parsed: serde_yaml::Value = serde_yaml::from_str(test_yaml).unwrap();
    let result = avocado_cli::utils::interpolation::interpolate_config(&mut parsed, None);

    assert!(result.is_err());
    let err = result.unwrap_err();
    let full_error = error_chain_string(&err);
    // Should contain both the location and the "not found" message
    assert!(
        full_error.contains("not found"),
        "Expected 'not found' in error, got: {full_error}"
    );
    assert!(
        full_error.contains("reference"),
        "Expected 'reference' location in error, got: {full_error}"
    );
}

#[test]
#[serial]
fn test_multiple_interpolation_types() {
    env::set_var("TEST_PKG_ENV_VAR_INTERP", "test-pkg");
    env::set_var("AVOCADO_TARGET", "riscv64-unknown-linux-gnu");

    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    let runtime = config.runtimes.as_ref().unwrap();
    let dev = runtime.get("dev").unwrap();

    if let Some(deps) = &dev.dependencies {
        // Check env interpolation
        if let Some(env_pkg) = deps.get("env-pkg") {
            assert_eq!(env_pkg.as_str().unwrap(), "test-pkg");
        }

        // Check config interpolation
        if let Some(base_pkg) = deps.get("base-pkg") {
            assert_eq!(base_pkg.as_str().unwrap(), "ghcr.io/avocado/base");
        }

        // Check avocado interpolation
        if let Some(target_pkg) = deps.get("target-pkg") {
            assert_eq!(
                target_pkg.as_str().unwrap(),
                "avocado-os-riscv64-unknown-linux-gnu"
            );
        }
    }

    env::remove_var("TEST_PKG_ENV_VAR_INTERP");
    env::remove_var("AVOCADO_TARGET");
}

#[test]
#[serial]
fn test_combined_interpolation() {
    env::set_var("AVOCADO_TARGET", "armv7-unknown-linux-gnueabihf");

    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    let runtime = config.runtimes.as_ref().unwrap();
    let prod = runtime.get("prod").unwrap();

    if let Some(deps) = &prod.dependencies {
        if let Some(combined) = deps.get("combined") {
            assert_eq!(
                combined.as_str().unwrap(),
                "ghcr.io/avocado/base-armv7-unknown-linux-gnueabihf"
            );
        }
    }

    env::remove_var("AVOCADO_TARGET");
}

#[test]
fn test_sdk_image_interpolation() {
    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    // SDK image should be interpolated from distro fields
    assert_eq!(
        config.sdk.as_ref().unwrap().image.as_ref().unwrap(),
        "docker.io/avocadolinux/sdk:apollo-edge"
    );
}

#[test]
fn test_distro_config_loaded() {
    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    // Check that distro is loaded
    assert!(config.distro.is_some());
    let distro = config.distro.as_ref().unwrap();
    assert_eq!(distro.channel.as_ref().unwrap(), "apollo-edge");
    assert_eq!(distro.version.as_ref().unwrap(), "0.1.0");
}

#[test]
fn test_config_distro_interpolation_in_sdk() {
    let config_path = get_interpolation_test_config();
    let config = avocado_cli::utils::config::Config::load(&config_path).unwrap();

    // SDK image should use config.distro interpolation
    let sdk = config.sdk.as_ref().unwrap();
    assert_eq!(
        sdk.image.as_ref().unwrap(),
        "docker.io/avocadolinux/sdk:apollo-edge"
    );

    // SDK dependencies should use config.distro.version interpolation
    let deps = sdk.packages.as_ref().unwrap();
    let toolchain_version = deps.get("avocado-sdk-toolchain").unwrap();
    assert_eq!(toolchain_version.as_str().unwrap(), "0.1.0");
}
