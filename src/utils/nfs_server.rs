//! NFS Server utilities using Ganesha for remote volume sharing.
//!
//! This module provides a shared NFS server implementation that can be used
//! by both the HITL server command and the runs-on remote execution feature.

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command as AsyncCommand};

use crate::utils::output::{print_info, OutputLevel};

/// Default port range for NFS server auto-selection
pub const DEFAULT_NFS_PORT_RANGE: RangeInclusive<u16> = 12050..=12099;

/// Default NFS port used by HITL server
pub const HITL_DEFAULT_PORT: u16 = 12049;

/// An NFS export configuration
#[derive(Debug, Clone)]
pub struct NfsExport {
    /// Unique export ID (1-based)
    pub export_id: u32,
    /// Local filesystem path to export
    pub local_path: PathBuf,
    /// NFS pseudo path (e.g., "/src", "/state")
    pub pseudo_path: String,
}

impl NfsExport {
    /// Create a new NFS export
    pub fn new(export_id: u32, local_path: PathBuf, pseudo_path: String) -> Self {
        Self {
            export_id,
            local_path,
            pseudo_path,
        }
    }

    /// Generate Ganesha EXPORT block for this export
    pub fn to_ganesha_config(&self) -> String {
        format!(
            r#"EXPORT {{
  Export_Id = {};
  Path = {};
  Pseudo = {};
  FSAL {{
    name = VFS;
  }}
}}
"#,
            self.export_id,
            self.local_path.display(),
            self.pseudo_path
        )
    }
}

/// Configuration for the NFS server
#[derive(Debug, Clone)]
pub struct NfsServerConfig {
    /// Port to listen on
    pub port: u16,
    /// List of exports
    pub exports: Vec<NfsExport>,
    /// Enable verbose logging
    pub verbose: bool,
    /// Bind address (default: 0.0.0.0)
    pub bind_addr: String,
}

impl Default for NfsServerConfig {
    fn default() -> Self {
        Self {
            port: *DEFAULT_NFS_PORT_RANGE.start(),
            exports: Vec::new(),
            verbose: false,
            bind_addr: "0.0.0.0".to_string(),
        }
    }
}

impl NfsServerConfig {
    /// Create a new NFS server config with the given port
    pub fn new(port: u16) -> Self {
        Self {
            port,
            ..Default::default()
        }
    }

    /// Set verbose mode
    #[allow(dead_code)]
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Add an export to the configuration
    pub fn add_export(&mut self, local_path: PathBuf, pseudo_path: String) -> &mut Self {
        let export_id = (self.exports.len() + 1) as u32;
        self.exports
            .push(NfsExport::new(export_id, local_path, pseudo_path));
        self
    }

    /// Generate the complete Ganesha configuration file content
    pub fn generate_ganesha_config(&self) -> String {
        let log_level = if self.verbose { "DEBUG" } else { "EVENT" };

        let mut config = format!(
            r#"# Auto-generated Ganesha NFS configuration
LOG {{
  Default_Log_Level = {log_level};
}}

NFS_Core_Param {{
  NFS_Port = {};
  Enable_NLM = false;
  Enable_RQUOTA = false;
  Enable_UDP = false;
  Protocols = 4;
  allow_set_io_flusher_fail = true;
  Nb_Max_Fd = 65536;
  Max_Open_Files = 10000;
  DRC_Max_Size = 32768;
  Attr_Expiration_Time = 60;
  Nb_Worker = 256;
  Bind_addr = {};
}}

NFSV4 {{
  Graceless = false;
  Allow_Numeric_Owners = true;
  Only_Numeric_Owners = true;
}}

# Defaults that all EXPORT{{}} blocks inherit unless they override
EXPORT_DEFAULTS {{
  Access_Type = RW;
  Squash = No_Root_Squash;
  Transports = TCP;
  Protocols = 4;
  SecType = none;
  Disable_ACL = true;
  Manage_Gids = false;
  Anonymous_uid = 0;
  Anonymous_gid = 0;

  CLIENT {{
    Clients = *;
    Access_Type = RW;
  }}
}}

"#,
            self.port, self.bind_addr
        );

        // Add export blocks
        for export in &self.exports {
            config.push_str(&export.to_ganesha_config());
            config.push('\n');
        }

        config
    }
}

/// Find an available port in the given range
///
/// Returns the first available port, or None if all ports are in use.
pub fn find_available_port(range: RangeInclusive<u16>) -> Option<u16> {
    range.into_iter().find(|&port| is_port_available(port))
}

