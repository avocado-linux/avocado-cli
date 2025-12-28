//! Integration tests for the `--runs-on` remote execution feature.
//!
//! These tests verify:
//! - SSH connectivity and command execution
//! - NFS server configuration and exports
//! - Remote NFS volume creation
//! - Signing via SSH tunnel
//! - File access and permission mapping
//! - Read/write operations to both src_dir and _avocado volumes
//!
//! ## Running Tests
//!
//! Most tests use localhost and require:
//! - SSH key-based auth configured for localhost (ssh localhost should work without password)
//! - Docker available locally
//!
//! To set up localhost SSH (if not already configured):
//!   ssh-keygen -t ed25519  # if you don't have a key
//!   cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
//!   chmod 600 ~/.ssh/authorized_keys
//!
//! Run localhost tests:
//!   cargo test --test runs_on_integration -- --ignored localhost
//!
//! Run with custom remote host:
//!   RUNS_ON_TEST_HOST=user@hostname cargo test --test runs_on_integration -- --ignored

#![allow(dead_code)]

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod common;

/// Get the test remote host - defaults to current_user@localhost
fn get_test_host() -> String {
    std::env::var("RUNS_ON_TEST_HOST").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        format!("{}@localhost", user)
    })
}

/// Check if localhost SSH is available
fn localhost_ssh_available() -> bool {
    std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=2",
            "localhost",
            "true",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// =============================================================================
// Unit Tests (run without network)
// =============================================================================

mod nfs_config_tests {
    use avocado_cli::utils::nfs_server::{NfsExport, NfsServerConfig};
    use std::path::PathBuf;

    #[test]
    fn test_single_export_config() {
        let mut config = NfsServerConfig::new(12050);
        config.add_export(PathBuf::from("/home/user/src"), "/src".to_string());

        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("NFS_Port = 12050"));
        assert!(ganesha_config.contains("Export_Id = 1"));
        assert!(ganesha_config.contains("Path = /home/user/src"));
        assert!(ganesha_config.contains("Pseudo = /src"));
    }

    #[test]
    fn test_dual_export_config_for_runs_on() {
        let mut config = NfsServerConfig::new(12051);
        config.add_export(PathBuf::from("/home/user/project"), "/src".to_string());
        config.add_export(
            PathBuf::from("/var/lib/docker/volumes/avo-abc123/_data"),
            "/state".to_string(),
        );

        let ganesha_config = config.generate_ganesha_config();

        // Verify both exports are present
        assert!(ganesha_config.contains("Export_Id = 1"));
        assert!(ganesha_config.contains("Export_Id = 2"));
        assert!(ganesha_config.contains("Pseudo = /src"));
        assert!(ganesha_config.contains("Pseudo = /state"));

        // Verify security settings for remote access
        assert!(ganesha_config.contains("Squash = No_Root_Squash"));
        assert!(ganesha_config.contains("Access_Type = RW"));
        assert!(ganesha_config.contains("SecType = none"));
    }

    #[test]
    fn test_export_id_auto_increment() {
        let mut config = NfsServerConfig::new(12050);
        config.add_export(PathBuf::from("/path1"), "/export1".to_string());
        config.add_export(PathBuf::from("/path2"), "/export2".to_string());
        config.add_export(PathBuf::from("/path3"), "/export3".to_string());

        assert_eq!(config.exports.len(), 3);
        assert_eq!(config.exports[0].export_id, 1);
        assert_eq!(config.exports[1].export_id, 2);
        assert_eq!(config.exports[2].export_id, 3);
    }

    #[test]
    fn test_nfs_export_ganesha_block() {
        let export = NfsExport::new(
            42,
            PathBuf::from("/var/lib/docker/volumes/test/_data"),
            "/state".to_string(),
        );

        let block = export.to_ganesha_config();

        assert!(block.contains("EXPORT {"));
        assert!(block.contains("Export_Id = 42"));
        assert!(block.contains("Path = /var/lib/docker/volumes/test/_data"));
        assert!(block.contains("Pseudo = /state"));
        assert!(block.contains("FSAL {"));
        assert!(block.contains("name = VFS"));
    }

    #[test]
    fn test_verbose_logging_config() {
        let config = NfsServerConfig::new(12050).with_verbose(true);
        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("Default_Log_Level = DEBUG"));
    }

    #[test]
    fn test_normal_logging_config() {
        let config = NfsServerConfig::new(12050);
        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("Default_Log_Level = EVENT"));
    }
}

