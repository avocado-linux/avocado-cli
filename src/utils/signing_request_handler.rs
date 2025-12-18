//! Handler for processing signing requests from containers.
//!
//! This module implements the logic for processing signing requests,
//! including extracting binaries from volumes, computing hashes,
//! signing them, and writing signatures back to the volume.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::process::Command;

use crate::utils::image_signing::{
    compute_file_hash, sign_hash_manifest, ChecksumAlgorithm, HashManifest, HashManifestEntry,
};
use crate::utils::output::{print_info, OutputLevel};

/// Configuration for a signing request
#[derive(Debug, Clone)]
pub struct SigningRequestConfig<'a> {
    pub binary_path: &'a str,
    pub checksum_algorithm: &'a str,
    pub runtime_name: &'a str,
    pub target_arch: &'a str,
    pub key_name: &'a str,
    pub keyid: &'a str,
    pub volume_name: &'a str,
    pub verbose: bool,
}

/// Handle a signing request from a container
///
/// # Arguments
/// * `config` - Configuration for the signing request
///
/// # Returns
/// * Tuple of (signature_path, signature_content)
pub async fn handle_signing_request(config: SigningRequestConfig<'_>) -> Result<(String, String)> {
    let SigningRequestConfig {
        binary_path,
        checksum_algorithm,
        runtime_name,
        target_arch,
        key_name,
        keyid,
        volume_name,
        verbose,
    } = config;
    // Validate binary path is within expected volume structure
    validate_binary_path(binary_path, target_arch, runtime_name)?;

    // Parse checksum algorithm
    let checksum_algo: ChecksumAlgorithm = checksum_algorithm
        .parse()
        .with_context(|| format!("Invalid checksum algorithm: {}", checksum_algorithm))?;

    // Extract binary from volume
    let temp_dir = TempDir::new().context("Failed to create temporary directory")?;
    let binary_filename = Path::new(binary_path)
        .file_name()
        .context("Invalid binary path: no filename")?
        .to_str()
        .context("Invalid binary filename encoding")?;

    let temp_binary_path = temp_dir.path().join(binary_filename);

    extract_binary_from_volume(volume_name, binary_path, &temp_binary_path, verbose).await?;

    // Compute hash of the binary
    if verbose {
        print_info(
            &format!("Computing {} hash of binary", checksum_algo.name()),
            OutputLevel::Verbose,
        );
    }

    let hash_bytes = compute_file_hash(&temp_binary_path, &checksum_algo)
        .context("Failed to compute file hash")?;

    let hash_hex = hash_bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    // Get file size
    let file_size = std::fs::metadata(&temp_binary_path)
        .context("Failed to get file metadata")?
        .len();

    // Create a hash manifest with a single entry
    let manifest = HashManifest {
        runtime: runtime_name.to_string(),
        checksum_algorithm: checksum_algo.name().to_string(),
        files: vec![HashManifestEntry {
            container_path: binary_path.to_string(),
            hash: hash_hex,
            size: file_size,
        }],
    };

    // Sign the hash
    if verbose {
        print_info(
            &format!("Signing binary with key '{}'", key_name),
            OutputLevel::Verbose,
        );
    }

    let signatures =
        sign_hash_manifest(&manifest, key_name, keyid).context("Failed to sign binary hash")?;

    if signatures.is_empty() {
        anyhow::bail!("No signature generated");
    }

    let signature = &signatures[0];

    // Write signature back to volume
    if verbose {
        print_info("Writing signature to volume", OutputLevel::Verbose);
    }

    write_signature_to_volume(
        volume_name,
        &signature.container_path,
        &signature.content,
        verbose,
    )
    .await?;

    Ok((signature.container_path.clone(), signature.content.clone()))
}

/// Validate that a binary path is within the expected volume structure
fn validate_binary_path(binary_path: &str, target_arch: &str, runtime_name: &str) -> Result<()> {
    // Expected patterns:
    // 1. /opt/_avocado/{target}/runtimes/{runtime}/...
    // 2. /opt/_avocado/{target}/output/runtimes/{runtime}/...
    let expected_prefix_1 = format!("/opt/_avocado/{}/runtimes/{}", target_arch, runtime_name);
    let expected_prefix_2 = format!(
        "/opt/_avocado/{}/output/runtimes/{}",
        target_arch, runtime_name
    );

    let is_valid =
        binary_path.starts_with(&expected_prefix_1) || binary_path.starts_with(&expected_prefix_2);

    if !is_valid {
        anyhow::bail!(
            "Binary path '{}' is not within expected runtime directories '{}' or '{}'",
            binary_path,
            expected_prefix_1,
            expected_prefix_2
        );
    }

    // Prevent path traversal
    if binary_path.contains("..") {
        anyhow::bail!("Binary path contains invalid '..' components");
    }

    Ok(())
}