/// Check if a port is available for binding
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// A running NFS server instance
pub struct NfsServer {
    /// The Ganesha child process (when running directly on host)
    process: Option<Child>,
    /// Container name (when running in a container)
    container_name: Option<String>,
    /// Container tool used (docker/podman)
    container_tool: Option<String>,
    /// Path to the config file (kept for potential future use)
    #[allow(dead_code)]
    config_path: PathBuf,
    /// Path to the PID file
    pid_path: PathBuf,
    /// Temporary directory holding config files
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    /// The port the server is running on
    #[allow(dead_code)]
    port: u16,
    /// Whether verbose mode is enabled
    verbose: bool,
}

impl NfsServer {
    /// Start a new NFS server with the given configuration
    ///
    /// This will:
    /// 1. Generate the Ganesha configuration file
    /// 2. Start ganesha.nfsd in foreground mode
    /// 3. Return the running server handle
    pub async fn start(config: NfsServerConfig) -> Result<Self> {
        // Verify ganesha.nfsd is available
        let ganesha_check = AsyncCommand::new("which")
            .arg("ganesha.nfsd")
            .output()
            .await;

        if ganesha_check.is_err() || !ganesha_check.unwrap().status.success() {
            anyhow::bail!(
                "ganesha.nfsd not found. Please ensure NFS-Ganesha is installed.\n\
                On Ubuntu/Debian: apt install nfs-ganesha nfs-ganesha-vfs\n\
                On Fedora/RHEL: dnf install nfs-ganesha nfs-ganesha-vfs"
            );
        }

        // Create temporary directory for config and PID files
        let temp_dir =
            tempfile::tempdir().context("Failed to create temp directory for NFS config")?;
        let config_path = temp_dir.path().join("ganesha.conf");
        let pid_path = temp_dir.path().join("ganesha.pid");

        // Generate and write configuration
        let config_content = config.generate_ganesha_config();
        std::fs::write(&config_path, &config_content).with_context(|| {
            format!(
                "Failed to write Ganesha config to {}",
                config_path.display()
            )
        })?;

        if config.verbose {
            print_info(
                &format!(
                    "Starting NFS server on port {} with {} exports",
                    config.port,
                    config.exports.len()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Config file: {}", config_path.display()),
                OutputLevel::Verbose,
            );
        }

        // Start ganesha.nfsd in foreground mode
        let mut cmd = AsyncCommand::new("ganesha.nfsd");
        cmd.arg("-f")
            .arg(&config_path)
            .arg("-p")
            .arg(&pid_path)
            .arg("-F") // Foreground mode
            .arg("-L")
            .arg("/dev/stderr") // Log to stderr
            .stdout(Stdio::null())
            .stderr(if config.verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            });

        let process = cmd.spawn().context("Failed to start ganesha.nfsd")?;

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        Ok(Self {
            process: Some(process),
            container_name: None,
            container_tool: None,
            config_path,
            pid_path,
            temp_dir,
            port: config.port,
            verbose: config.verbose,
        })
    }