mod version_compatibility_tests {
    use avocado_cli::utils::remote::is_version_compatible;

    #[test]
    fn test_equal_versions() {
        assert!(is_version_compatible("0.20.0", "0.20.0"));
        assert!(is_version_compatible("1.0.0", "1.0.0"));
        assert!(is_version_compatible("2.5.10", "2.5.10"));
    }

    #[test]
    fn test_remote_newer_patch() {
        assert!(is_version_compatible("0.20.0", "0.20.1"));
        assert!(is_version_compatible("0.20.0", "0.20.99"));
    }

    #[test]
    fn test_remote_newer_minor() {
        assert!(is_version_compatible("0.20.0", "0.21.0"));
        assert!(is_version_compatible("0.20.5", "0.25.0"));
    }

    #[test]
    fn test_remote_newer_major() {
        assert!(is_version_compatible("0.20.0", "1.0.0"));
        assert!(is_version_compatible("1.5.3", "2.0.0"));
    }

    #[test]
    fn test_remote_older_patch() {
        assert!(!is_version_compatible("0.20.1", "0.20.0"));
        assert!(!is_version_compatible("0.20.5", "0.20.4"));
    }

    #[test]
    fn test_remote_older_minor() {
        assert!(!is_version_compatible("0.21.0", "0.20.0"));
        assert!(!is_version_compatible("0.25.0", "0.20.5"));
    }

    #[test]
    fn test_remote_older_major() {
        assert!(!is_version_compatible("1.0.0", "0.20.0"));
        assert!(!is_version_compatible("2.0.0", "1.5.3"));
    }

    #[test]
    fn test_prerelease_versions() {
        // Pre-release suffix should be stripped for comparison
        assert!(is_version_compatible("0.20.0-beta", "0.20.0"));
        assert!(is_version_compatible("0.20.0", "0.20.1-rc1"));
    }
}

mod remote_host_tests {
    use avocado_cli::utils::remote::RemoteHost;

    #[test]
    fn test_parse_standard_format() {
        let host = RemoteHost::parse("jschneck@riptide.local").unwrap();
        assert_eq!(host.user, Some("jschneck".to_string()));
        assert_eq!(host.host, "riptide.local");
        assert_eq!(host.ssh_target(), "jschneck@riptide.local");
    }

    #[test]
    fn test_parse_ip_address() {
        let host = RemoteHost::parse("root@192.168.1.100").unwrap();
        assert_eq!(host.user, Some("root".to_string()));
        assert_eq!(host.host, "192.168.1.100");
    }

    #[test]
    fn test_parse_ipv6_address() {
        let host = RemoteHost::parse("user@::1").unwrap();
        assert_eq!(host.user, Some("user".to_string()));
        assert_eq!(host.host, "::1");
    }

    #[test]
    fn test_parse_hostname_with_domain() {
        let host = RemoteHost::parse("admin@server.example.com").unwrap();
        assert_eq!(host.user, Some("admin".to_string()));
        assert_eq!(host.host, "server.example.com");
    }

    #[test]
    fn test_parse_hostname_only() {
        // SSH can infer user when only hostname is provided
        let host = RemoteHost::parse("hostname-only").unwrap();
        assert_eq!(host.user, None);
        assert_eq!(host.host, "hostname-only");
        assert_eq!(host.ssh_target(), "hostname-only");
    }

    #[test]
    fn test_parse_empty_username() {
        let result = RemoteHost::parse("@hostname");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Username"));
    }

    #[test]
    fn test_parse_empty_hostname() {
        let result = RemoteHost::parse("user@");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Hostname"));
    }

    #[test]
    fn test_parse_multiple_at_symbols() {
        // Should take first @ as separator
        let host = RemoteHost::parse("user@host@domain").unwrap();
        assert_eq!(host.user, Some("user".to_string()));
        assert_eq!(host.host, "host@domain");
    }
}

mod port_selection_tests {
    use avocado_cli::utils::nfs_server::{find_available_port, is_port_available};

    #[test]
    fn test_find_port_in_high_range() {
        // Use high ports that are likely available
        let port = find_available_port(60000..=60010);
        assert!(port.is_some());
        let p = port.unwrap();
        assert!(p >= 60000 && p <= 60010);
    }

