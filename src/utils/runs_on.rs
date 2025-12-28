//! RunsOn orchestration for remote execution workflow.
//!
//! This module provides the high-level orchestration for running avocado commands
//! on remote hosts while using NFS-backed volumes from the local machine.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

use crate::utils::nfs_server::{
    find_available_port, get_docker_volume_mountpoint, is_port_available, NfsExport, NfsServer,
    NfsServerConfig, DEFAULT_NFS_PORT_RANGE,
};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::remote::{get_local_ip_for_remote, RemoteHost, RemoteVolumeManager, SshClient};

#[cfg(unix)]
use crate::utils::remote::SshTunnel;

/// Context for remote execution via `--runs-on`
///
/// This manages the lifecycle of:
/// - NFS server on the local host
/// - NFS-backed Docker volumes on the remote host
/// - SSH tunnel for signing (if needed)
pub struct RunsOnContext {
    /// The remote host
    remote_host: RemoteHost,
    /// SSH client for remote operations
    ssh: SshClient,
    /// The running NFS server
    nfs_server: Option<NfsServer>,
    /// NFS port being used
    #[allow(dead_code)]
    nfs_port: u16,
    /// Local IP address reachable from remote
    #[allow(dead_code)]
    local_ip: String,
    /// Container tool (docker/podman)
    container_tool: String,
    /// Session UUID for unique volume names
    session_id: String,
    /// Remote volume for src_dir
    remote_src_volume: Option<String>,
    /// Remote volume for _avocado state
    remote_state_volume: Option<String>,
    /// SSH tunnel for signing
    #[cfg(unix)]
    signing_tunnel: Option<SshTunnel>,
    /// Enable verbose output
    verbose: bool,
}

impl RunsOnContext {
    /// Create and set up a new RunsOn context
    ///
    /// This will:
    /// 1. Validate SSH connectivity
    /// 2. Start NFS server with exports for src_dir and the avocado volume
    /// 3. Create NFS-backed Docker volumes on the remote host
    ///
    /// # Arguments
    /// * `runs_on` - Remote host specification (user@host)
    /// * `nfs_port` - Optional specific port (None = auto-select)
    /// * `src_dir` - Local source directory to export
    /// * `local_volume_name` - Local Docker volume name (e.g., "avo-{uuid}")
    /// * `container_tool` - Container tool to use (docker/podman)
    /// * `container_image` - SDK container image to use for NFS server
    /// * `verbose` - Enable verbose output
    pub async fn setup(
        runs_on: &str,
        nfs_port: Option<u16>,
        src_dir: &Path,
        local_volume_name: &str,
        container_tool: &str,
        container_image: &str,
        verbose: bool,
    ) -> Result<Self> {
        // Parse remote host
        let remote_host = RemoteHost::parse(runs_on)?;

        // Print banner to indicate runs-on mode
        println!();
        print_info(
            &format!(
                "🌐 Remote execution mode: running on {}",
                remote_host.ssh_target()
            ),
            OutputLevel::Normal,
        );
        println!();

        // Create SSH client and verify connectivity
        print_info("Checking SSH connectivity...", OutputLevel::Normal);
        let ssh = SshClient::new(remote_host.clone()).with_verbose(verbose);
        ssh.check_connectivity().await?;

        // Check remote CLI version compatibility
        print_info("Checking remote avocado version...", OutputLevel::Normal);
        let remote_version = ssh.check_cli_version().await?;
        print_success(
            &format!("Remote avocado version: {} ✓", remote_version),
            OutputLevel::Normal,
        );

        // Determine which port to use
        let port = match nfs_port {
            Some(p) => {
                if !is_port_available(p) {
                    anyhow::bail!("Specified NFS port {} is not available", p);
                }
                p
            }
            None => find_available_port(DEFAULT_NFS_PORT_RANGE)
                .context("No available ports in range 12050-12099 for NFS server")?,
        };

        if verbose {
            print_info(
                &format!("Using NFS port {} for remote execution", port),
                OutputLevel::Normal,
            );
        }

        // Get local IP that the remote can reach
        let local_ip = get_local_ip_for_remote(&remote_host.host)
            .await
            .context("Failed to determine local IP for NFS server")?;

        if verbose {
            print_info(
                &format!("Local IP for NFS: {}", local_ip),
                OutputLevel::Normal,
            );
        }

        // Get the mountpoint of the local Docker volume
        let volume_mountpoint = get_docker_volume_mountpoint(container_tool, local_volume_name)
            .await
            .with_context(|| {
                format!(
                    "Failed to get mountpoint for volume '{}'",
                    local_volume_name
                )
            })?;

        if verbose {
            print_info(
                &format!("Local volume mountpoint: {}", volume_mountpoint.display()),
                OutputLevel::Normal,
            );
        }

        // Create and start NFS server inside the SDK container
        // The container has ganesha.nfsd installed
        let config = NfsServerConfig {
            port,
            exports: vec![
                NfsExport::new(1, src_dir.to_path_buf(), "/src".to_string()),
                NfsExport::new(2, volume_mountpoint.clone(), "/state".to_string()),
            ],
            verbose,
            bind_addr: "0.0.0.0".to_string(),
        };

        // Volume mounts for the container to access the paths
        let volume_mounts = vec![
            (
                src_dir.to_string_lossy().to_string(),
                src_dir.to_string_lossy().to_string(),
            ),
            (
                volume_mountpoint.to_string_lossy().to_string(),
                volume_mountpoint.to_string_lossy().to_string(),
            ),
        ];

        let nfs_server =
            NfsServer::start_in_container(config, container_tool, container_image, volume_mounts)
                .await
                .context("Failed to start NFS server")?;

        print_success(
            &format!("NFS server started on port {}", port),
            OutputLevel::Normal,
        );

        // Generate unique session ID for volume names
        let session_id = Uuid::new_v4().to_string()[..8].to_string();
        let src_volume_name = format!("avocado-src-{}", session_id);
        let state_volume_name = format!("avocado-state-{}", session_id);

        // Create NFS-backed volumes on remote
        print_info(
            "Creating NFS volumes on remote host...",
            OutputLevel::Normal,
        );
        let remote_vm = RemoteVolumeManager::new(
            SshClient::new(remote_host.clone()).with_verbose(verbose),
            container_tool.to_string(),
        );

        // Create source volume
        remote_vm
            .create_nfs_volume(&src_volume_name, &local_ip.to_string(), port, "/src")
            .await
            .with_context(|| {
                format!(
                    "Failed to create NFS volume '{}' on remote",
                    src_volume_name
                )
            })?;

        // Create state volume
        remote_vm
            .create_nfs_volume(&state_volume_name, &local_ip.to_string(), port, "/state")
            .await
            .with_context(|| {
                format!(
                    "Failed to create NFS volume '{}' on remote",
                    state_volume_name
                )
            })?;

        print_success("Remote NFS volumes ready ✓", OutputLevel::Normal);
        println!();
        print_info(
            &format!("📂 src_dir: {} → remote:/opt/src", src_dir.display()),
            OutputLevel::Normal,
        );
        print_info(
            &format!(
                "📂 _avocado: {} → remote:/opt/_avocado",
                volume_mountpoint.display()
            ),
            OutputLevel::Normal,
        );
        println!();

        Ok(Self {
            remote_host,
            ssh,
            nfs_server: Some(nfs_server),
            nfs_port: port,
            local_ip: local_ip.to_string(),
            container_tool: container_tool.to_string(),
            session_id,
            remote_src_volume: Some(src_volume_name),
            remote_state_volume: Some(state_volume_name),
            #[cfg(unix)]
            signing_tunnel: None,
            verbose,
        })
    }

