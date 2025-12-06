#![allow(dead_code)] // Test utilities - some functions may not be used in all tests

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct TestResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Get the appropriate command for the CLI (Rust only)
fn get_cli_command() -> (String, Vec<String>) {
    // Always use Rust CLI via cargo
    (
        "cargo".to_string(),
        vec!["run".to_string(), "--".to_string()],
    )
}

/// Execute the CLI with the given arguments and configuration
fn execute_cli(args: &[&str], working_dir: Option<&Path>) -> TestResult {
    let (base_cmd, base_args) = get_cli_command();

    // Create a shell script that sets up a clean environment and runs the CLI
    let env_script = format!(
        r#"#!/bin/bash
# Clear AVOCADO_TARGET to ensure clean state
unset AVOCADO_TARGET
# Set AVOCADO_TARGET only if it was explicitly set in the test
if [ -n "$TEST_AVOCADO_TARGET" ]; then
    export AVOCADO_TARGET="$TEST_AVOCADO_TARGET"
fi
# Run the CLI command
{} {} {}
"#,
        base_cmd,
        base_args.join(" "),
        args.join(" ")
    );

    let mut cmd = Command::new("bash");
    cmd.arg("-c");
    cmd.arg(env_script);

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    // Start with a clean environment and only add what we need
    cmd.env_clear();

    // Add essential environment variables
    for (key, value) in std::env::vars() {
        match key.as_str() {
            "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "CARGO_TARGET_DIR"
            | "CARGO_MANIFEST_DIR" | "CARGO_PKG_NAME" | "CARGO_PKG_VERSION" | "RUST_BACKTRACE"
            | "RUSTC" | "RUSTUP_HOME" | "CARGO_HOME" => {
                cmd.env(key, value);
            }
            "AVOCADO_TARGET" => {
                // Pass through as TEST_AVOCADO_TARGET so the script can decide whether to use it
                cmd.env("TEST_AVOCADO_TARGET", value);
            }
            _ if key.starts_with("CARGO_") || key.starts_with("RUST_") => {
                cmd.env(key, value);
            }
            _ => {} // Skip other environment variables to prevent pollution
        }
    }

    let output = cmd.output().expect("Failed to execute command");

    TestResult {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(-1),
    }
}

/// Insert config flag after the subcommand(s)
fn insert_config_flag(args: &[&str], config_path: &str) -> Vec<String> {
    if args.is_empty() {
        return args.iter().map(|s| s.to_string()).collect();
    }

    let mut new_args = Vec::new();

    // Add subcommand
    new_args.push(args[0].to_string());

    // Add sub-subcommand if present
    if args.len() > 1 {
        new_args.push(args[1].to_string());
    }

    // Insert config flag
    new_args.push("-C".to_string());
    new_args.push(config_path.to_string());

    // Add remaining args
    if args.len() > 2 {
        new_args.extend(args[2..].iter().map(|s| s.to_string()));
    }

    new_args
}

/// Get the path to the minimal.toml config file
fn get_minimal_config_path() -> PathBuf {
    std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("minimal.yaml")
}

/// Generate a unique temporary directory name
pub fn generate_temp_dir_name() -> String {
    let pid = std::process::id();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let thread_name = std::thread::current()
        .name()
        .unwrap_or("test")
        .replace("::", "_");

    format!("{thread_name}_{pid}_{timestamp}")
}

/// Core function that handles all CLI execution variations
fn run_cli_core(
    args: &[&str],
    working_dir: Option<&Path>,
    config_path: Option<&Path>,
    use_temp_dir: bool,
) -> TestResult {
    let temp_dir_path = if use_temp_dir {
        Some(create_temp_dir())
    } else {
        None
    };

    let actual_working_dir = match (&temp_dir_path, working_dir) {
        (Some(temp), _) => Some(temp.as_path()),
        (None, Some(dir)) => Some(dir),
        (None, None) => None,
    };

    let result = if let Some(config) = config_path {
        let config_str = config.to_string_lossy();
        let args_with_config = insert_config_flag(args, &config_str);
        let args_refs: Vec<&str> = args_with_config.iter().map(|s| s.as_str()).collect();
        execute_cli(&args_refs, actual_working_dir)
    } else {
        execute_cli(args, actual_working_dir)
    };

    // Clean up temp directory if we created one
    if let Some(ref temp_dir) = temp_dir_path {
        std::fs::remove_dir_all(temp_dir).ok();
    }

    // Clean up docker volumes
    cleanup_docker_volumes();

    result
}

// Public API functions - maintained for compatibility