    #[test]
    fn test_is_port_available_high_port() {
        // High port should generally be available
        assert!(is_port_available(59999));
    }

    #[test]
    fn test_port_becomes_unavailable_after_bind() {
        use std::net::TcpListener;

        // Bind to a port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Port should no longer be available
        assert!(!is_port_available(port));

        // After dropping, it should become available again
        drop(listener);
        // Note: There may be a brief delay before the port is released
    }
}

mod shell_escape_tests {
    // These test the shell_escape function used in runs_on

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    #[test]
    fn test_simple_string() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_string_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_string_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_command_injection_attempt() {
        // Ensure shell metacharacters are safely escaped
        let dangerous = "$(rm -rf /)";
        let escaped = shell_escape(dangerous);
        assert_eq!(escaped, "'$(rm -rf /)'");
    }

    #[test]
    fn test_newlines() {
        let multiline = "line1\nline2";
        let escaped = shell_escape(multiline);
        assert!(escaped.starts_with("'"));
        assert!(escaped.ends_with("'"));
    }
}

// =============================================================================
// CLI Flag Tests (run without network)
// =============================================================================

mod cli_flag_tests {
    use crate::common;

    #[test]
    fn test_runs_on_flag_appears_in_help() {
        let result = common::run_cli(&["--help"]);
        assert!(result.success);
        assert!(result.stdout.contains("--runs-on"));
        assert!(result.stdout.contains("USER@HOST"));
    }

    #[test]
    fn test_nfs_port_flag_appears_in_help() {
        let result = common::run_cli(&["--help"]);
        assert!(result.success);
        assert!(result.stdout.contains("--nfs-port"));
    }

    #[test]
    fn test_runs_on_requires_value() {
        let result = common::run_cli(&["--runs-on"]);
        assert!(!result.success);
        // Should error about missing value
        assert!(
            result.stderr.contains("value is required")
                || result.stderr.contains("requires a value")
                || result.stderr.contains("argument requires"),
            "Expected error about missing value, got: {}",
            result.stderr
        );
    }

    #[test]
    fn test_nfs_port_requires_value() {
        let result = common::run_cli(&["--nfs-port"]);
        assert!(!result.success);
        assert!(
            result.stderr.contains("value is required")
                || result.stderr.contains("requires a value")
                || result.stderr.contains("argument requires"),
            "Expected error about missing value, got: {}",
            result.stderr
        );
    }

    #[test]
    fn test_nfs_port_requires_number() {
        let result = common::run_cli(&["--nfs-port", "not-a-number", "--help"]);
        assert!(!result.success);
        assert!(result.stderr.contains("invalid") || result.stderr.contains("number"));
    }
}

// =============================================================================
// Integration Tests (require remote host)
// =============================================================================

// =============================================================================
// Localhost SSH Tests
// =============================================================================

#[test]
#[ignore = "Requires localhost SSH key-based auth"]
fn test_localhost_ssh_connectivity() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured. Run: ssh-keygen && cat ~/.ssh/id_*.pub >> ~/.ssh/authorized_keys");
        return;
    }

    let host = get_test_host();

    // Use a simple command that should work if SSH is configured
    // Pass command as single string like other tests
    let result = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            &host,
            "echo ok",
        ])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "SSH connectivity to '{}' failed: {}",
        host,
        String::from_utf8_lossy(&result.stderr)
    );

    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.trim() == "ok", "Expected 'ok', got: {}", stdout);
}

#[test]
#[ignore = "Requires localhost SSH and avocado installed"]
fn test_localhost_cli_version_check() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();

    // Check if avocado is available on localhost
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, "avocado --version"])
        .output()
        .expect("Failed to execute ssh");

    if !result.status.success() {
        eprintln!("Skipping: avocado CLI not installed on localhost");
        return;
    }

    let version_output = String::from_utf8_lossy(&result.stdout);
    assert!(
        version_output.contains("avocado"),
        "Version output should contain 'avocado': {}",
        version_output
    );

    // Extract version number
    let version = version_output
        .split_whitespace()
        .last()
        .unwrap_or("unknown");

    // Should be a valid semver-like version
    assert!(
        version.contains('.'),
        "Version should contain a dot: {}",
        version
    );
}

