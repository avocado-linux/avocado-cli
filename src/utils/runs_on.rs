//! RunsOn orchestration for remote execution workflow.
//!
//! This module provides the high-level orchestration for running avocado commands
//! on remote hosts while using NFS-backed volumes from the local machine.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::utils::container::is_docker_desktop;
use crate::utils::nfs_server::{
    find_available_port, get_docker_volume_mountpoint, is_port_available, NfsExport, NfsServer,
    NfsServerConfig, DEFAULT_NFS_PORT_RANGE,
};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::remote::{
    get_local_ip_for_remote, RemoteHost, RemoteVolumeManager, SshClient, SshControlMaster,
};

#[cfg(unix)]
use crate::utils::remote::SshTunnel;

/// Context for remote execution via `--runs-on`
///
/// This manages the lifecycle of:
/// - SSH ControlMaster for connection reuse
/// - NFS server on the local host
/// - NFS-backed Docker volumes on the remote host
/// - SSH tunnel for signing (if needed)
pub struct RunsOnContext {
    /// The remote host
    remote_host: RemoteHost,
    /// SSH ControlMaster for connection reuse
    ssh_master: Option<SshControlMaster>,
    /// SSH client for remote operations (uses ControlMaster if available)
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
    /// Remote path to the signing helper script
    #[cfg(unix)]
    remote_helper_script: Option<String>,
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
                "Remote execution mode: running on {}",
                remote_host.ssh_target()
            ),
            OutputLevel::Normal,
        );
        println!();

        // Start SSH ControlMaster for connection reuse
        // This creates a persistent SSH connection that all subsequent commands will share
        print_info(
            "Establishing persistent SSH connection...",
            OutputLevel::Normal,
        );
        let ssh_master = SshControlMaster::start(remote_host.clone(), verbose)
            .await
            .context("Failed to establish SSH ControlMaster connection")?;

        // Create SSH client that uses the ControlMaster
        let ssh = ssh_master.create_client();

        // Verify connectivity using the multiplexed connection
        print_info("Checking SSH connectivity...", OutputLevel::Normal);
        ssh.check_connectivity().await?;

        // Check remote CLI version compatibility
        print_info("Checking remote avocado version...", OutputLevel::Normal);
        let remote_version = ssh.check_cli_version().await?;
        print_success(
            &format!("Remote avocado version: {remote_version}"),
            OutputLevel::Normal,
        );

        // Determine which port to use
        let port = match nfs_port {
            Some(p) => {
                if !is_port_available(p) {
                    anyhow::bail!("Specified NFS port {p} is not available");
                }
                p
            }
            None => find_available_port(DEFAULT_NFS_PORT_RANGE)
                .context("No available ports in range 12050-12099 for NFS server")?,
        };

        if verbose {
            print_info(
                &format!("Using NFS port {port} for remote execution"),
                OutputLevel::Normal,
            );
        }

        // Get local IP that the remote can reach
        let local_ip = get_local_ip_for_remote(&remote_host.host)
            .await
            .context("Failed to determine local IP for NFS server")?;

        if verbose {
            print_info(
                &format!("Local IP for NFS: {local_ip}"),
                OutputLevel::Normal,
            );
        }

        // On Docker Desktop (macOS/Windows), the volume mountpoint returned by Docker
        // is inside the Docker Desktop VM and not accessible from the host filesystem.
        // We need to mount by volume name instead of host path.
        let (state_export_path, volume_mounts) = if is_docker_desktop() {
            if verbose {
                print_info(
                    "Docker Desktop detected: mounting volume by name",
                    OutputLevel::Normal,
                );
            }
            // Use a fixed container path for the state volume
            let container_state_path = PathBuf::from("/opt/nfs-state");
            let mounts = vec![
                (
                    src_dir.to_string_lossy().to_string(),
                    src_dir.to_string_lossy().to_string(),
                ),
                // Mount Docker volume by name to a container path
                (
                    local_volume_name.to_string(),
                    container_state_path.to_string_lossy().to_string(),
                ),
            ];
            (container_state_path, mounts)
        } else {
            // On native Docker (Linux), we can use the host volume mountpoint directly
            let volume_mountpoint = get_docker_volume_mountpoint(container_tool, local_volume_name)
                .await
                .with_context(|| {
                    format!(
                        "Failed to get mountpoint for volume '{local_volume_name}'"
                    )
                })?;

            if verbose {
                print_info(
                    &format!("Local volume mountpoint: {}", volume_mountpoint.display()),
                    OutputLevel::Normal,
                );
            }

            let mounts = vec![
                (
                    src_dir.to_string_lossy().to_string(),
                    src_dir.to_string_lossy().to_string(),
                ),
                (
                    volume_mountpoint.to_string_lossy().to_string(),
                    volume_mountpoint.to_string_lossy().to_string(),
                ),
            ];
            (volume_mountpoint, mounts)
        };

        // Create and start NFS server inside the SDK container
        // The container has ganesha.nfsd installed
        let config = NfsServerConfig {
            port,
            exports: vec![
                NfsExport::new(1, src_dir.to_path_buf(), "/src".to_string()),
                NfsExport::new(2, state_export_path.clone(), "/state".to_string()),
            ],
            verbose,
            bind_addr: "0.0.0.0".to_string(),
        };

        let nfs_server =
            NfsServer::start_in_container(config, container_tool, container_image, volume_mounts)
                .await
                .context("Failed to start NFS server")?;

        print_success(
            &format!("NFS server started on port {port}"),
            OutputLevel::Normal,
        );

        // Generate unique session ID for volume names
        let session_id = Uuid::new_v4().to_string()[..8].to_string();
        let src_volume_name = format!("avocado-src-{session_id}");
        let state_volume_name = format!("avocado-state-{session_id}");

        // Create NFS-backed volumes on remote (use ControlMaster client)
        print_info(
            "Creating NFS volumes on remote host...",
            OutputLevel::Normal,
        );
        let remote_vm =
            RemoteVolumeManager::new(ssh_master.create_client(), container_tool.to_string());

        // Create source volume
        remote_vm
            .create_nfs_volume(&src_volume_name, &local_ip.to_string(), port, "/src")
            .await
            .with_context(|| {
                format!(
                    "Failed to create NFS volume '{src_volume_name}' on remote"
                )
            })?;

        // Create state volume
        remote_vm
            .create_nfs_volume(&state_volume_name, &local_ip.to_string(), port, "/state")
            .await
            .with_context(|| {
                format!(
                    "Failed to create NFS volume '{state_volume_name}' on remote"
                )
            })?;

        print_success("Remote NFS volumes ready.", OutputLevel::Normal);
        println!();
        print_info(
            &format!("src_dir: {} -> remote:/opt/src", src_dir.display()),
            OutputLevel::Normal,
        );
        print_info(
            &format!(
                "_avocado: {} -> remote:/opt/_avocado",
                state_export_path.display()
            ),
            OutputLevel::Normal,
        );
        println!();

        Ok(Self {
            remote_host,
            ssh_master: Some(ssh_master),
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
            #[cfg(unix)]
            remote_helper_script: None,
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

    /// Get the CPU architecture of the remote host
    ///
    /// Returns the architecture string from `uname -m` (e.g., "x86_64", "aarch64").
    /// This is used to track SDK packages per host architecture in the lock file.
    pub async fn get_host_arch(&self) -> anyhow::Result<String> {
        self.ssh.get_architecture().await
    }

    /// Setup SSH tunnel for signing
    ///
    /// This creates an SSH tunnel that forwards signing requests from the remote
    /// back to the local signing service. It also creates the helper script on
    /// the remote host.
    #[cfg(unix)]
    pub async fn setup_signing_tunnel(&mut self, local_socket: &Path) -> Result<String> {
        use crate::utils::signing_service::generate_helper_script;

        let remote_socket = format!("/tmp/avocado-sign-{}.sock", self.session_id);
        let remote_helper_path = format!("/tmp/avocado-sign-request-{}", self.session_id);

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

        // Create the signing tunnel
        let tunnel = SshTunnel::create(&self.remote_host, local_socket, &remote_socket)
            .await
            .context("Failed to create SSH tunnel for signing")?;

        // Create the helper script on the remote host
        let helper_script = generate_helper_script();
        // Escape the script content for shell
        let escaped_script = helper_script.replace("'", "'\\''");
        let create_script_cmd = format!(
            "printf '%s' '{escaped_script}' > {remote_helper_path} && chmod +x {remote_helper_path}"
        );

        self.ssh
            .run_command(&create_script_cmd)
            .await
            .context("Failed to create signing helper script on remote")?;

        if self.verbose {
            print_info(
                &format!("Created signing helper script at {remote_helper_path}"),
                OutputLevel::Normal,
            );
        }

        let socket_path = tunnel.remote_socket().to_string();
        self.signing_tunnel = Some(tunnel);
        self.remote_helper_script = Some(remote_helper_path);

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
        let docker_cmd = self.build_docker_command(image, command, &env_vars, extra_docker_args)?;

        print_info(
            &format!("Executing on {}...", self.remote_host.ssh_target()),
            OutputLevel::Normal,
        );
        println!();

        if self.verbose {
            print_info(
                &format!("Running on remote: {docker_cmd}"),
                OutputLevel::Verbose,
            );
        }

        // Execute on remote
        self.ssh.run_command_interactive(&docker_cmd).await
    }

    /// Run a command on the remote host inside a container and capture output
    ///
    /// This is similar to `run_container_command` but captures stdout instead
    /// of inheriting it. Used for commands that need to return output (like rpm queries).
    ///
    /// # Arguments
    /// * `image` - Container image to use
    /// * `command` - Command to run inside the container
    /// * `env_vars` - Environment variables to set
    /// * `extra_docker_args` - Additional Docker arguments
    ///
    /// # Returns
    /// Some(output) if the command succeeded, None if it failed
    pub async fn run_container_command_with_output(
        &self,
        image: &str,
        command: &str,
        env_vars: HashMap<String, String>,
        extra_docker_args: &[String],
    ) -> Result<Option<String>> {
        let docker_cmd = self.build_docker_command(image, command, &env_vars, extra_docker_args)?;

        if self.verbose {
            print_info(
                &format!("Running on remote (capturing output): {docker_cmd}"),
                OutputLevel::Verbose,
            );
        }

        // Execute on remote and capture output
        match self.ssh.run_command(&docker_cmd).await {
            Ok(output) => Ok(Some(output)),
            Err(_) => Ok(None),
        }
    }

    /// Build the docker run command string
    fn build_docker_command(
        &self,
        image: &str,
        command: &str,
        env_vars: &HashMap<String, String>,
        extra_docker_args: &[String],
    ) -> Result<String> {
        let src_volume = self
            .remote_src_volume
            .as_ref()
            .context("Source volume not created")?;
        let state_volume = self
            .remote_state_volume
            .as_ref()
            .context("State volume not created")?;

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
        for (key, value) in env_vars {
            docker_cmd.push_str(&format!(" -e {}={}", key, shell_escape(value)));
        }

        // Add signing socket and helper script if tunnel is active
        #[cfg(unix)]
        if let Some(ref tunnel) = self.signing_tunnel {
            docker_cmd.push_str(&format!(
                " -v {}:{} -e AVOCADO_SIGNING_SOCKET={}",
                tunnel.remote_socket(),
                tunnel.remote_socket(),
                tunnel.remote_socket()
            ));

            // Mount the helper script if it exists
            if let Some(ref helper_path) = self.remote_helper_script {
                docker_cmd.push_str(&format!(
                    " -v {helper_path}:/usr/local/bin/avocado-sign-request:ro"
                ));
            }
        }

        // Add extra Docker arguments
        for arg in extra_docker_args {
            docker_cmd.push_str(&format!(" {arg}"));
        }

        // Add image and command
        docker_cmd.push_str(&format!(" {} bash -c {}", image, shell_escape(command)));

        Ok(docker_cmd)
    }

    /// Check if the context is still active (not yet torn down)
    pub fn is_active(&self) -> bool {
        self.nfs_server.is_some()
    }

    /// Get the remote host
    pub fn remote_host(&self) -> &RemoteHost {
        &self.remote_host
    }

    /// Clean up all resources
    ///
    /// This will:
    /// - Remove NFS-backed volumes from remote
    /// - Close SSH tunnel (if any)
    /// - Stop NFS server
    /// - Stop SSH ControlMaster
    ///
    /// After calling this method, the context should not be used for running commands.
    /// This method can be called multiple times safely (subsequent calls are no-ops).
    pub async fn teardown(&mut self) -> Result<()> {
        // If already torn down, return early
        if !self.is_active()
            && self.remote_src_volume.is_none()
            && self.remote_state_volume.is_none()
            && self.ssh_master.is_none()
        {
            return Ok(());
        }

        println!();
        print_info("Cleaning up remote resources...", OutputLevel::Normal);

        // Close signing tunnel first
        #[cfg(unix)]
        if let Some(tunnel) = self.signing_tunnel.take() {
            let _ = tunnel.close().await;
        }

        // Remove remote helper script
        #[cfg(unix)]
        if let Some(helper_path) = self.remote_helper_script.take() {
            let _ = self
                .ssh
                .run_command(&format!("rm -f {helper_path}"))
                .await;
        }

        // Remove remote volumes - use the existing SSH client which has ControlMaster
        // Create a new client from the master if available, otherwise use a plain one
        let cleanup_ssh = if let Some(ref master) = self.ssh_master {
            master.create_client()
        } else {
            SshClient::new(self.remote_host.clone()).with_verbose(self.verbose)
        };
        let remote_vm = RemoteVolumeManager::new(cleanup_ssh, self.container_tool.clone());

        let mut cleanup_errors = Vec::new();

        if let Some(volume) = self.remote_src_volume.take() {
            if self.verbose {
                print_info(
                    &format!("Removing remote volume: {volume}"),
                    OutputLevel::Normal,
                );
            }
            if let Err(e) = remote_vm.remove_volume(&volume).await {
                cleanup_errors.push(format!("Failed to remove {volume}: {e}"));
            }
        }

        if let Some(volume) = self.remote_state_volume.take() {
            if self.verbose {
                print_info(
                    &format!("Removing remote volume: {volume}"),
                    OutputLevel::Normal,
                );
            }
            if let Err(e) = remote_vm.remove_volume(&volume).await {
                cleanup_errors.push(format!("Failed to remove {volume}: {e}"));
            }
        }

        // Stop NFS server
        if self.verbose {
            print_info("Stopping NFS server...", OutputLevel::Normal);
        }
        if let Some(server) = self.nfs_server.take() {
            if let Err(e) = server.stop().await {
                cleanup_errors.push(format!("Failed to stop NFS server: {e}"));
            }
        }

        // Stop SSH ControlMaster (do this last since other cleanup uses it)
        if self.verbose {
            print_info("Closing SSH connection...", OutputLevel::Normal);
        }
        if let Some(mut master) = self.ssh_master.take() {
            if let Err(e) = master.stop().await {
                cleanup_errors.push(format!("Failed to stop SSH ControlMaster: {e}"));
            }
        }

        // Report any cleanup errors (but don't fail - cleanup is best-effort)
        if !cleanup_errors.is_empty() {
            for error in &cleanup_errors {
                print_info(&format!("Warning: {error}"), OutputLevel::Normal);
            }
        }

        print_success(
            &format!(
                "Remote resources cleaned up on {}.",
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
