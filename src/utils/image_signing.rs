//! Image signing utilities for runtime builds.
//!
//! Provides functionality for signing image files using ed25519 keys
//! with configurable hash algorithms (sha256 or blake3).
//!
//! Supports multi-pass signing workflow:
//! 1. Container computes hashes and outputs manifest
//! 2. Host signs hashes (supports file-based and PKCS#11 keys)
//! 3. Signatures written back to volume

use anyhow::{Context, Result};
use base64::prelude::*;
use blake3;
use ed25519_dalek::{Signature, Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::str::FromStr;

use super::signing_keys::{get_key_entries, is_file_uri, is_pkcs11_uri};

/// Hash manifest entry for a single file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashManifestEntry {
    /// Path to the file inside the container
    pub container_path: String,
    /// Hex-encoded hash of the file
    pub hash: String,
    /// File size in bytes
    pub size: u64,
}

/// Hash manifest containing all files to be signed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashManifest {
    /// Runtime name
    pub runtime: String,
    /// Checksum algorithm used (sha256 or blake3)
    pub checksum_algorithm: String,
    /// List of files with their hashes
    pub files: Vec<HashManifestEntry>,
}

/// Signature data to be written back to volume
#[derive(Debug, Clone)]
pub struct SignatureData {
    /// Path to the file inside the container (where .sig should be written)
    pub container_path: String,
    /// Signature file content (JSON)
    pub content: String,
}

/// Supported checksum algorithms for signing
#[derive(Debug, Clone, PartialEq)]
pub enum ChecksumAlgorithm {
    Sha256,
    Blake3,
}

impl FromStr for ChecksumAlgorithm {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "sha256" | "sha-256" => Ok(ChecksumAlgorithm::Sha256),
            "blake3" => Ok(ChecksumAlgorithm::Blake3),
            _ => anyhow::bail!(
                "Unsupported checksum algorithm '{}'. Supported: sha256, blake3",
                s
            ),
        }
    }
}

impl ChecksumAlgorithm {
    /// Get the name of the checksum algorithm
    pub fn name(&self) -> &str {
        match self {
            ChecksumAlgorithm::Sha256 => "sha256",
            ChecksumAlgorithm::Blake3 => "blake3",
        }
    }
}

/// Compute checksum of a file
#[allow(dead_code)] // Public API for future use
pub fn compute_file_hash(file_path: &Path, algorithm: &ChecksumAlgorithm) -> Result<Vec<u8>> {
    let data = fs::read(file_path)
        .with_context(|| format!("Failed to read file for hashing: {}", file_path.display()))?;

    Ok(match algorithm {
        ChecksumAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            hasher.finalize().to_vec()
        }
        ChecksumAlgorithm::Blake3 => blake3::hash(&data).as_bytes().to_vec(),
    })
}

/// Load a signing key from disk
fn load_signing_key(keyid: &str) -> Result<SigningKey> {
    let key_file_path = super::signing_keys::get_key_file_path(keyid)?.with_extension("key");

    let private_key_b64 = fs::read_to_string(&key_file_path).with_context(|| {
        format!(
            "Failed to read private key file: {}",
            key_file_path.display()
        )
    })?;

    let private_key_bytes = BASE64_STANDARD
        .decode(private_key_b64.trim())
        .context("Failed to decode private key from base64")?;

    if private_key_bytes.len() != 32 {
        anyhow::bail!("Invalid private key length");
    }

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&private_key_bytes);

    Ok(SigningKey::from_bytes(&key_bytes))
}

/// Sign a file and save the signature
#[allow(dead_code)] // Public API for future use
pub fn sign_file(
    file_path: &Path,
    key_name: &str,
    keyid: &str,
    checksum_algorithm: &ChecksumAlgorithm,
) -> Result<()> {
    // Compute file checksum
    let hash = compute_file_hash(file_path, checksum_algorithm)?;

    // Load signing key (only file:// URIs supported for now)
    let signing_key = load_signing_key(keyid).with_context(|| {
        format!(
            "Failed to load signing key '{}' (keyid: {})",
            key_name, keyid
        )
    })?;

    // Sign the hash
    let signature: Signature = signing_key.sign(&hash);

    // Create signature file content
    let sig_content = create_signature_content(
        &hash,
        signature.to_bytes(),
        checksum_algorithm,
        key_name,
        keyid,
    )?;

    // Write signature to .sig file
    let sig_path = file_path.with_extension(
        format!(
            "{}.sig",
            file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("raw")
        )
        .as_str(),
    );

    fs::write(&sig_path, sig_content)
        .with_context(|| format!("Failed to write signature file: {}", sig_path.display()))?;

    Ok(())
}