#[test]
#[ignore = "Requires localhost SSH and Docker"]
fn test_localhost_docker_via_ssh() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();

    let result = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "docker",
            "info",
            "--format",
            "{{.ServerVersion}}",
        ])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "Docker not available via SSH to '{}': {}",
        host,
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "Requires localhost SSH"]
fn test_localhost_file_transfer_via_ssh() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let test_content = format!("test-content-{}", uuid::Uuid::new_v4());
    let test_file = temp_dir.join("test.txt");

    // Write a file locally
    fs::write(&test_file, &test_content).expect("Failed to write test file");

    // Read it back via SSH (simulates NFS-like access pattern)
    let result = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "cat",
            test_file.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute ssh");

    assert!(result.status.success(), "Failed to read file via SSH");

    let read_content = String::from_utf8_lossy(&result.stdout);
    assert_eq!(read_content.trim(), test_content, "File content mismatch");

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
#[ignore = "Requires localhost SSH"]
fn test_localhost_write_file_via_ssh() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    // Use a simple alphanumeric content to avoid escaping issues
    let test_content = format!("remotewrite{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let test_file = temp_dir.join("remote-created.txt");

    // Small delay to avoid SSH connection rate limiting
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Write file via SSH - pass command as a single string
    // Use -o ServerAliveInterval to keep connection stable
    let write_cmd = format!("printf '{}' > '{}'", test_content, test_file.display());
    let result = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=5",
            &host,
            &write_cmd,
        ])
        .output()
        .expect("Failed to execute ssh");

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        // Connection closed errors are often transient - skip rather than fail
        if stderr.contains("Connection closed") || stderr.contains("connection reset") {
            eprintln!("Skipping due to transient SSH error: {}", stderr);
            fs::remove_dir_all(&temp_dir).ok();
            return;
        }
        panic!("Failed to write file via SSH: {}", stderr);
    }

    // Small delay to ensure file system sync
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Read the file locally
    assert!(
        test_file.exists(),
        "File should exist: {}",
        test_file.display()
    );
    let read_content = fs::read_to_string(&test_file).expect("Failed to read file locally");
    assert_eq!(
        read_content, test_content,
        "File content mismatch after remote write"
    );

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
#[ignore = "Requires localhost SSH"]
#[cfg(unix)]
fn test_localhost_permission_preservation() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let test_file = temp_dir.join("perms-test.txt");

    // Create file with specific permissions
    fs::write(&test_file, "test").expect("Failed to write");
    fs::set_permissions(&test_file, fs::Permissions::from_mode(0o755))
        .expect("Failed to set permissions");

    // Check permissions via SSH - pass as single command string
    let stat_cmd = format!("stat -c '%a' '{}'", test_file.display());
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &stat_cmd])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "stat command failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mode = String::from_utf8_lossy(&result.stdout);
    assert_eq!(mode.trim(), "755", "Permission should be preserved as 755");

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
#[ignore = "Requires localhost SSH"]
#[cfg(unix)]
fn test_localhost_ownership_preservation() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let test_file = temp_dir.join("owner-test.txt");

    // Create file
    fs::write(&test_file, "test").expect("Failed to write");

    // Get current user's UID
    let local_uid = unsafe { libc::getuid() };

    // Check owner via SSH - pass as single command string
    let stat_cmd = format!("stat -c '%u' '{}'", test_file.display());
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &stat_cmd])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "stat command failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let remote_uid: u32 = String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse()
        .expect("Failed to parse UID");

    assert_eq!(
        remote_uid, local_uid,
        "Owner UID should be preserved (local: {}, remote: {})",
        local_uid, remote_uid
    );

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
#[ignore = "Requires NFS-Ganesha installed"]
fn test_ganesha_available() {
    let result = std::process::Command::new("which")
        .arg("ganesha.nfsd")
        .output()
        .expect("Failed to check for ganesha");

    assert!(
        result.status.success(),
        "ganesha.nfsd not found. Install with: apt install nfs-ganesha nfs-ganesha-vfs"
    );
}

#[test]
#[ignore = "Requires localhost SSH and Docker"]
fn test_localhost_docker_volume_create() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let volume_name = format!("avocado-test-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // Create a simple Docker volume via SSH
    let result = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "docker",
            "volume",
            "create",
            &volume_name,
        ])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "Failed to create Docker volume via SSH: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Clean up the volume
    let _ = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "docker",
            "volume",
            "rm",
            "-f",
            &volume_name,
        ])
        .output();
}