/// Extract a binary from a Docker volume using docker cp
async fn extract_binary_from_volume(
    volume_name: &str,
    container_path: &str,
    dest_path: &Path,
    verbose: bool,
) -> Result<()> {
    if verbose {
        print_info(
            &format!("Extracting binary from volume: {}", container_path),
            OutputLevel::Verbose,
        );
    }

    // Create a temporary container with the volume mounted
    let container_name = format!("avocado-sign-extract-{}", uuid::Uuid::new_v4());
    let volume_mount = format!("{}:/opt/_avocado:ro", volume_name);

    // Create container
    let create_cmd = [
        "docker",
        "create",
        "--name",
        &container_name,
        "-v",
        &volume_mount,
        "busybox",
        "true",
    ];

    let output = Command::new(create_cmd[0])
        .args(&create_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to create temporary container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to create temporary container: {}", stderr);
    }

    // Copy file from container
    let container_src = format!("{}:{}", container_name, container_path);
    let dest_str = dest_path
        .to_str()
        .context("Invalid destination path encoding")?;

    let cp_cmd = ["docker", "cp", &container_src, dest_str];

    let output = Command::new(cp_cmd[0])
        .args(&cp_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to copy file from container")?;

    // Clean up container
    let _ = cleanup_container(&container_name).await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to copy binary from volume: {}", stderr);
    }

    if !dest_path.exists() {
        anyhow::bail!("Binary extraction failed: file not found after docker cp");
    }

    Ok(())
}

/// Write a signature file to a Docker volume using docker cp
async fn write_signature_to_volume(
    volume_name: &str,
    signature_path: &str,
    signature_content: &str,
    verbose: bool,
) -> Result<()> {
    if verbose {
        print_info(
            &format!("Writing signature to volume: {}", signature_path),
            OutputLevel::Verbose,
        );
    }

    // Create a temporary file with the signature content
    let temp_dir = TempDir::new().context("Failed to create temporary directory")?;
    let temp_sig_file = temp_dir.path().join("signature.sig");

    std::fs::write(&temp_sig_file, signature_content)
        .context("Failed to write signature to temporary file")?;

    // Create a temporary container with the volume mounted
    let container_name = format!("avocado-sign-write-{}", uuid::Uuid::new_v4());
    let volume_mount = format!("{}:/opt/_avocado:rw", volume_name);

    // Create container
    let create_cmd = [
        "docker",
        "create",
        "--name",
        &container_name,
        "-v",
        &volume_mount,
        "busybox",
        "true",
    ];

    let output = Command::new(create_cmd[0])
        .args(&create_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to create temporary container")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to create temporary container: {}", stderr);
    }

    // Copy signature file to container
    let temp_sig_str = temp_sig_file
        .to_str()
        .context("Invalid temporary file path encoding")?;
    let container_dest = format!("{}:{}", container_name, signature_path);

    let cp_cmd = ["docker", "cp", temp_sig_str, &container_dest];

    let output = Command::new(cp_cmd[0])
        .args(&cp_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to copy signature to container")?;

    // Clean up container
    let _ = cleanup_container(&container_name).await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to write signature to volume: {}", stderr);
    }

    Ok(())
}

/// Clean up a temporary container
async fn cleanup_container(container_name: &str) -> Result<()> {
    let rm_cmd = ["docker", "rm", "-f", container_name];

    let _ = Command::new(rm_cmd[0])
        .args(&rm_cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_binary_path_valid() {
        let result = validate_binary_path(
            "/opt/_avocado/x86_64/runtimes/test-runtime/custom-binary",
            "x86_64",
            "test-runtime",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_binary_path_valid_output() {
        let result = validate_binary_path(
            "/opt/_avocado/x86_64/output/runtimes/test-runtime/custom-binary",
            "x86_64",
            "test-runtime",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_binary_path_valid_nested_output() {
        let result = validate_binary_path(
            "/opt/_avocado/qemux86-64/output/runtimes/dev/stone/_build/peridio-firmware.bin",
            "qemux86-64",
            "dev",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_binary_path_wrong_runtime() {
        let result = validate_binary_path(
            "/opt/_avocado/x86_64/runtimes/other-runtime/custom-binary",
            "x86_64",
            "test-runtime",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_binary_path_traversal() {
        let result = validate_binary_path(
            "/opt/_avocado/x86_64/runtimes/test-runtime/../../../etc/passwd",
            "x86_64",
            "test-runtime",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_binary_path_wrong_prefix() {
        let result = validate_binary_path("/tmp/malicious-binary", "x86_64", "test-runtime");
        assert!(result.is_err());
    }
}
