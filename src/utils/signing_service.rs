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
    /// Path to the binary inside the container (for reference in signature file)
    pub binary_path: String,
    /// Hex-encoded hash computed by the container
    pub hash: String,
    /// File size in bytes
    pub size: u64,
    /// Checksum algorithm used (sha256 or blake3)
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
    /// The signature content (JSON format) - container writes this to .sig file
    pub signature: Option<String>,
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
    /// Signing key name to use
    pub key_name: String,
    /// Signing key ID
    pub keyid: String,
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

    // Process the signing request (synchronous - just signs the pre-computed hash)
    let response = process_signing_request(request, &config);

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
///
/// The container has already computed the hash - we just need to sign it.
/// This is fast since there's no file I/O involved.
fn process_signing_request(request: SignRequest, config: &SigningServiceConfig) -> SignResponse {
    match sign_hash_from_request(&request, config) {
        Ok(signature_content) => SignResponse {
            response_type: "sign_response".to_string(),
            success: true,
            signature: Some(signature_content),
            error: None,
        },
        Err(e) => SignResponse {
            response_type: "sign_response".to_string(),
            success: false,
            signature: None,
            error: Some(format!("{:#}", e)),
        },
    }
}

/// Sign a hash provided in the request
fn sign_hash_from_request(
    request: &SignRequest,
    config: &SigningServiceConfig,
) -> anyhow::Result<String> {
    use crate::utils::image_signing::{sign_hash_manifest, HashManifest, HashManifestEntry};

    // Create a manifest with the pre-computed hash from the container
    let manifest = HashManifest {
        runtime: config.runtime_name.clone(),
        checksum_algorithm: request.checksum_algorithm.clone(),
        files: vec![HashManifestEntry {
            container_path: request.binary_path.clone(),
            hash: request.hash.clone(),
            size: request.size,
        }],
    };

    // Sign the hash - this is fast, no file I/O needed
    let signatures = sign_hash_manifest(&manifest, &config.key_name, &config.keyid)
        .context("Failed to sign hash")?;

    if signatures.is_empty() {
        anyhow::bail!("No signature generated");
    }

    Ok(signatures[0].content.clone())
}

/// Generate the helper script for containers to request signing
pub fn generate_helper_script() -> String {
    r#"#!/bin/bash
# avocado-sign-request - Request binary signing from host CLI
# This script is injected into containers during provisioning to enable
# inline binary signing without breaking script execution flow.
#
# The script computes the hash locally in the container, sends only the hash
# to the host for signing, and writes the signature file locally.
# This avoids expensive file transfers between container and host.

set -e

# Configuration
MAX_RETRIES=3
RETRY_DELAY=1
# Timeout for waiting on response (signing is fast since we only send the hash)
SOCKET_TIMEOUT=30

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

# Get file size
FILE_SIZE=$(stat -c%s "$BINARY_PATH" 2>/dev/null || stat -f%z "$BINARY_PATH" 2>/dev/null)
if [ -z "$FILE_SIZE" ]; then
    echo "Error: Could not determine file size" >&2
    exit 1
fi

# Compute hash locally in the container
echo "Computing $CHECKSUM_ALGO hash of: $BINARY_PATH" >&2
case "$CHECKSUM_ALGO" in
    sha256)
        if command -v sha256sum &> /dev/null; then
            HASH=$(sha256sum "$BINARY_PATH" | cut -d' ' -f1)
        elif command -v shasum &> /dev/null; then
            HASH=$(shasum -a 256 "$BINARY_PATH" | cut -d' ' -f1)
        else
            echo "Error: No sha256 tool available (sha256sum or shasum)" >&2
            exit 2
        fi
        ;;
    blake3)
        if command -v b3sum &> /dev/null; then
            HASH=$(b3sum "$BINARY_PATH" | cut -d' ' -f1)
        else
            echo "Error: b3sum not available for blake3 hashing" >&2
            exit 2
        fi
        ;;
    *)
        echo "Error: Unsupported checksum algorithm: $CHECKSUM_ALGO" >&2
        exit 1
        ;;
esac

if [ -z "$HASH" ]; then
    echo "Error: Failed to compute hash" >&2
    exit 1
fi

# Build JSON request with the pre-computed hash
# Using printf to avoid issues with JSON escaping
REQUEST=$(printf '{"type":"sign_request","binary_path":"%s","hash":"%s","size":%s,"checksum_algorithm":"%s"}' \
    "$BINARY_PATH" "$HASH" "$FILE_SIZE" "$CHECKSUM_ALGO")

# Function to send request and get response
send_signing_request() {
    local response=""
    
    # Send request to signing service via Unix socket
    # The -t option for socat sets the timeout for half-close situations
    if command -v socat &> /dev/null; then
        response=$(echo "$REQUEST" | socat -t${SOCKET_TIMEOUT} -T${SOCKET_TIMEOUT} - UNIX-CONNECT:/run/avocado/sign.sock 2>/dev/null) || true
    elif command -v nc &> /dev/null; then
        # Try with -q option first (GNU netcat), fall back to -w only
        if nc -h 2>&1 | grep -q '\-q'; then
            response=$(echo "$REQUEST" | nc -w ${SOCKET_TIMEOUT} -q ${SOCKET_TIMEOUT} -U /run/avocado/sign.sock 2>/dev/null) || true
        else
            response=$(echo "$REQUEST" | nc -w ${SOCKET_TIMEOUT} -U /run/avocado/sign.sock 2>/dev/null) || true
        fi
    else
        echo "Error: Neither socat nor nc available for socket communication" >&2
        exit 2
    fi
    
    echo "$response"
}