    /// Start a new NFS server inside a container
    ///
    /// This runs ganesha.nfsd inside the SDK container, which has ganesha installed.
    /// The container mounts the paths to export and maps the NFS port.
    ///
    /// # Arguments
    /// * `config` - NFS server configuration
    /// * `container_tool` - Container tool to use (docker/podman)
    /// * `container_image` - SDK container image to use
    /// * `volume_mounts` - Additional volume mounts needed (for the docker volume)
    pub async fn start_in_container(
        config: NfsServerConfig,
        container_tool: &str,
        container_image: &str,
        volume_mounts: Vec<(String, String)>, // (host_path, container_path)
    ) -> Result<Self> {
        // Create temporary directory for config files
        let temp_dir =
            tempfile::tempdir().context("Failed to create temp directory for NFS config")?;
        let config_path = temp_dir.path().join("ganesha.conf");
        let pid_path = temp_dir.path().join("ganesha.pid");

        // Generate and write configuration
        let config_content = config.generate_ganesha_config();
        std::fs::write(&config_path, &config_content).with_context(|| {
            format!(
                "Failed to write Ganesha config to {}",
                config_path.display()
            )
        })?;

        // Generate unique container name
        let container_name = format!(
            "avocado-nfs-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("temp")
        );

        if config.verbose {
            print_info(
                &format!(
                    "Starting NFS server in container on port {} with {} exports",
                    config.port,
                    config.exports.len()
                ),
                OutputLevel::Normal,
            );
        }

        // Build container command
        let mut args: Vec<String> = vec![
            "run".to_string(),
            "--rm".to_string(),
            "-d".to_string(), // Detached mode
            "--name".to_string(),
            container_name.clone(),
            "--privileged".to_string(), // Required for NFS
            "--network".to_string(),
            "host".to_string(), // Use host networking for NFS port
        ];

        // Mount the config file
        args.push("-v".to_string());
        args.push(format!(
            "{}:/etc/ganesha/ganesha.conf:ro",
            config_path.display()
        ));

        // Mount the PID file location
        args.push("-v".to_string());
        args.push(format!("{}:/var/run/ganesha", temp_dir.path().display()));

        // Add volume mounts for exported paths
        for (host_path, container_path) in &volume_mounts {
            args.push("-v".to_string());
            args.push(format!("{}:{}", host_path, container_path));
        }

        // Also mount the exported paths from config
        for export in &config.exports {
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}",
                export.local_path.display(),
                export.local_path.display()
            ));
        }

        // Container image and command
        args.push(container_image.to_string());
        args.push("ganesha.nfsd".to_string());
        args.push("-f".to_string());
        args.push("/etc/ganesha/ganesha.conf".to_string());
        args.push("-F".to_string()); // Foreground mode
        args.push("-L".to_string());
        args.push("/dev/stderr".to_string());

        if config.verbose {
            print_info(
                &format!("Running: {} {}", container_tool, args.join(" ")),
                OutputLevel::Verbose,
            );
        }

        // Start the container
        let output = AsyncCommand::new(container_tool)
            .args(&args)
            .output()
            .await
            .context("Failed to start NFS server container")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to start NFS server container: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        // Verify container is running
        let check_output = AsyncCommand::new(container_tool)
            .args(["inspect", "-f", "{{.State.Running}}", &container_name])
            .output()
            .await?;

        if !check_output.status.success()
            || String::from_utf8_lossy(&check_output.stdout).trim() != "true"
        {
            // Get container logs for debugging
            let logs = AsyncCommand::new(container_tool)
                .args(["logs", &container_name])
                .output()
                .await
                .ok();

            let log_output = logs
                .map(|l| String::from_utf8_lossy(&l.stderr).to_string())
                .unwrap_or_default();

            anyhow::bail!(
                "NFS server container failed to start. Logs:\n{}",
                log_output
            );
        }

        if config.verbose {
            print_info(
                &format!(
                    "NFS server container '{}' started successfully",
                    container_name
                ),
                OutputLevel::Normal,
            );
        }

        Ok(Self {
            process: None,
            container_name: Some(container_name),
            container_tool: Some(container_tool.to_string()),
            config_path,
            pid_path,
            temp_dir,
            port: config.port,
            verbose: config.verbose,
        })
    }

    /// Get the port the server is running on
    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop the NFS server gracefully
    pub async fn stop(mut self) -> Result<()> {
        // Handle container-based NFS server
        if let (Some(container_name), Some(container_tool)) =
            (&self.container_name, &self.container_tool)
        {
            if self.verbose {
                print_info(
                    &format!("Stopping NFS server container '{}'...", container_name),
                    OutputLevel::Normal,
                );
            }

            // Stop the container
            let _ = AsyncCommand::new(container_tool)
                .args(["stop", "-t", "2", container_name])
                .output()
                .await;

            // Remove the container (should already be removed due to --rm, but just in case)
            let _ = AsyncCommand::new(container_tool)
                .args(["rm", "-f", container_name])
                .output()
                .await;

            if self.verbose {
                print_info("NFS server container stopped", OutputLevel::Normal);
            }
        }

        // Handle direct process-based NFS server
        if let Some(mut process) = self.process.take() {
            if self.verbose {
                print_info("Stopping NFS server...", OutputLevel::Normal);
            }

            // Try graceful shutdown first
            #[cfg(unix)]
            {
                if let Some(pid) = process.id() {
                    // Send SIGTERM
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
            }

            // Wait up to 2 seconds for graceful shutdown
            let timeout =
                tokio::time::timeout(tokio::time::Duration::from_secs(2), process.wait()).await;

            if timeout.is_err() {
                if self.verbose {
                    print_info("Force killing NFS server...", OutputLevel::Normal);
                }
                // Force kill if it didn't stop gracefully
                let _ = process.kill().await;
            }

            if self.verbose {
                print_info("NFS server stopped", OutputLevel::Normal);
            }
        }

        // Clean up PID file if it exists
        if self.pid_path.exists() {
            let _ = std::fs::remove_file(&self.pid_path);
        }

        Ok(())
    }
}

