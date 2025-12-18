//! Signing service for handling binary signing requests from containers.
//!
//! This module provides a Unix domain socket service that listens for signing
//! requests from containers during provisioning operations. The service allows
//! container scripts to request binary signing without breaking execution flow.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

use crate::utils::output::{print_error, print_info, OutputLevel};

/// Maximum time to wait for a signing operation (30 seconds)
const SIGNING_TIMEOUT: Duration = Duration::from_secs(30);

/// Request from container to sign a binary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignRequest {
    /// Type identifier for the request
    #[serde(rename = "type")]
    pub request_type: String,
    /// Path to the binary inside the container volume
    pub binary_path: String,
    /// Checksum algorithm to use (sha256 or blake3)
    pub checksum_algorithm: String,
}

/// Response from host after signing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignResponse {
    /// Type identifier for the response
    #[serde(rename = "type")]
    pub response_type: String,
    /// Whether the signing was successful
    pub success: bool,
    /// Path to the signature file in the container
    pub signature_path: Option<String>,
    /// Content of the signature file (JSON)
    pub signature_content: Option<String>,
    /// Error message if signing failed
    pub error: Option<String>,
}

/// Configuration for the signing service
#[derive(Debug, Clone)]
pub struct SigningServiceConfig {
    /// Path to the Unix socket file on the host
    pub socket_path: PathBuf,
    /// Name of the runtime being provisioned
    pub runtime_name: String,
    /// Target architecture
    pub target_arch: String,
    /// Signing key name to use
    pub key_name: String,
    /// Signing key ID
    pub keyid: String,
    /// Volume name for reading/writing files
    pub volume_name: String,
    /// Enable verbose logging
    pub verbose: bool,
}

/// Handle for controlling the signing service
pub struct SigningService {
    /// Channel to send shutdown signal
    shutdown_tx: mpsc::Sender<()>,
    /// Task handle for the service
    task_handle: tokio::task::JoinHandle<Result<()>>,
    /// Temporary directory for socket and helper script (kept alive until service is dropped)
    _temp_dir: std::sync::Arc<tempfile::TempDir>,
}

impl SigningService {
    /// Start a new signing service
    pub async fn start(config: SigningServiceConfig, temp_dir: tempfile::TempDir) -> Result<Self> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Create the socket
        let socket_path = config.socket_path.clone();

        // Remove socket file if it exists from a previous run
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).with_context(|| {
                format!(
                    "Failed to remove existing socket at {}",
                    socket_path.display()
                )
            })?;
        }

        // Create parent directory if needed
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create socket directory at {}", parent.display())
            })?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind Unix socket at {}", socket_path.display()))?;

        // Set socket permissions to 0600 (owner only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&socket_path, perms)
                .context("Failed to set socket permissions")?;
        }

        if config.verbose {
            print_info(
                &format!("Signing service listening on {}", socket_path.display()),
                OutputLevel::Verbose,
            );
        }

        // Spawn the service task
        let config_clone = config.clone();
        let socket_path_clone = socket_path.clone();
        let task_handle = tokio::spawn(async move {
            let result = run_service(listener, config_clone, &mut shutdown_rx).await;

            // Clean up socket file
            let _ = std::fs::remove_file(&socket_path_clone);

            result
        });

        Ok(Self {
            shutdown_tx,
            task_handle,
            _temp_dir: std::sync::Arc::new(temp_dir),
        })
    }

    /// Shutdown the signing service
    pub async fn shutdown(self) -> Result<()> {
        // Send shutdown signal
        let _ = self.shutdown_tx.send(()).await;

        // Wait for the task to complete
        self.task_handle
            .await
            .context("Failed to join signing service task")?
            .context("Signing service encountered an error")
    }
}