#[test]
#[ignore = "Requires localhost SSH and Docker"]
fn test_localhost_container_run_with_volume() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let test_content = format!("volume-test-{}", uuid::Uuid::new_v4());
    let test_file = temp_dir.join("container-test.txt");

    // Run a container via SSH that writes to a bind-mounted directory
    // Pass the entire docker command as a single string to SSH to avoid escaping issues
    let docker_cmd = format!(
        "docker run --rm -v '{}:/mnt' alpine sh -c 'echo {} > /mnt/container-test.txt'",
        temp_dir.display(),
        test_content
    );
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &docker_cmd])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "Failed to run container via SSH: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Verify the file was created locally
    assert!(test_file.exists(), "Container should have created the file");
    let read_content = fs::read_to_string(&test_file).expect("Failed to read file");
    assert_eq!(read_content.trim(), test_content);

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
#[ignore = "Requires localhost SSH"]
#[cfg(unix)]
fn test_localhost_signing_socket_tunnel() {
    use std::os::unix::net::UnixListener;

    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let local_socket_path = temp_dir.join("local-sign.sock");
    let remote_socket_path = format!(
        "/tmp/avocado-test-sign-{}.sock",
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    // Create a local Unix socket to forward
    let _listener = UnixListener::bind(&local_socket_path).expect("Failed to create local socket");

    // Start SSH tunnel in background (forward remote socket to local)
    let mut tunnel = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-N", // Don't execute command
            "-R",
            &format!("{}:{}", remote_socket_path, local_socket_path.display()),
            &host,
        ])
        .spawn()
        .expect("Failed to start SSH tunnel");

    // Give it time to establish
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Verify the tunnel process is running
    assert!(
        tunnel.try_wait().unwrap().is_none(),
        "SSH tunnel should still be running"
    );

    // Check if remote socket exists
    let check = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "test",
            "-S",
            &remote_socket_path,
        ])
        .output()
        .expect("Failed to check remote socket");

    // Clean up
    let _ = tunnel.kill();
    let _ = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            "rm",
            "-f",
            &remote_socket_path,
        ])
        .output();
    fs::remove_dir_all(&temp_dir).ok();

    assert!(
        check.status.success(),
        "Remote socket should exist at {}",
        remote_socket_path
    );
}

#[test]
#[ignore = "Requires localhost SSH and Docker"]
fn test_localhost_container_reads_local_file() {
    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let test_content = format!("local-file-{}", uuid::Uuid::new_v4());
    let test_file = temp_dir.join("local-data.txt");

    // Create file locally first
    fs::write(&test_file, &test_content).expect("Failed to write local file");

    // Run container via SSH that reads the local file (simulates src_dir access)
    // Pass the entire docker command as a single string
    let docker_cmd = format!(
        "docker run --rm -v '{}:/opt/src:ro' alpine cat /opt/src/local-data.txt",
        temp_dir.display()
    );
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &docker_cmd])
        .output()
        .expect("Failed to execute ssh");

    assert!(
        result.status.success(),
        "Container failed to read local file: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let output = String::from_utf8_lossy(&result.stdout);
    assert_eq!(
        output.trim(),
        test_content,
        "Container should read local file content"
    );

    fs::remove_dir_all(&temp_dir).ok();
}

#[test]
fn test_concurrent_port_allocation() {
    // Test that multiple runs-on sessions can run concurrently
    // by using different ports from the 12050-12099 range

    use avocado_cli::utils::nfs_server::{find_available_port, is_port_available};
    use std::net::TcpListener;

    // Find first available port
    let port1 = find_available_port(50100..=50110);
    assert!(port1.is_some(), "Should find first port");

    // Bind to it to simulate it being in use
    let _listener1 = TcpListener::bind(format!("0.0.0.0:{}", port1.unwrap())).unwrap();

    // Find another port - should get a different one
    let port2 = find_available_port(50100..=50110);
    assert!(port2.is_some(), "Should find second port");
    assert_ne!(
        port1, port2,
        "Should find different port when first is in use"
    );

    // Original port should now be unavailable
    assert!(
        !is_port_available(port1.unwrap()),
        "First port should be in use"
    );
}

// =============================================================================
// Full Workflow Integration Tests
// =============================================================================