# Retry loop with exponential backoff
RESPONSE=""
ATTEMPT=1
while [ $ATTEMPT -le $MAX_RETRIES ]; do
    if [ $ATTEMPT -gt 1 ]; then
        echo "Retry attempt $ATTEMPT of $MAX_RETRIES..." >&2
        sleep $RETRY_DELAY
        RETRY_DELAY=$((RETRY_DELAY * 2))
    fi
    
    RESPONSE=$(send_signing_request)
    
    # Check if we got a valid response
    if [ -n "$RESPONSE" ]; then
        if echo "$RESPONSE" | grep -q '"success"'; then
            break
        fi
    fi
    
    ATTEMPT=$((ATTEMPT + 1))
done

# Check if response is empty after all retries
if [ -z "$RESPONSE" ]; then
    echo "Error: No response from signing service after $MAX_RETRIES attempts" >&2
    exit 1
fi

# Parse response and check success
SUCCESS=$(echo "$RESPONSE" | grep -o '"success":[^,}]*' | cut -d: -f2 | tr -d ' ')

if [ "$SUCCESS" = "true" ]; then
    # Extract signature content from response and write to .sig file
    # The signature field contains the JSON signature content (escaped in the response)
    SIG_PATH="${BINARY_PATH}.sig"
    
    # Extract the signature JSON from the response using the best available tool
    SIGNATURE=""
    
    # Try jq first (most reliable for JSON parsing)
    if command -v jq &> /dev/null; then
        SIGNATURE=$(echo "$RESPONSE" | jq -r '.signature // empty' 2>/dev/null) || true
    fi
    
    # Fall back to python3 if jq didn't work or isn't available
    if [ -z "$SIGNATURE" ] && command -v python3 &> /dev/null; then
        SIGNATURE=$(echo "$RESPONSE" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    sig = data.get('signature', '')
    if sig:
        print(sig, end='')
except Exception as e:
    pass
" 2>/dev/null) || true
    fi
    
    # Last resort: try python (python2 on some systems)
    if [ -z "$SIGNATURE" ] && command -v python &> /dev/null; then
        SIGNATURE=$(echo "$RESPONSE" | python -c "
import sys, json
try:
    data = json.load(sys.stdin)
    sig = data.get('signature', '')
    if sig:
        sys.stdout.write(sig)
except:
    pass
" 2>/dev/null) || true
    fi
    
    if [ -z "$SIGNATURE" ]; then
        echo "Error: Could not extract signature from response. Need jq or python3." >&2
        echo "Response was: $RESPONSE" >&2
        exit 1
    fi
    
    # Write the signature file (use printf to avoid adding extra newline)
    printf '%s\n' "$SIGNATURE" > "$SIG_PATH"
    
    echo "Successfully signed: $BINARY_PATH" >&2
    exit 0
else
    # Extract error message
    ERROR=""
    if command -v jq &> /dev/null; then
        ERROR=$(echo "$RESPONSE" | jq -r '.error // empty' 2>/dev/null) || true
    fi
    if [ -z "$ERROR" ]; then
        ERROR=$(echo "$RESPONSE" | grep -o '"error":"[^"]*"' | cut -d'"' -f4)
    fi
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
            hash: "abcd1234".to_string(),
            size: 1024,
            checksum_algorithm: "sha256".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("sign_request"));
        assert!(json.contains("/opt/_avocado/x86_64/runtimes/test/binary"));
        assert!(json.contains("abcd1234"));
        assert!(json.contains("1024"));
        assert!(json.contains("sha256"));
    }

    #[test]
    fn test_sign_response_serialization() {
        let response = SignResponse {
            response_type: "sign_response".to_string(),
            success: true,
            signature: Some("{\"version\":\"1\"}".to_string()),
            error: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("sign_response"));
        assert!(json.contains("true"));
        assert!(json.contains("signature"));
    }

    #[test]
    fn test_helper_script_generation() {
        let script = generate_helper_script();
        assert!(script.contains("#!/bin/bash"));
        assert!(script.contains("avocado-sign-request"));
        assert!(script.contains("/run/avocado/sign.sock"));
        assert!(script.contains("sign_request"));
        // Verify retry logic is present
        assert!(script.contains("MAX_RETRIES"));
        assert!(script.contains("SOCKET_TIMEOUT"));
        // Verify proper socat/nc timeout options
        assert!(script.contains("-t${SOCKET_TIMEOUT}"));
        // Verify hash computation is done locally
        assert!(script.contains("sha256sum"));
        assert!(script.contains("Computing"));
        // Verify signature file is written locally
        assert!(script.contains("SIG_PATH"));
        // Verify jq is used for JSON parsing (most reliable)
        assert!(script.contains("jq -r"));
    }
}
