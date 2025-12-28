//! Remote execution utilities for SSH-based command execution and volume management.
//!
//! This module provides utilities for running avocado commands on remote hosts
//! while using NFS-backed volumes from the local machine.

use anyhow::{Context, Result};
use std::net::IpAddr;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_info, OutputLevel};

/// Represents a remote host in user@host or just host format
#[derive(Debug, Clone)]
pub struct RemoteHost {
    /// Username for SSH connection (None means use current user)
    pub user: Option<String>,
    /// Hostname or IP address
    pub host: String,
}

impl RemoteHost {
    /// Parse a remote host specification in the format "user@host" or just "host"
    /// If no user is specified, SSH will use the current user.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();

        if spec.is_empty() {
            anyhow::bail!("Remote host specification cannot be empty");
        }

        if spec.contains('@') {
            let parts: Vec<&str> = spec.splitn(2, '@').collect();
            let user = parts[0].to_string();
            let host = parts[1].to_string();

            if user.is_empty() {
                anyhow::bail!("Username cannot be empty in '{}'", spec);
            }

            if host.is_empty() {
                anyhow::bail!("Hostname cannot be empty in '{}'", spec);
            }

            Ok(Self {
                user: Some(user),
                host,
            })
        } else {
            // No @ sign - just a hostname, SSH will infer the current user
            Ok(Self {
                user: None,
                host: spec.to_string(),
            })
        }
    }

    /// Get the SSH target string (user@host or just host)
    pub fn ssh_target(&self) -> String {
        match &self.user {
            Some(user) => format!("{}@{}", user, self.host),
            None => self.host.clone(),
        }
    }
}

/// SSH client for remote command execution
pub struct SshClient {
    remote: RemoteHost,
    verbose: bool,
}

impl SshClient {
    /// Create a new SSH client for the given remote host
    pub fn new(remote: RemoteHost) -> Self {
        Self {
            remote,
            verbose: false,
        }
    }

    /// Set verbose mode
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Check SSH connectivity to the remote host
    ///
    /// This runs a simple command to verify we can connect via SSH.
    pub async fn check_connectivity(&self) -> Result<()> {
        if self.verbose {
            print_info(
                &format!(
                    "Checking SSH connectivity to {}...",
                    self.remote.ssh_target()
                ),
                OutputLevel::Normal,
            );
        }

        let output = AsyncCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &self.remote.ssh_target(),
                "echo",
                "ok",
            ])
            .output()
            .await
            .context("Failed to execute SSH command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Cannot connect to '{}' via SSH. Ensure:\n\
                 1. SSH key-based authentication is configured\n\
                 2. The remote host is reachable\n\
                 3. The username is correct\n\
                 Error: {}",
                self.remote.ssh_target(),
                stderr.trim()
            );
        }

        if self.verbose {
            print_info(
                &format!("SSH connection to {} successful", self.remote.ssh_target()),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Check that the remote avocado CLI version is compatible
    ///
    /// The remote version must be equal to or greater than the local version.
    /// Returns the remote version string if compatible.
    ///
    /// For localhost/127.0.0.1, this check is skipped since it's the same machine.
    pub async fn check_cli_version(&self) -> Result<String> {
        let local_version = env!("CARGO_PKG_VERSION");

        // Skip version check for localhost - it's the same machine
        if self.remote.host == "localhost" || self.remote.host == "127.0.0.1" {
            if self.verbose {
                print_info(
                    "Skipping version check for localhost (same machine)",
                    OutputLevel::Normal,
                );
            }
            return Ok(local_version.to_string());
        }

        if self.verbose {
            print_info(
                &format!(
                    "Checking avocado CLI version on {}...",
                    self.remote.ssh_target()
                ),
                OutputLevel::Normal,
            );
        }

        // Try to get the remote avocado version
        let output = AsyncCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &self.remote.ssh_target(),
                "avocado --version 2>/dev/null || echo 'not-installed'",
            ])
            .output()
            .await
            .context("Failed to check remote avocado version")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to check avocado version on '{}': {}",
                self.remote.ssh_target(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let version_output = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if version_output == "not-installed" || version_output.is_empty() {
            anyhow::bail!(
                "avocado CLI is not installed on '{}'. Please install avocado {} or later.",
                self.remote.ssh_target(),
                local_version
            );
        }

        // Parse version from output like "avocado 0.20.0"
        let remote_version = version_output
            .split_whitespace()
            .last()
            .unwrap_or(&version_output);

        // Compare versions
        if !is_version_compatible(local_version, remote_version) {
            anyhow::bail!(
                "Remote avocado version '{}' is older than local version '{}'. \
                 Please upgrade avocado on '{}' to version {} or later.",
                remote_version,
                local_version,
                self.remote.ssh_target(),
                local_version
            );
        }

        if self.verbose {
            print_info(
                &format!(
                    "Remote avocado version: {} (local: {})",
                    remote_version, local_version
                ),
                OutputLevel::Normal,
            );
        }

        Ok(remote_version.to_string())
    }

    /// Run a command on the remote host and return the output
    pub async fn run_command(&self, command: &str) -> Result<String> {
        if self.verbose {
            print_info(
                &format!("Running remote command: {}", command),
                OutputLevel::Verbose,
            );
        }

        let output = AsyncCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &self.remote.ssh_target(),
                command,
            ])
            .output()
            .await
            .with_context(|| format!("Failed to run command on remote: {}", command))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Remote command failed: {}\nError: {}",
                command,
                stderr.trim()
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run a command on the remote host, inheriting stdout/stderr
    pub async fn run_command_interactive(&self, command: &str) -> Result<bool> {
        if self.verbose {
            print_info(
                &format!("Running remote command (interactive): {}", command),
                OutputLevel::Verbose,
            );
        }

        let status = AsyncCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-t", // Force pseudo-terminal allocation for interactive commands
                &self.remote.ssh_target(),
                command,
            ])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("Failed to run command on remote: {}", command))?;

        Ok(status.success())
    }

    /// Get the remote host reference
    #[allow(dead_code)]
    pub fn remote(&self) -> &RemoteHost {
        &self.remote
    }
}

