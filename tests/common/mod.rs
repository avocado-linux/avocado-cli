use std::env;
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

/// Get the appropriate command for the CLI (Python or Rust)
fn get_cli_command() -> (String, Vec<String>) {
    // Check if we should use Rust CLI for certain commands
    if let Ok(use_rust) = env::var("USE_RUST_CLI") {
        if use_rust == "1" || use_rust.to_lowercase() == "true" {
            return (
                "cargo".to_string(),
                vec!["run".to_string(), "--".to_string()],
            );
        }
    }

    // Default to Python CLI
    let python_cmd = get_python_command();
    (python_cmd, vec!["-m".to_string(), "avocado".to_string()])
}

/// Get the appropriate Python command for the CLI
fn get_python_command() -> String {
    // Try to use the venv Python first, fall back to system python if not available
    let venv_python = std::env::current_dir()
        .ok()
        .map(|dir| dir.join(".venv").join("bin").join("python"))
        .filter(|path| path.exists());

    match venv_python {
        Some(path) => path.to_string_lossy().to_string(),
        None => {
            // Fall back to system python - check for python3 first, then python
            if Command::new("python3").arg("--version").output().is_ok() {
                "python3".to_string()
            } else {
                "python".to_string()
            }
        }
    }
}

/// Execute the CLI with the given arguments and configuration
fn execute_cli(args: &[&str], working_dir: Option<&Path>) -> TestResult {
    let (base_cmd, base_args) = get_cli_command();
    let mut cmd = Command::new(base_cmd);

    for arg in base_args {
        cmd.arg(arg);
    }
    cmd.args(args);

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
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
    new_args.push("-c".to_string());
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
        .join("minimal.toml")
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

    format!("{}_{}_{}", thread_name, pid, timestamp)
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
    run_cli_core(args, temp_dir, config, false)
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

pub fn cleanup_temp_dir(temp_dir: &Path) {
    std::fs::remove_dir_all(temp_dir).ok();
}
