use std::fs;
use tempfile::TempDir;

use avocado_cli::commands::runtime::RuntimeInstallCommand;

fn create_test_config_file(temp_dir: &TempDir, content: &str) -> String {
    let config_path = temp_dir.path().join("avocado.yaml");
    fs::write(&config_path, content).unwrap();
    config_path.to_string_lossy().to_string()
}

#[test]
fn test_new() {
    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        "avocado.yaml".to_string(),
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
    assert_eq!(cmd.config_path, "avocado.yaml");
    assert!(!cmd.verbose);
    assert!(!cmd.force);
    assert_eq!(cmd.target, Some("x86_64".to_string()));
}

#[test]
fn test_new_all_runtimes() {
    let cmd = RuntimeInstallCommand::new(
        None,
        "avocado.yaml".to_string(),
        true,
        true,
        None,
        Some(vec!["--arg1".to_string()]),
        Some(vec!["--dnf-arg".to_string()]),
    );

    assert_eq!(cmd.runtime, None);
    assert_eq!(cmd.config_path, "avocado.yaml");
    assert!(cmd.verbose);
    assert!(cmd.force);
    assert_eq!(cmd.target, None);
    assert_eq!(cmd.container_args, Some(vec!["--arg1".to_string()]));
    assert_eq!(cmd.dnf_args, Some(vec!["--dnf-arg".to_string()]));
}

#[tokio::test]
async fn test_execute_no_runtime_section() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // Should handle missing runtime section gracefully
    let result = cmd.execute().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_execute_runtime_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"

[runtime.other-runtime]
target = "x86_64"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // Should handle missing specific runtime gracefully
    let result = cmd.execute().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_execute_no_sdk_config() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "11.0"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // Should fail without SDK configuration
    let result = cmd.execute().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No SDK configuration found"));
}

#[tokio::test]
async fn test_execute_no_container_image() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
# Missing image field

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "11.0"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // Should fail without container image
    let result = cmd.execute().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No SDK container image specified"));
}

#[test]
fn test_runtime_install_with_package_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "11.0"
python3 = "*"
curl = { version = "7.0" }
app-ext = { ext = "my-extension" }

[ext.my-extension]
version = "2.0"
sysext = true
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // This would test the actual installation logic, but since we can't run containers in tests,
    // we'll just verify the command was created correctly
    assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
    assert_eq!(cmd.target, Some("x86_64".to_string()));
}

#[test]
fn test_runtime_install_all_runtimes() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"

[runtime.runtime1]
target = "x86_64"

[runtime.runtime1.dependencies]
gcc = "11.0"

[runtime.runtime2]
target = "aarch64"

[runtime.runtime2.dependencies]
python3 = "*"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        None, // Install for all runtimes
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // This would install dependencies for both runtime1 and runtime2
    assert_eq!(cmd.runtime, None);
}

#[test]
fn test_runtime_install_no_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"
# No dependencies section
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        false,
        false,
        Some("x86_64".to_string()),
        None,
        None,
    );

    // Should handle runtime with no dependencies gracefully
    assert_eq!(cmd.runtime, Some("test-runtime".to_string()));
}

#[test]
fn test_runtime_install_with_container_and_dnf_args() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
gcc = "*"
"#;
    let config_path = create_test_config_file(&temp_dir, config_content);

    let cmd = RuntimeInstallCommand::new(
        Some("test-runtime".to_string()),
        config_path,
        true,
        true,
        Some("x86_64".to_string()),
        Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
        Some(vec!["--nogpgcheck".to_string()]),
    );

    assert_eq!(cmd.container_args, Some(vec!["--cap-add=SYS_ADMIN".to_string()]));
    assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    assert!(cmd.verbose);
    assert!(cmd.force);
}