/// Run the signing service loop
async fn run_service(
    listener: UnixListener,
    config: SigningServiceConfig,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> Result<()> {
    loop {
        tokio::select! {
            // Handle shutdown signal
            _ = shutdown_rx.recv() => {
                if config.verbose {
                    print_info("Signing service shutting down", OutputLevel::Verbose);
                }
                break;
            }

            // Accept new connections
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let config = config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, config).await {
                                print_error(
                                    &format!("Error handling signing request: {}", e),
                                    OutputLevel::Normal,
                                );
                            }
                        });
                    }
                    Err(e) => {
                        print_error(
                            &format!("Failed to accept connection: {}", e),
                            OutputLevel::Normal,
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle a single connection from a container
async fn handle_connection(stream: UnixStream, config: SigningServiceConfig) -> Result<()> {
    if config.verbose {
        print_info(
            "Received signing request from container",
            OutputLevel::Verbose,
        );
    }

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read the request with timeout
    let request: SignRequest = match timeout(SIGNING_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(_)) => serde_json::from_str(&line).context("Failed to parse signing request JSON")?,
        Ok(Err(e)) => {
            return Err(anyhow::anyhow!("Failed to read request: {}", e));
        }
        Err(_) => {
            return Err(anyhow::anyhow!("Timeout reading signing request"));
        }
    };

    if config.verbose {
        print_info(
            &format!("Processing signing request for: {}", request.binary_path),
            OutputLevel::Verbose,
        );
    }

    // Process the signing request
    let response = process_signing_request(request, &config).await;

    // Send response back to container
    let response_json =
        serde_json::to_string(&response).context("Failed to serialize signing response")?;

    writer
        .write_all(response_json.as_bytes())
        .await
        .context("Failed to write response")?;
    writer
        .write_all(b"\n")
        .await
        .context("Failed to write newline")?;
    writer.flush().await.context("Failed to flush response")?;

    if config.verbose {
        if response.success {
            print_info(
                "Signing request completed successfully",
                OutputLevel::Verbose,
            );
        } else {
            print_error(
                &format!(
                    "Signing request failed: {}",
                    response.error.unwrap_or_default()
                ),
                OutputLevel::Verbose,
            );
        }
    }

    Ok(())
}

/// Process a signing request and generate a response
async fn process_signing_request(
    request: SignRequest,
    config: &SigningServiceConfig,
) -> SignResponse {
    // Use the signing request handler to process the request
    let request_config = crate::utils::signing_request_handler::SigningRequestConfig {
        binary_path: &request.binary_path,
        checksum_algorithm: &request.checksum_algorithm,
        runtime_name: &config.runtime_name,
        target_arch: &config.target_arch,
        key_name: &config.key_name,
        keyid: &config.keyid,
        volume_name: &config.volume_name,
        verbose: config.verbose,
    };

    match crate::utils::signing_request_handler::handle_signing_request(request_config).await {
        Ok((sig_path, sig_content)) => SignResponse {
            response_type: "sign_response".to_string(),
            success: true,
            signature_path: Some(sig_path),
            signature_content: Some(sig_content),
            error: None,
        },
        Err(e) => SignResponse {
            response_type: "sign_response".to_string(),
            success: false,
            signature_path: None,
            signature_content: None,
            error: Some(format!("{:#}", e)),
        },
    }
}

/// Generate the helper script for containers to request signing
pub fn generate_helper_script() -> String {
    r#"#!/bin/bash
# avocado-sign-request - Request binary signing from host CLI
# This script is injected into containers during provisioning to enable
# inline binary signing without breaking script execution flow.

set -e

# Check if signing socket is available
if [ ! -S "/run/avocado/sign.sock" ]; then
    echo "Error: Signing socket not available" >&2
    exit 2  # Signing unavailable
fi

# Check arguments
if [ $# -ne 1 ]; then
    echo "Usage: avocado-sign-request <binary-path>" >&2
    exit 1
fi

BINARY_PATH="$1"

# Check if binary exists
if [ ! -f "$BINARY_PATH" ]; then
    echo "Error: Binary not found: $BINARY_PATH" >&2
    exit 3  # File not found
fi

# Get absolute path
BINARY_PATH=$(realpath "$BINARY_PATH")

# Determine checksum algorithm from environment or default to sha256
CHECKSUM_ALGO="${AVOCADO_SIGNING_CHECKSUM:-sha256}"

# Build JSON request
REQUEST=$(cat <<EOF
{"type":"sign_request","binary_path":"$BINARY_PATH","checksum_algorithm":"$CHECKSUM_ALGO"}
EOF
)

# Send request to signing service via Unix socket
# Using nc (netcat) or socat for socket communication
if command -v socat &> /dev/null; then
    RESPONSE=$(echo "$REQUEST" | socat - UNIX-CONNECT:/run/avocado/sign.sock 2>/dev/null)
elif command -v nc &> /dev/null; then
    RESPONSE=$(echo "$REQUEST" | nc -U /run/avocado/sign.sock 2>/dev/null)
else
    echo "Error: Neither socat nor nc available for socket communication" >&2
    exit 2
fi

# Check if response is empty
if [ -z "$RESPONSE" ]; then
    echo "Error: No response from signing service" >&2
    exit 1
fi

# Parse response and check success
SUCCESS=$(echo "$RESPONSE" | grep -o '"success":[^,}]*' | cut -d: -f2 | tr -d ' ')

if [ "$SUCCESS" = "true" ]; then
    echo "Successfully signed: $BINARY_PATH" >&2
    exit 0
else
    ERROR=$(echo "$RESPONSE" | grep -o '"error":"[^"]*"' | cut -d'"' -f4)
    echo "Error signing binary: $ERROR" >&2
    exit 1
fi
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_request_serialization() {
        let request = SignRequest {
            request_type: "sign_request".to_string(),
            binary_path: "/opt/_avocado/x86_64/runtimes/test/binary".to_string(),
            checksum_algorithm: "sha256".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("sign_request"));
        assert!(json.contains("/opt/_avocado/x86_64/runtimes/test/binary"));
        assert!(json.contains("sha256"));
    }

    #[test]
    fn test_sign_response_serialization() {
        let response = SignResponse {
            response_type: "sign_response".to_string(),
            success: true,
            signature_path: Some("/opt/_avocado/x86_64/runtimes/test/binary.sig".to_string()),
            signature_content: Some("{}".to_string()),
            error: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("sign_response"));
        assert!(json.contains("true"));
        assert!(json.contains(".sig"));
    }

    #[test]
    fn test_helper_script_generation() {
        let script = generate_helper_script();
        assert!(script.contains("#!/bin/bash"));
        assert!(script.contains("avocado-sign-request"));
        assert!(script.contains("/run/avocado/sign.sock"));
        assert!(script.contains("sign_request"));
    }
}