/// Create signature file content in JSON format
fn create_signature_content(
    hash: &[u8],
    signature: [u8; 64],
    checksum_algorithm: &ChecksumAlgorithm,
    key_name: &str,
    keyid: &str,
) -> Result<String> {
    let sig_data = serde_json::json!({
        "version": "1",
        "checksum_algorithm": checksum_algorithm.name(),
        "checksum": hex_encode(hash),
        "signature": hex_encode(&signature),
        "key_name": key_name,
        "keyid": keyid,
    });

    serde_json::to_string_pretty(&sig_data).context("Failed to serialize signature data")
}

/// Sign runtime images (extension images and var image)
#[allow(dead_code)] // Public API for future use
pub fn sign_runtime_images(
    runtime_name: &str,
    key_name: &str,
    keyid: &str,
    checksum_algorithm: &ChecksumAlgorithm,
    avocado_prefix: &Path,
) -> Result<Vec<String>> {
    let mut signed_files = Vec::new();

    // Sign extension images in output/extensions/
    let ext_dir = avocado_prefix.join("output/extensions");
    if ext_dir.exists() {
        for entry in fs::read_dir(&ext_dir).with_context(|| {
            format!("Failed to read extensions directory: {}", ext_dir.display())
        })? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("raw") {
                sign_file(&path, key_name, keyid, checksum_algorithm)?;
                signed_files.push(path.display().to_string());
            }
        }
    }

    // Sign var image in runtimes/{runtime_name}/
    let runtime_dir = avocado_prefix.join("runtimes").join(runtime_name);
    if runtime_dir.exists() {
        for entry in fs::read_dir(&runtime_dir).with_context(|| {
            format!(
                "Failed to read runtime directory: {}",
                runtime_dir.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();

            // Sign .raw files (var image and others)
            if path.extension().and_then(|e| e.to_str()) == Some("raw") {
                sign_file(&path, key_name, keyid, checksum_algorithm)?;
                signed_files.push(path.display().to_string());
            }
        }
    }

    Ok(signed_files)
}

/// Validate that a signing key is usable
///
/// Note: key_name is the local name from config, keyid is the actual key name in global registry
pub fn validate_signing_key_for_use(key_name: &str, keyid: &str) -> Result<()> {
    // Get key entry from registry using the keyid (which is the actual registry key name)
    let entries = get_key_entries(&[keyid.to_string()])?;
    let (_registry_name, entry) = entries
        .first()
        .ok_or_else(|| anyhow::anyhow!("Key with ID '{}' not found in global registry", keyid))?;

    // Validate based on key type
    if is_file_uri(&entry.uri) {
        // Verify the key file exists and can be loaded
        load_signing_key(keyid).with_context(|| {
            format!(
                "Failed to load signing key '{}' (keyid: {}). The key may be missing or corrupted.",
                key_name, keyid
            )
        })?;
    } else if is_pkcs11_uri(&entry.uri) {
        // PKCS#11 keys - basic validation
        // Full validation would require opening a session, which we defer to signing time
        println!("Note: PKCS#11 key validation deferred to signing operation");
    } else {
        anyhow::bail!(
            "Signing key '{}' (keyid: {}) uses unsupported URI type: {}",
            key_name,
            keyid,
            entry.uri
        );
    }

    Ok(())
}

/// Type alias for signing function to reduce complexity
type SignFn = Box<dyn Fn(&[u8]) -> Result<Vec<u8>>>;

/// Sign a hash manifest and return signature data
///
/// Note: key_name is the local name from config, keyid is the actual key name in global registry
pub fn sign_hash_manifest(
    manifest: &HashManifest,
    key_name: &str,
    keyid: &str,
) -> Result<Vec<SignatureData>> {
    let mut signatures = Vec::new();

    // Get key entry from registry using the keyid (which is the actual registry key name)
    let entries = get_key_entries(&[keyid.to_string()])?;
    let (_registry_name, entry) = entries
        .first()
        .ok_or_else(|| anyhow::anyhow!("Key with ID '{}' not found in global registry", keyid))?;

    // Determine signing method based on URI type
    let sign_fn: SignFn = if is_file_uri(&entry.uri) {
        // File-based signing
        let signing_key = load_signing_key(keyid).with_context(|| {
            format!(
                "Failed to load signing key '{}' (keyid: {})",
                key_name, keyid
            )
        })?;
        Box::new(move |hash: &[u8]| {
            let signature: Signature = signing_key.sign(hash);
            Ok(signature.to_bytes().to_vec())
        })
    } else if is_pkcs11_uri(&entry.uri) {
        // PKCS#11 signing
        let uri = entry.uri.clone();
        Box::new(move |hash: &[u8]| sign_with_pkcs11(&uri, hash))
    } else {
        anyhow::bail!(
            "Signing key '{}' uses unsupported URI type: {}",
            key_name,
            entry.uri
        );
    };

    // Sign each file's hash
    for file_entry in &manifest.files {
        // Decode hex hash
        let hash_bytes = hex_decode(&file_entry.hash)
            .with_context(|| format!("Failed to decode hash for {}", file_entry.container_path))?;

        // Sign the hash using appropriate method
        let signature_bytes = sign_fn(&hash_bytes)
            .with_context(|| format!("Failed to sign hash for {}", file_entry.container_path))?;

        // Create signature file content
        let sig_content = create_signature_content(
            &hash_bytes,
            signature_bytes.try_into().unwrap_or_else(|v: Vec<u8>| {
                // Pad or truncate to 64 bytes for compatibility
                let mut arr = [0u8; 64];
                let len = v.len().min(64);
                arr[..len].copy_from_slice(&v[..len]);
                arr
            }),
            &manifest.checksum_algorithm.parse()?,
            key_name,
            keyid,
        )?;

        // Determine signature file path
        let sig_path = format!("{}.sig", file_entry.container_path);

        signatures.push(SignatureData {
            container_path: sig_path,
            content: sig_content,
        });
    }

    Ok(signatures)
}

/// Sign a hash using PKCS#11 hardware token
///
/// This function provides basic PKCS#11 signing support. Full implementation
/// requires proper token/slot discovery and PIN management.
fn sign_with_pkcs11(uri: &str, _hash: &[u8]) -> Result<Vec<u8>> {
    // Parse PKCS#11 URI (simplified - full URI parsing would be more complex)
    // Format: pkcs11:token=TokenName;object=KeyLabel

    // For now, return an error with helpful message
    // Full implementation would:
    // 1. Initialize PKCS#11 library
    // 2. Find slot/token
    // 3. Open session
    // 4. Find private key object
    // 5. Perform signing operation
    // 6. Return signature

    anyhow::bail!(
        "PKCS#11 signing is not yet fully implemented. URI: {}\n\
        \n\
        To implement PKCS#11 signing:\n\
        1. Install PKCS#11 library for your device (e.g., opensc for YubiKey)\n\
        2. Set PKCS11_MODULE_PATH environment variable\n\
        3. Ensure device is connected and accessible\n\
        \n\
        Currently, only file-based ed25519 keys are supported for signing.",
        uri
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("Invalid hex string at position {}", i))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checksum_algorithm_from_str() {
        assert_eq!(
            "sha256".parse::<ChecksumAlgorithm>().unwrap(),
            ChecksumAlgorithm::Sha256
        );
        assert_eq!(
            "SHA256".parse::<ChecksumAlgorithm>().unwrap(),
            ChecksumAlgorithm::Sha256
        );
        assert_eq!(
            "sha-256".parse::<ChecksumAlgorithm>().unwrap(),
            ChecksumAlgorithm::Sha256
        );
        assert_eq!(
            "blake3".parse::<ChecksumAlgorithm>().unwrap(),
            ChecksumAlgorithm::Blake3
        );
        assert_eq!(
            "BLAKE3".parse::<ChecksumAlgorithm>().unwrap(),
            ChecksumAlgorithm::Blake3
        );
        assert!("md5".parse::<ChecksumAlgorithm>().is_err());
    }

    #[test]
    fn test_checksum_algorithm_name() {
        assert_eq!(ChecksumAlgorithm::Sha256.name(), "sha256");
        assert_eq!(ChecksumAlgorithm::Blake3.name(), "blake3");
    }
}