pub fn run_cli(args: &[&str]) -> TestResult {
    run_cli_core(args, None, None, false)
}

#[allow(dead_code)]
pub fn run_cli_in_dir(args: &[&str], dir: &Path) -> TestResult {
    run_cli_core(args, Some(dir), None, false)
}

pub fn run_cli_in_temp(args: &[&str]) -> TestResult {
    run_cli_core(args, None, None, true)
}

pub fn run_cli_in_temp_with_config(args: &[&str]) -> TestResult {
    let config_path = get_minimal_config_path();
    run_cli_core(args, None, Some(&config_path), true)
}

#[allow(dead_code)]
pub fn run_cli_in_temp_with_custom_config(args: &[&str], config_path: &Path) -> TestResult {
    run_cli_core(args, None, Some(config_path), true)
}

#[allow(dead_code)]
pub fn run_cli_in_temp_dir_with_config(args: &[&str], temp_dir: Option<&Path>) -> TestResult {
    let config_path = get_minimal_config_path();
    if let Some(dir) = temp_dir {
        run_cli_core(args, Some(dir), Some(&config_path), false)
    } else {
        run_cli_core(args, None, Some(&config_path), true)
    }
}

#[allow(dead_code)]
pub fn run_cli_in_temp_dir_with_custom_config(
    args: &[&str],
    temp_dir: Option<&Path>,
    config_path: &Path,
) -> TestResult {
    if let Some(dir) = temp_dir {
        run_cli_core(args, Some(dir), Some(config_path), false)
    } else {
        run_cli_core(args, None, Some(config_path), true)
    }
}

pub fn cli_with_config(
    args: &[&str],
    temp_dir: Option<&Path>,
    config: Option<&Path>,
) -> TestResult {
    if let Some(config_path) = config {
        let config_str = config_path.to_str().unwrap();
        let args_with_config = insert_config_flag(args, config_str);
        let args_ref: Vec<&str> = args_with_config.iter().map(|s| s.as_str()).collect();
        execute_cli(&args_ref, temp_dir)
    } else {
        execute_cli(args, temp_dir)
    }
}

// Assertion helpers

fn assert_command_succeeds(result: &TestResult) {
    if !result.success {
        eprintln!("Command failed with exit code {}", result.exit_code);
        eprintln!("Stdout: {}", result.stdout);
        eprintln!("Stderr: {}", result.stderr);
    }
    assert!(result.success);
}

fn assert_command_fails(result: &TestResult) {
    if result.success {
        eprintln!("Command unexpectedly succeeded");
        eprintln!("Stdout: {}", result.stdout);
        eprintln!("Stderr: {}", result.stderr);
    }
    assert!(!result.success);
}

fn assert_non_empty_output(result: &TestResult) {
    assert!(
        !result.stdout.is_empty() || !result.stderr.is_empty(),
        "Expected non-empty response text, but both stdout and stderr were empty"
    );
}

pub fn assert_command_completes(result: &TestResult) {
    assert!(result.exit_code >= 0);
}

pub fn assert_cmd(args: &[&str], temp_dir: Option<&Path>, config: Option<&Path>) {
    let result = cli_with_config(args, temp_dir, config);
    assert_command_succeeds(&result);
    assert_non_empty_output(&result);
}

pub fn refute_cmd(args: &[&str], temp_dir: Option<&Path>, config: Option<&Path>) {
    let result = cli_with_config(args, temp_dir, config);
    assert_command_fails(&result);
    assert_non_empty_output(&result);
}

pub fn assert_cmds(commands: &[&[&str]], temp_dir: Option<&Path>, config: Option<&Path>) {
    for args in commands {
        assert_cmd(args, temp_dir, config);
    }
}

#[allow(dead_code)]
pub fn refute_cmds(commands: &[&[&str]], temp_dir: Option<&Path>, config: Option<&Path>) {
    for args in commands {
        refute_cmd(args, temp_dir, config);
    }
}

// Utility functions

pub fn create_temp_dir() -> PathBuf {
    let temp_dir_name = generate_temp_dir_name();
    let temp_dir = std::env::temp_dir().join(&temp_dir_name);
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp directory");
    temp_dir
}

#[allow(dead_code)]
pub fn cleanup_temp_dir(temp_dir: &Path) {
    std::fs::remove_dir_all(temp_dir).ok();
}

/// Clean up docker volumes created during tests
fn cleanup_docker_volumes() {
    let output = Command::new("docker")
        .arg("volume")
        .arg("prune")
        .arg("--force")
        .output()
        .expect("Failed to execute docker volume prune");

    if !output.status.success() {
        eprintln!(
            "Failed to clean up docker volumes: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