impl Drop for NfsServer {
    fn drop(&mut self) {
        // Try to kill the process if it's still running
        if let Some(ref mut process) = self.process {
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

/// Builder for creating NFS server configurations
pub struct NfsServerBuilder {
    config: NfsServerConfig,
}

impl NfsServerBuilder {
    /// Create a new builder with auto-selected port
    pub fn new() -> Result<Self> {
        let port = find_available_port(DEFAULT_NFS_PORT_RANGE)
            .context("No available ports in range 12050-12099 for NFS server")?;

        Ok(Self {
            config: NfsServerConfig::new(port),
        })
    }

    /// Create a new builder with a specific port
    pub fn with_port(port: u16) -> Result<Self> {
        if !is_port_available(port) {
            anyhow::bail!("Port {} is not available for NFS server", port);
        }

        Ok(Self {
            config: NfsServerConfig::new(port),
        })
    }

    /// Set verbose mode
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.config.verbose = verbose;
        self
    }

    /// Add an export
    pub fn add_export(
        mut self,
        local_path: impl AsRef<Path>,
        pseudo_path: impl Into<String>,
    ) -> Self {
        self.config
            .add_export(local_path.as_ref().to_path_buf(), pseudo_path.into());
        self
    }

    /// Build and return the configuration
    #[allow(dead_code)]
    pub fn build(self) -> NfsServerConfig {
        self.config
    }

    /// Build and start the NFS server
    pub async fn start(self) -> Result<NfsServer> {
        NfsServer::start(self.config).await
    }
}

impl Default for NfsServerBuilder {
    fn default() -> Self {
        Self::new().expect("Failed to find available port for NFS server")
    }
}

/// Get the mountpoint of a Docker volume on the host filesystem
///
/// This queries Docker for the volume's mountpoint, which is needed to export
/// the volume contents via NFS.
pub async fn get_docker_volume_mountpoint(
    container_tool: &str,
    volume_name: &str,
) -> Result<PathBuf> {
    let output = AsyncCommand::new(container_tool)
        .args([
            "volume",
            "inspect",
            volume_name,
            "--format",
            "{{.Mountpoint}}",
        ])
        .output()
        .await
        .with_context(|| format!("Failed to inspect Docker volume '{}'", volume_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to get mountpoint for volume '{}': {}",
            volume_name,
            stderr
        );
    }

    let mountpoint = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if mountpoint.is_empty() {
        anyhow::bail!("Docker volume '{}' has no mountpoint", volume_name);
    }

    Ok(PathBuf::from(mountpoint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nfs_export_config_generation() {
        let export = NfsExport::new(1, PathBuf::from("/home/user/project"), "/src".to_string());

        let config = export.to_ganesha_config();

        assert!(config.contains("Export_Id = 1"));
        assert!(config.contains("Path = /home/user/project"));
        assert!(config.contains("Pseudo = /src"));
        assert!(config.contains("FSAL {"));
        assert!(config.contains("name = VFS"));
    }

    #[test]
    fn test_nfs_server_config_generation() {
        let mut config = NfsServerConfig::new(12050);
        config.add_export(PathBuf::from("/home/user/src"), "/src".to_string());
        config.add_export(
            PathBuf::from("/var/lib/docker/volumes/avo-123/_data"),
            "/state".to_string(),
        );

        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("NFS_Port = 12050"));
        assert!(ganesha_config.contains("Export_Id = 1"));
        assert!(ganesha_config.contains("Export_Id = 2"));
        assert!(ganesha_config.contains("Pseudo = /src"));
        assert!(ganesha_config.contains("Pseudo = /state"));
        assert!(ganesha_config.contains("Protocols = 4"));
        assert!(ganesha_config.contains("Squash = No_Root_Squash"));
    }

    #[test]
    fn test_nfs_server_config_verbose_logging() {
        let config = NfsServerConfig::new(12050).with_verbose(true);
        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("Default_Log_Level = DEBUG"));
    }

    #[test]
    fn test_nfs_server_config_default_logging() {
        let config = NfsServerConfig::new(12050);
        let ganesha_config = config.generate_ganesha_config();

        assert!(ganesha_config.contains("Default_Log_Level = EVENT"));
    }

    #[test]
    fn test_find_available_port_in_range() {
        // This test may be flaky depending on what ports are in use
        // but it should generally find at least one available port
        let port = find_available_port(50000..=50010);
        assert!(port.is_some());
    }

    #[test]
    fn test_nfs_server_builder() {
        let config = NfsServerBuilder::with_port(50099)
            .expect("Port should be available")
            .verbose(true)
            .add_export("/tmp/test", "/test")
            .build();

        assert_eq!(config.port, 50099);
        assert!(config.verbose);
        assert_eq!(config.exports.len(), 1);
        assert_eq!(config.exports[0].pseudo_path, "/test");
    }
}