    /// Get the NFS port being used
    #[allow(dead_code)]
    pub fn nfs_port(&self) -> u16 {
        self.nfs_port
    }

    /// Get the session ID
    #[allow(dead_code)]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the remote source volume name
    #[allow(dead_code)]
    pub fn src_volume(&self) -> Option<&str> {
        self.remote_src_volume.as_deref()
    }

    /// Get the remote state volume name
    #[allow(dead_code)]
    pub fn state_volume(&self) -> Option<&str> {
        self.remote_state_volume.as_deref()
    }

    /// Setup SSH tunnel for signing
    ///
    /// This creates an SSH tunnel that forwards signing requests from the remote
    /// back to the local signing service.
    #[cfg(unix)]
    pub async fn setup_signing_tunnel(&mut self, local_socket: &Path) -> Result<String> {
        let remote_socket = format!("/tmp/avocado-sign-{}.sock", self.session_id);

        if self.verbose {
            print_info(
                &format!(
                    "Setting up signing tunnel: {} -> {}",
                    remote_socket,
                    local_socket.display()
                ),
                OutputLevel::Normal,
            );
        }

        let tunnel = SshTunnel::create(&self.remote_host, local_socket, &remote_socket)
            .await
            .context("Failed to create SSH tunnel for signing")?;

        let socket_path = tunnel.remote_socket().to_string();
        self.signing_tunnel = Some(tunnel);

        Ok(socket_path)
    }

    /// Signing tunnel stub for non-Unix platforms
    #[cfg(not(unix))]
    pub async fn setup_signing_tunnel(&mut self, _local_socket: &Path) -> Result<String> {
        anyhow::bail!("Signing tunnel is only supported on Unix platforms")
    }