/// Manager for creating and removing NFS-backed Docker volumes on remote hosts
pub struct RemoteVolumeManager {
    ssh: SshClient,
    container_tool: String,
}

impl RemoteVolumeManager {
    /// Create a new remote volume manager
    pub fn new(ssh: SshClient, container_tool: String) -> Self {
        Self {
            ssh,
            container_tool,
        }
    }

    /// Create an NFS-backed Docker volume on the remote host
    ///
    /// # Arguments
    /// * `volume_name` - Name for the new volume
    /// * `nfs_host` - NFS server hostname or IP
    /// * `nfs_port` - NFS server port
    /// * `export_path` - NFS pseudo path to mount (e.g., "/src", "/state")
    pub async fn create_nfs_volume(
        &self,
        volume_name: &str,
        nfs_host: &str,
        nfs_port: u16,
        export_path: &str,
    ) -> Result<()> {
        let command = format!(
            "{} volume create \
             --driver local \
             --opt type=nfs \
             --opt o=addr={},rw,nfsvers=4,port={} \
             --opt device=:{} \
             {}",
            self.container_tool, nfs_host, nfs_port, export_path, volume_name
        );

        self.ssh.run_command(&command).await?;

        if self.ssh.verbose {
            print_info(
                &format!("Created NFS volume '{}' on remote", volume_name),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Remove a Docker volume from the remote host
    pub async fn remove_volume(&self, volume_name: &str) -> Result<()> {
        let command = format!("{} volume rm -f {}", self.container_tool, volume_name);

        // Ignore errors - volume might not exist
        let _ = self.ssh.run_command(&command).await;

        if self.ssh.verbose {
            print_info(
                &format!("Removed volume '{}' from remote", volume_name),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Check if a volume exists on the remote host
    #[allow(dead_code)]
    pub async fn volume_exists(&self, volume_name: &str) -> Result<bool> {
        let command = format!(
            "{} volume inspect {} >/dev/null 2>&1 && echo 'exists' || echo 'not found'",
            self.container_tool, volume_name
        );

        let output = self.ssh.run_command(&command).await?;
        Ok(output.trim() == "exists")
    }

    /// Run a Docker container on the remote host with the given volume mappings
    ///
    /// # Arguments
    /// * `image` - Container image to run
    /// * `volumes` - Volume mappings (host_volume:container_path)
    /// * `env_vars` - Environment variables
    /// * `command` - Command to run in the container
    /// * `extra_args` - Additional Docker arguments
    #[allow(dead_code)]
    pub async fn run_container(
        &self,
        image: &str,
        volumes: &[(&str, &str)],
        env_vars: &[(&str, &str)],
        command: &str,
        extra_args: &[&str],
    ) -> Result<bool> {
        let mut docker_cmd = format!("{} run --rm", self.container_tool);

        // Add volume mappings
        for (host_vol, container_path) in volumes {
            docker_cmd.push_str(&format!(" -v {}:{}", host_vol, container_path));
        }

        // Add environment variables
        for (key, value) in env_vars {
            docker_cmd.push_str(&format!(" -e {}={}", key, value));
        }

        // Add extra arguments
        for arg in extra_args {
            docker_cmd.push_str(&format!(" {}", arg));
        }

        // Add image and command
        docker_cmd.push_str(&format!(
            " {} bash -c '{}'",
            image,
            command.replace('\'', "'\\''")
        ));

        self.ssh.run_command_interactive(&docker_cmd).await
    }
}

/// SSH tunnel for forwarding Unix sockets
#[cfg(unix)]
pub struct SshTunnel {
    /// The SSH process
    process: Option<tokio::process::Child>,
    /// Remote socket path
    remote_socket: String,
    /// Local socket path (stored for potential debugging/logging)
    #[allow(dead_code)]
    local_socket: std::path::PathBuf,
}

#[cfg(unix)]
impl SshTunnel {
    /// Create an SSH tunnel forwarding a Unix socket from remote to local
    ///
    /// This uses SSH's `-R` option to forward a remote Unix socket to a local one,
    /// allowing the remote process to communicate with a local service.
    pub async fn create(
        remote: &RemoteHost,
        local_socket: &Path,
        remote_socket: &str,
    ) -> Result<Self> {
        // Ensure the local socket exists
        if !local_socket.exists() {
            anyhow::bail!("Local socket does not exist: {}", local_socket.display());
        }

        // Start SSH with socket forwarding
        // -R remote_socket:local_socket forwards from remote to local
        let process = AsyncCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ExitOnForwardFailure=yes",
                "-N", // Don't execute a remote command
                "-R",
                &format!("{}:{}", remote_socket, local_socket.display()),
                &remote.ssh_target(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to create SSH tunnel")?;

        // Give it a moment to establish
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        Ok(Self {
            process: Some(process),
            remote_socket: remote_socket.to_string(),
            local_socket: local_socket.to_path_buf(),
        })
    }

    /// Get the remote socket path
    pub fn remote_socket(&self) -> &str {
        &self.remote_socket
    }

    /// Close the SSH tunnel
    pub async fn close(mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill().await;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            // Best effort kill
            #[cfg(unix)]
            {
                if let Some(pid) = process.id() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

/// Get the local machine's IP address that is reachable from the remote host
///
/// This tries to determine the local IP address that the remote host can use
/// to connect back to this machine (for NFS).
pub async fn get_local_ip_for_remote(remote_host: &str) -> Result<IpAddr> {
    // Try to resolve the remote host and get the local IP used to reach it
    // This is done by creating a UDP socket and "connecting" to the remote
    // (no actual connection is made for UDP, but the OS figures out which
    // local interface would be used)

    use std::net::UdpSocket;

    // First, try to resolve the remote host
    let remote_addrs: Vec<_> = tokio::net::lookup_host(format!("{}:22", remote_host))
        .await
        .with_context(|| format!("Failed to resolve remote host '{}'", remote_host))?
        .collect();

    if remote_addrs.is_empty() {
        anyhow::bail!("Could not resolve remote host '{}'", remote_host);
    }

    // Create a UDP socket and "connect" to the remote to determine local interface
    let socket = UdpSocket::bind("0.0.0.0:0").context("Failed to create UDP socket")?;

    socket
        .connect(remote_addrs[0])
        .context("Failed to determine route to remote host")?;

    let local_addr = socket.local_addr().context("Failed to get local address")?;

    Ok(local_addr.ip())
}

/// Check if a remote version is compatible with the local version
///
/// The remote version must be equal to or greater than the local version.
/// Uses semantic versioning comparison.
pub fn is_version_compatible(local_version: &str, remote_version: &str) -> bool {
    let parse_version = |v: &str| -> Option<(u32, u32, u32)> {
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() >= 3 {
            Some((
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts[2].split('-').next()?.parse().ok()?, // Handle pre-release like 0.20.0-beta
            ))
        } else if parts.len() == 2 {
            Some((parts[0].parse().ok()?, parts[1].parse().ok()?, 0))
        } else {
            None
        }
    };

    match (parse_version(local_version), parse_version(remote_version)) {
        (Some(local), Some(remote)) => {
            // Remote must be >= local
            remote >= local
        }
        _ => {
            // If we can't parse versions, assume compatible (fail open)
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_host_parse_valid() {
        let host = RemoteHost::parse("jschneck@riptide.local").unwrap();
        assert_eq!(host.user, Some("jschneck".to_string()));
        assert_eq!(host.host, "riptide.local");
        assert_eq!(host.ssh_target(), "jschneck@riptide.local");
    }

    #[test]
    fn test_remote_host_parse_ip() {
        let host = RemoteHost::parse("user@192.168.1.100").unwrap();
        assert_eq!(host.user, Some("user".to_string()));
        assert_eq!(host.host, "192.168.1.100");
    }

    #[test]
    fn test_remote_host_parse_hostname_only() {
        // SSH can infer the current user when no user is specified
        let host = RemoteHost::parse("hostname").unwrap();
        assert_eq!(host.user, None);
        assert_eq!(host.host, "hostname");
        assert_eq!(host.ssh_target(), "hostname");
    }

    #[test]
    fn test_remote_host_parse_localhost() {
        let host = RemoteHost::parse("localhost").unwrap();
        assert_eq!(host.user, None);
        assert_eq!(host.host, "localhost");
        assert_eq!(host.ssh_target(), "localhost");
    }

    #[test]
    fn test_remote_host_parse_invalid_empty_user() {
        let result = RemoteHost::parse("@hostname");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Username"));
    }

    #[test]
    fn test_remote_host_parse_invalid_empty_host() {
        let result = RemoteHost::parse("user@");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Hostname"));
    }

    #[test]
    fn test_version_compatible_equal() {
        assert!(is_version_compatible("0.20.0", "0.20.0"));
        assert!(is_version_compatible("1.0.0", "1.0.0"));
    }

    #[test]
    fn test_version_compatible_remote_newer() {
        assert!(is_version_compatible("0.20.0", "0.21.0"));
        assert!(is_version_compatible("0.20.0", "1.0.0"));
        assert!(is_version_compatible("0.20.0", "0.20.1"));
    }

    #[test]
    fn test_version_incompatible_remote_older() {
        assert!(!is_version_compatible("0.21.0", "0.20.0"));
        assert!(!is_version_compatible("1.0.0", "0.20.0"));
        assert!(!is_version_compatible("0.20.1", "0.20.0"));
    }

    #[test]
    fn test_version_compatible_major_minor_only() {
        assert!(is_version_compatible("0.20", "0.20.0"));
        assert!(is_version_compatible("0.20.0", "0.21"));
    }

    #[test]
    fn test_version_compatible_with_prerelease() {
        // Pre-release versions should still compare by numbers
        assert!(is_version_compatible("0.20.0-beta", "0.20.0"));
        assert!(is_version_compatible("0.20.0", "0.20.1-rc1"));
    }

    #[test]
    fn test_version_compatible_unparseable() {
        // Unparseable versions should fail open (assume compatible)
        assert!(is_version_compatible("unparseable", "0.20.0"));
        assert!(is_version_compatible("0.20.0", "unparseable"));
    }
}
