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
use ed25519_compact::{SecretKey, Signature};
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
fn load_signing_key(keyid: &str) -> Result<SecretKey> {
    use ed25519_compact::{KeyPair, Seed};

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
        anyhow::bail!(
            "Invalid private key length: expected 32 bytes, got {}. The key may be corrupted.",
            private_key_bytes.len()
        );
    }

    // Load the seed (32 bytes) and create the keypair from it
    // This works for both keys created with ed25519-dalek and ed25519-compact
    // as both store the 32-byte seed
    let seed = Seed::from_slice(&private_key_bytes).with_context(|| {
        format!(
            "Failed to parse seed bytes. The key file may be corrupted: {}",
            key_file_path.display()
        )
    })?;

    let keypair = KeyPair::from_seed(seed);
    Ok(keypair.sk)
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
    let signature: Signature = signing_key.sign(&hash, None);

    // Create signature file content
    let sig_content = create_signature_content(
        &hash,
        signature
            .as_ref()
            .try_into()
            .expect("signature should be 64 bytes"),
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

    // Sign extension images in runtime-specific extensions directory
    let ext_dir = avocado_prefix
        .join("runtimes")
        .join(runtime_name)
        .join("extensions");
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
            let signature: Signature = signing_key.sign(hash, None);
            Ok(signature.as_ref().to_vec())
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
/// This function implements PKCS#11 signing for hardware devices (TPM, YubiKey, HSMs).
fn sign_with_pkcs11(uri: &str, hash: &[u8]) -> Result<Vec<u8>> {
    use super::pkcs11_devices::{
        init_pkcs11_session, parse_pkcs11_uri, sign_with_pkcs11_device, DeviceType,
        Pkcs11AuthMethod,
    };
    use std::env;

    // Parse PKCS#11 URI to extract token and object labels
    let (token_label, object_label) =
        parse_pkcs11_uri(uri).context("Failed to parse PKCS#11 URI")?;

    // Determine device type from token label or environment
    let device_type = if token_label.to_lowercase().contains("tpm") {
        DeviceType::Tpm
    } else if token_label.to_lowercase().contains("yubikey")
        || token_label.to_lowercase().contains("yubico")
    {
        DeviceType::Yubikey
    } else {
        DeviceType::Auto
    };

    // Get auth method from environment or prompt
    let auth_method = if let Ok(_pin) = env::var("AVOCADO_PKCS11_PIN") {
        Pkcs11AuthMethod::EnvVar("AVOCADO_PKCS11_PIN".to_string())
    } else {
        // For signing operations, prompt for PIN
        // TPM and YubiKey typically require authentication for private key operations
        Pkcs11AuthMethod::Prompt
    };

    // Get the authentication credential
    let auth = super::pkcs11_devices::get_device_auth(&auth_method)
        .context("Failed to get authentication for signing")?;

    // Initialize PKCS#11 and open session with authentication
    let (pkcs11, session) =
        init_pkcs11_session(&device_type, Some(&token_label), &auth, &auth_method)
            .context("Failed to initialize PKCS#11 session for signing")?;

    // Sign the data
    let signature = sign_with_pkcs11_device(&session, &object_label, hash, &auth)
        .context("Failed to sign data with PKCS#11 device")?;

    // Explicitly keep pkcs11 alive until signing is done
    drop(pkcs11);

    Ok(signature)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{:02x}", b);
            acc
        })
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
    use ed25519_compact::{KeyPair, Seed};

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

    #[test]
    fn test_create_and_verify_signature_sha256() {
        // Generate a test keypair
        let keypair = KeyPair::from_seed(Seed::default());
        let secret_key = keypair.sk;
        let public_key = keypair.pk;

        // Create test data
        let test_data = b"This is test data for signing";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Sha256).unwrap();

        // Sign the hash
        let signature = secret_key.sign(&hash, None);

        // Create signature content
        let sig_content = create_signature_content(
            &hash,
            signature
                .as_ref()
                .try_into()
                .expect("signature should be 64 bytes"),
            &ChecksumAlgorithm::Sha256,
            "test-key",
            "test-keyid",
        )
        .unwrap();

        // Parse the signature JSON
        let sig_json: serde_json::Value = serde_json::from_str(&sig_content).unwrap();

        // Extract signature and checksum from JSON
        let signature_hex = sig_json["signature"].as_str().unwrap();
        let checksum_hex = sig_json["checksum"].as_str().unwrap();

        // Decode hex signature
        let signature_bytes = hex_decode(signature_hex).unwrap();
        let checksum_bytes = hex_decode(checksum_hex).unwrap();

        // Verify signature format
        assert_eq!(signature_bytes.len(), 64, "Signature should be 64 bytes");
        assert_eq!(checksum_bytes, hash, "Checksum should match original hash");

        // Reconstruct signature for verification
        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&signature_bytes);
        let reconstructed_sig = ed25519_compact::Signature::from_slice(&sig_array).unwrap();

        // Verify the signature with the public key
        let result = public_key.verify(&hash, &reconstructed_sig);
        assert!(
            result.is_ok(),
            "Signature verification should succeed with original public key"
        );
    }

    #[test]
    fn test_create_and_verify_signature_blake3() {
        // Generate a test keypair
        let keypair = KeyPair::from_seed(Seed::default());
        let secret_key = keypair.sk;
        let public_key = keypair.pk;

        // Create test data
        let test_data = b"This is test data for BLAKE3 signing";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Blake3).unwrap();

        // Sign the hash
        let signature = secret_key.sign(&hash, None);

        // Create signature content
        let sig_content = create_signature_content(
            &hash,
            signature
                .as_ref()
                .try_into()
                .expect("signature should be 64 bytes"),
            &ChecksumAlgorithm::Blake3,
            "test-key-blake3",
            "test-keyid-blake3",
        )
        .unwrap();

        // Parse the signature JSON
        let sig_json: serde_json::Value = serde_json::from_str(&sig_content).unwrap();

        // Verify JSON structure
        assert_eq!(sig_json["version"].as_str().unwrap(), "1");
        assert_eq!(sig_json["checksum_algorithm"].as_str().unwrap(), "blake3");
        assert_eq!(sig_json["key_name"].as_str().unwrap(), "test-key-blake3");
        assert_eq!(sig_json["keyid"].as_str().unwrap(), "test-keyid-blake3");

        // Extract and verify signature
        let signature_hex = sig_json["signature"].as_str().unwrap();
        let signature_bytes = hex_decode(signature_hex).unwrap();

        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&signature_bytes);
        let reconstructed_sig = ed25519_compact::Signature::from_slice(&sig_array).unwrap();

        // Verify the signature
        let result = public_key.verify(&hash, &reconstructed_sig);
        assert!(
            result.is_ok(),
            "BLAKE3 signature verification should succeed"
        );
    }

    #[test]
    fn test_signature_json_format() {
        // Generate a test keypair
        let keypair = KeyPair::from_seed(Seed::default());
        let secret_key = keypair.sk;

        // Create test hash
        let test_hash = [0x42u8; 32]; // Simple test hash

        // Sign the hash
        let signature = secret_key.sign(test_hash, None);

        // Create signature content
        let sig_content = create_signature_content(
            &test_hash,
            signature
                .as_ref()
                .try_into()
                .expect("signature should be 64 bytes"),
            &ChecksumAlgorithm::Sha256,
            "my-key",
            "my-keyid-123",
        )
        .unwrap();

        // Parse JSON
        let sig_json: serde_json::Value = serde_json::from_str(&sig_content).unwrap();

        // Verify all required fields exist
        assert!(sig_json.get("version").is_some(), "version field missing");
        assert!(
            sig_json.get("checksum_algorithm").is_some(),
            "checksum_algorithm field missing"
        );
        assert!(sig_json.get("checksum").is_some(), "checksum field missing");
        assert!(
            sig_json.get("signature").is_some(),
            "signature field missing"
        );
        assert!(sig_json.get("key_name").is_some(), "key_name field missing");
        assert!(sig_json.get("keyid").is_some(), "keyid field missing");

        // Verify values
        assert_eq!(sig_json["version"].as_str().unwrap(), "1");
        assert_eq!(sig_json["key_name"].as_str().unwrap(), "my-key");
        assert_eq!(sig_json["keyid"].as_str().unwrap(), "my-keyid-123");
    }

    #[test]
    fn test_hex_encode_decode_roundtrip() {
        let original = vec![0x00, 0x01, 0xAB, 0xCD, 0xEF, 0xFF];

        // Encode to hex
        let hex = hex_encode(&original);

        // Verify format
        assert_eq!(hex.len(), original.len() * 2);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));

        // Decode back
        let decoded = hex_decode(&hex).unwrap();

        // Verify roundtrip
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_signature_with_different_keys_fails() {
        // Generate two different keypairs
        let keypair1 = KeyPair::from_seed(Seed::default());
        let secret_key1 = keypair1.sk;

        let mut seed_bytes = [0u8; 32];
        seed_bytes[0] = 1; // Different seed
        let seed2 = Seed::from_slice(&seed_bytes).unwrap();
        let keypair2 = KeyPair::from_seed(seed2);
        let public_key2 = keypair2.pk;

        // Create test data and sign with key1
        let test_data = b"Test data";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Sha256).unwrap();
        let signature = secret_key1.sign(&hash, None);

        // Try to verify with key2 (should fail)
        let result = public_key2.verify(&hash, &signature);
        assert!(
            result.is_err(),
            "Verification should fail with different public key"
        );
    }

    #[test]
    fn test_signature_with_modified_hash_fails() {
        // Generate a keypair
        let keypair = KeyPair::from_seed(Seed::default());
        let secret_key = keypair.sk;
        let public_key = keypair.pk;

        // Create test data and sign
        let test_data = b"Original data";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Sha256).unwrap();
        let signature = secret_key.sign(&hash, None);

        // Compute hash of different data
        let different_data = b"Modified data";
        let different_hash =
            compute_file_hash_from_bytes(different_data, &ChecksumAlgorithm::Sha256).unwrap();

        // Try to verify signature against different hash (should fail)
        let result = public_key.verify(&different_hash, &signature);
        assert!(
            result.is_err(),
            "Verification should fail with modified data"
        );
    }

    #[test]
    fn test_compute_file_hash_sha256() {
        let test_data = b"Test data for SHA256";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Sha256).unwrap();

        // SHA256 produces 32 bytes
        assert_eq!(hash.len(), 32, "SHA256 hash should be 32 bytes");

        // Verify it's deterministic
        let hash2 = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Sha256).unwrap();
        assert_eq!(hash, hash2, "Hash should be deterministic");
    }

    #[test]
    fn test_compute_file_hash_blake3() {
        let test_data = b"Test data for BLAKE3";
        let hash = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Blake3).unwrap();

        // BLAKE3 produces 32 bytes
        assert_eq!(hash.len(), 32, "BLAKE3 hash should be 32 bytes");

        // Verify it's deterministic
        let hash2 = compute_file_hash_from_bytes(test_data, &ChecksumAlgorithm::Blake3).unwrap();
        assert_eq!(hash, hash2, "Hash should be deterministic");
    }

    // Helper function to compute hash from bytes (for testing)
    fn compute_file_hash_from_bytes(data: &[u8], algorithm: &ChecksumAlgorithm) -> Result<Vec<u8>> {
        match algorithm {
            ChecksumAlgorithm::Sha256 => {
                let mut hasher = Sha256::new();
                hasher.update(data);
                Ok(hasher.finalize().to_vec())
            }
            ChecksumAlgorithm::Blake3 => Ok(blake3::hash(data).as_bytes().to_vec()),
        }
    }
}