    /// Run a command on the remote host inside a container
    ///
    /// This executes the given command in a container on the remote host,
    /// with the NFS volumes mounted appropriately.
    ///
    /// # Arguments
    /// * `image` - Container image to use
    /// * `command` - Command to run inside the container
    /// * `env_vars` - Environment variables to set
    /// * `extra_docker_args` - Additional Docker arguments
    pub async fn run_container_command(
        &self,
        image: &str,
        command: &str,
        env_vars: HashMap<String, String>,
        extra_docker_args: &[String],
    ) -> Result<bool> {
        let src_volume = self
            .remote_src_volume
            .as_ref()
            .context("Source volume not created")?;
        let state_volume = self
            .remote_state_volume
            .as_ref()
            .context("State volume not created")?;

        print_info(
            &format!("▶ Executing on {}...", self.remote_host.ssh_target()),
            OutputLevel::Normal,
        );
        println!();

        // Build the docker run command with --rm to ensure cleanup
        // Mount src volume to /mnt/src so bindfs can remap to /opt/src with UID translation
        // Mount state volume directly to /opt/_avocado (no UID mapping needed)
        let mut docker_cmd = format!(
            "{} run --rm \
             -v {}:/mnt/src:rw \
             -v {}:/opt/_avocado:rw \
             --device /dev/fuse \
             --cap-add SYS_ADMIN \
             --security-opt label=disable",
            self.container_tool, src_volume, state_volume
        );

        // Add environment variables
        for (key, value) in &env_vars {
            docker_cmd.push_str(&format!(" -e {}={}", key, shell_escape(value)));
        }

        // Add signing socket if tunnel is active
        #[cfg(unix)]
        if let Some(ref tunnel) = self.signing_tunnel {
            docker_cmd.push_str(&format!(
                " -v {}:{} -e AVOCADO_SIGNING_SOCKET={}",
                tunnel.remote_socket(),
                tunnel.remote_socket(),
                tunnel.remote_socket()
            ));
        }

        // Add extra Docker arguments
        for arg in extra_docker_args {
            docker_cmd.push_str(&format!(" {}", arg));
        }

        // Add image and command
        docker_cmd.push_str(&format!(" {} bash -c {}", image, shell_escape(command)));

        if self.verbose {
            print_info(
                &format!("Running on remote: {}", docker_cmd),
                OutputLevel::Verbose,
            );
        }

        // Execute on remote
        self.ssh.run_command_interactive(&docker_cmd).await
    }

    /// Clean up all resources
    ///
    /// This will:
    /// - Remove NFS-backed volumes from remote
    /// - Close SSH tunnel (if any)
    /// - Stop NFS server
    pub async fn teardown(mut self) -> Result<()> {
        println!();
        print_info("🧹 Cleaning up remote resources...", OutputLevel::Normal);

        // Close signing tunnel first
        #[cfg(unix)]
        if let Some(tunnel) = self.signing_tunnel.take() {
            let _ = tunnel.close().await;
        }

        // Remove remote volumes
        let remote_vm = RemoteVolumeManager::new(
            SshClient::new(self.remote_host.clone()).with_verbose(self.verbose),
            self.container_tool.clone(),
        );

        let mut cleanup_errors = Vec::new();

        if let Some(ref volume) = self.remote_src_volume {
            if self.verbose {
                print_info(
                    &format!("Removing remote volume: {}", volume),
                    OutputLevel::Normal,
                );
            }
            if let Err(e) = remote_vm.remove_volume(volume).await {
                cleanup_errors.push(format!("Failed to remove {}: {}", volume, e));
            }
        }

        if let Some(ref volume) = self.remote_state_volume {
            if self.verbose {
                print_info(
                    &format!("Removing remote volume: {}", volume),
                    OutputLevel::Normal,
                );
            }
            if let Err(e) = remote_vm.remove_volume(volume).await {
                cleanup_errors.push(format!("Failed to remove {}: {}", volume, e));
            }
        }

        // Stop NFS server
        if self.verbose {
            print_info("Stopping NFS server...", OutputLevel::Normal);
        }
        if let Some(server) = self.nfs_server.take() {
            if let Err(e) = server.stop().await {
                cleanup_errors.push(format!("Failed to stop NFS server: {}", e));
            }
        }

        // Report any cleanup errors (but don't fail - cleanup is best-effort)
        if !cleanup_errors.is_empty() {
            for error in &cleanup_errors {
                print_info(&format!("⚠ {}", error), OutputLevel::Normal);
            }
        }

        print_success(
            &format!(
                "🌐 Remote volumes cleaned up on {}",
                self.remote_host.ssh_target()
            ),
            OutputLevel::Normal,
        );
        println!();

        Ok(())
    }
}

/// Shell escape a string for safe use in a shell command
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_escape_complex() {
        assert_eq!(
            shell_escape("echo 'hello' && rm -rf /"),
            "'echo '\\''hello'\\'' && rm -rf /'"
        );
    }
}