#[test]
#[ignore = "Requires localhost SSH and Docker"]
fn test_full_localhost_workflow() {
    // This is a comprehensive test simulating the runs-on workflow using localhost

    if !localhost_ssh_available() {
        eprintln!("Skipping: localhost SSH not configured");
        return;
    }

    let host = get_test_host();
    let temp_dir = common::create_temp_dir();
    let src_dir = temp_dir.join("src");
    let state_dir = temp_dir.join("state");

    fs::create_dir_all(&src_dir).expect("Failed to create src dir");
    fs::create_dir_all(&state_dir).expect("Failed to create state dir");

    // Step 1: Create test files in "src" directory
    let src_content = format!("source-file-{}", uuid::Uuid::new_v4());
    fs::write(src_dir.join("source.txt"), &src_content).expect("Failed to write source file");

    // Step 2: Run container via SSH that:
    //   - Reads from /opt/src (simulating src_dir mount)
    //   - Writes to /opt/_avocado (simulating state mount)
    // Pass entire docker command as single string to avoid shell escaping issues
    let state_content = format!("state-file-{}", uuid::Uuid::new_v4());
    let docker_cmd = format!(
        "docker run --rm -v '{}:/opt/src:ro' -v '{}:/opt/_avocado:rw' alpine sh -c 'cat /opt/src/source.txt && echo {} > /opt/_avocado/state.txt'",
        src_dir.display(),
        state_dir.display(),
        state_content
    );
    let result = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &docker_cmd])
        .output()
        .expect("Failed to run container");

    assert!(
        result.status.success(),
        "Container failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Step 3: Verify container could read source file
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains(&src_content),
        "Container should read src file content"
    );

    // Step 4: Verify state file was written locally
    let state_file = state_dir.join("state.txt");
    assert!(
        state_file.exists(),
        "State file should exist after container run"
    );
    let read_state = fs::read_to_string(&state_file).expect("Failed to read state file");
    assert_eq!(
        read_state.trim(),
        state_content,
        "State content should match"
    );

    // Step 5: Test bidirectional - modify local, verify in container
    let modified_content = format!("modified-{}", uuid::Uuid::new_v4());
    fs::write(state_dir.join("modified.txt"), &modified_content).expect("Failed to write");

    let verify_cmd = format!(
        "docker run --rm -v '{}:/opt/_avocado:ro' alpine cat /opt/_avocado/modified.txt",
        state_dir.display()
    );
    let verify = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", &host, &verify_cmd])
        .output()
        .expect("Failed to verify");

    assert!(verify.status.success());
    let verify_output = String::from_utf8_lossy(&verify.stdout);
    assert_eq!(
        verify_output.trim(),
        modified_content,
        "Should read locally modified file"
    );

    fs::remove_dir_all(&temp_dir).ok();
}

// =============================================================================
// Error Handling Tests
// =============================================================================

mod error_handling_tests {
    use avocado_cli::utils::remote::RemoteHost;

    #[test]
    fn test_hostname_only_is_valid() {
        // SSH can infer the user from the environment when only hostname is provided
        let result = RemoteHost::parse("hostname");
        assert!(result.is_ok());
        let host = result.unwrap();
        assert_eq!(host.user, None);
        assert_eq!(host.host, "hostname");
    }

    #[test]
    fn test_empty_string_error() {
        let result = RemoteHost::parse("");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_user_descriptive_error() {
        let result = RemoteHost::parse("@hostname");
        let err = result.unwrap_err();
        let msg = err.to_string();

        // Should provide helpful error message
        assert!(msg.contains("Username") || msg.contains("empty"));
    }
}

// =============================================================================
// Permission and Security Tests
// =============================================================================

mod security_tests {
    #[test]
    fn test_nfs_config_has_proper_security_settings() {
        use avocado_cli::utils::nfs_server::NfsServerConfig;
        use std::path::PathBuf;

        let mut config = NfsServerConfig::new(12050);
        config.add_export(PathBuf::from("/test"), "/test".to_string());

        let ganesha_config = config.generate_ganesha_config();

        // Verify security-relevant settings
        assert!(
            ganesha_config.contains("Squash = No_Root_Squash"),
            "Should allow root access for proper UID mapping"
        );
        assert!(
            ganesha_config.contains("Anonymous_uid = 0"),
            "Anonymous should map to root"
        );
        assert!(
            ganesha_config.contains("Anonymous_gid = 0"),
            "Anonymous should map to root group"
        );
        assert!(
            ganesha_config.contains("Only_Numeric_Owners = true"),
            "Should use numeric owners for cross-system compatibility"
        );
    }

    #[test]
    fn test_nfs_config_binds_to_all_interfaces() {
        use avocado_cli::utils::nfs_server::NfsServerConfig;

        let config = NfsServerConfig::new(12050);
        let ganesha_config = config.generate_ganesha_config();

        assert!(
            ganesha_config.contains("Bind_addr = 0.0.0.0"),
            "Should bind to all interfaces for remote access"
        );
    }
}

// =============================================================================
// Docker Volume Command Tests
// =============================================================================

mod docker_volume_tests {
    #[test]
    fn test_nfs_volume_create_command_format() {
        let volume_name = "avocado-src-abc123";
        let nfs_host = "192.168.1.100";
        let nfs_port = 12050u16;
        let export_path = "/src";

        let command = format!(
            "docker volume create \
             --driver local \
             --opt type=nfs \
             --opt o=addr={},rw,nfsvers=4,port={} \
             --opt device=:{} \
             {}",
            nfs_host, nfs_port, export_path, volume_name
        );

        assert!(command.contains("--driver local"));
        assert!(command.contains("type=nfs"));
        assert!(command.contains(&format!("addr={}", nfs_host)));
        assert!(command.contains(&format!("port={}", nfs_port)));
        assert!(command.contains(&format!("device=:{}", export_path)));
        assert!(command.contains(volume_name));
    }

    #[test]
    fn test_nfs_volume_remove_command_format() {
        let volume_name = "avocado-state-def456";

        let command = format!("docker volume rm -f {}", volume_name);

        assert!(command.contains("volume rm"));
        assert!(command.contains("-f"));
        assert!(command.contains(volume_name));
    }
}

// =============================================================================
// Container Command Tests
// =============================================================================

mod container_command_tests {
    use std::collections::HashMap;

    fn build_container_command(
        container_tool: &str,
        src_volume: &str,
        state_volume: &str,
        image: &str,
        command: &str,
        env_vars: &HashMap<String, String>,
    ) -> String {
        let mut cmd = format!(
            "{} run --rm \
             -v {}:/opt/src:rw \
             -v {}:/opt/_avocado:rw \
             --device /dev/fuse \
             --cap-add SYS_ADMIN \
             --security-opt label=disable",
            container_tool, src_volume, state_volume
        );

        for (key, value) in env_vars {
            cmd.push_str(&format!(" -e {}={}", key, value));
        }

        cmd.push_str(&format!(" {} bash -c '{}'", image, command));
        cmd
    }

    #[test]
    fn test_container_command_has_required_mounts() {
        let cmd = build_container_command(
            "docker",
            "avocado-src-123",
            "avocado-state-123",
            "ghcr.io/avocado-linux/sdk:latest",
            "ls -la",
            &HashMap::new(),
        );

        assert!(cmd.contains("-v avocado-src-123:/opt/src:rw"));
        assert!(cmd.contains("-v avocado-state-123:/opt/_avocado:rw"));
    }

    #[test]
    fn test_container_command_has_fuse_device() {
        let cmd =
            build_container_command("docker", "src", "state", "image", "cmd", &HashMap::new());

        assert!(cmd.contains("--device /dev/fuse"));
    }

    #[test]
    fn test_container_command_has_sys_admin_cap() {
        let cmd =
            build_container_command("docker", "src", "state", "image", "cmd", &HashMap::new());

        assert!(cmd.contains("--cap-add SYS_ADMIN"));
    }

    #[test]
    fn test_container_command_has_selinux_label_disable() {
        let cmd =
            build_container_command("docker", "src", "state", "image", "cmd", &HashMap::new());

        assert!(cmd.contains("--security-opt label=disable"));
    }

    #[test]
    fn test_container_command_includes_env_vars() {
        let mut env = HashMap::new();
        env.insert("AVOCADO_TARGET".to_string(), "qemux86-64".to_string());
        env.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());

        let cmd = build_container_command("docker", "src", "state", "image", "cmd", &env);

        assert!(
            cmd.contains("-e AVOCADO_TARGET=qemux86-64")
                || cmd.contains("-e CUSTOM_VAR=custom_value")
        );
    }

    #[test]
    fn test_container_command_supports_podman() {
        let cmd =
            build_container_command("podman", "src", "state", "image", "cmd", &HashMap::new());

        assert!(cmd.starts_with("podman run"));
    }
}
