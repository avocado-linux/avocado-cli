//! Signing keys management utilities.
//!
//! Provides functionality for managing ed25519 signing keys in a global config location.
//! Supports both file-based keys and PKCS#11 URIs for hardware security modules.

use anyhow::{Context, Result};
use base64::prelude::*;
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use ed25519_compact::{KeyPair, PublicKey, SecretKey, Seed};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Registry file name for storing key metadata
const KEYS_REGISTRY_FILE: &str = "keys.json";

/// Subdirectory name for signing keys within the avocado config
const SIGNING_KEYS_DIR: &str = "signing-keys";

/// Represents a single signing key entry in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    /// Unique key identifier (SHA-256 hash of public key)
    pub keyid: String,
    /// Cryptographic algorithm used (e.g., "ed25519", "ecdsa-p256", "ecdsa-p384", "rsa2048", "rsa4096")
    pub algorithm: String,
    /// Timestamp when the key was created/registered
    pub created_at: DateTime<Utc>,
    /// URI pointing to the key (file:// or pkcs11:)
    pub uri: String,
}

/// Global signing keys registry
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeysRegistry {
    /// Map of key names to their metadata
    pub keys: HashMap<String, KeyEntry>,
}

impl KeysRegistry {
    /// Load the registry from disk, creating an empty one if it doesn't exist
    pub fn load() -> Result<Self> {
        let registry_path = get_registry_path()?;

        if !registry_path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&registry_path).with_context(|| {
            format!("Failed to read registry file: {}", registry_path.display())
        })?;

        serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse registry file: {}", registry_path.display()))
    }

    /// Save the registry to disk
    pub fn save(&self) -> Result<()> {
        let registry_path = get_registry_path()?;

        // Ensure parent directory exists
        if let Some(parent) = registry_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        let contents =
            serde_json::to_string_pretty(self).context("Failed to serialize registry")?;

        fs::write(&registry_path, contents)
            .with_context(|| format!("Failed to write registry file: {}", registry_path.display()))
    }

    /// Add a new key entry to the registry
    pub fn add_key(&mut self, name: String, entry: KeyEntry) -> Result<()> {
        if self.keys.contains_key(&name) {
            anyhow::bail!("A key with name '{}' already exists", name);
        }
        self.keys.insert(name, entry);
        Ok(())
    }

    /// Remove a key entry from the registry
    pub fn remove_key(&mut self, name: &str) -> Result<KeyEntry> {
        self.keys
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("No key found with name '{}'", name))
    }

    /// Get a key entry by name
    pub fn get_key(&self, name: &str) -> Option<&KeyEntry> {
        self.keys.get(name)
    }
}

/// Get the base directory for avocado global config
pub fn get_avocado_config_dir() -> Result<PathBuf> {
    ProjectDirs::from("", "", "avocado")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("Could not determine config directory for your platform"))
}

/// Get the directory for storing signing keys
///
/// When running in a container, this checks the AVOCADO_SIGNING_KEYS_DIR environment variable
/// which points to the mounted keys directory. Otherwise, it returns the host path.
pub fn get_signing_keys_dir() -> Result<PathBuf> {
    // Check if we're running in a container with mounted keys
    if let Ok(container_keys_dir) = std::env::var("AVOCADO_SIGNING_KEYS_DIR") {
        return Ok(PathBuf::from(container_keys_dir));
    }

    // Otherwise use the host path
    let config_dir = get_avocado_config_dir()?;
    Ok(config_dir.join(SIGNING_KEYS_DIR))
}

/// Get the path to the keys registry file
pub fn get_registry_path() -> Result<PathBuf> {
    let keys_dir = get_signing_keys_dir()?;
    Ok(keys_dir.join(KEYS_REGISTRY_FILE))
}

/// Get the path for a key file (without extension)
pub fn get_key_file_path(keyid: &str) -> Result<PathBuf> {
    let keys_dir = get_signing_keys_dir()?;
    Ok(keys_dir.join(keyid))
}

/// Generate a key ID from a public key (full SHA-256 hash, base16/hex encoded)
///
/// Returns the full 64-character hex-encoded SHA-256 hash of the public key.
/// This key ID is also used as the default friendly name when no name is provided.
pub fn generate_keyid(public_key: &PublicKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key.as_ref());
    let hash = hasher.finalize();
    hex::encode(&hash)
}

/// Generate a new ed25519 keypair
pub fn generate_keypair() -> (SecretKey, PublicKey) {
    let keypair = KeyPair::from_seed(Seed::default());
    (keypair.sk, keypair.pk)
}

/// Save a keypair to disk
pub fn save_keypair(
    keyid: &str,
    signing_key: &SecretKey,
    verifying_key: &PublicKey,
) -> Result<PathBuf> {
    let keys_dir = get_signing_keys_dir()?;
    fs::create_dir_all(&keys_dir).with_context(|| {
        format!(
            "Failed to create signing keys directory: {}",
            keys_dir.display()
        )
    })?;

    let base_path = get_key_file_path(keyid)?;
    let private_key_path = base_path.with_extension("key");
    let public_key_path = base_path.with_extension("pub");

    // Save private key (base64 encoded)
    // Store the 32-byte seed, which can be used to reconstruct the key
    let seed_bytes = signing_key.seed();
    let private_key_b64 = BASE64_STANDARD.encode(seed_bytes.as_ref());
    fs::write(&private_key_path, &private_key_b64).with_context(|| {
        format!(
            "Failed to write private key: {}",
            private_key_path.display()
        )
    })?;

    // Set restrictive permissions on private key (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        fs::set_permissions(&private_key_path, permissions).with_context(|| {
            format!(
                "Failed to set permissions on private key: {}",
                private_key_path.display()
            )
        })?;
    }

    // Save public key (base64 encoded)
    let public_key_b64 = BASE64_STANDARD.encode(verifying_key.as_ref());
    fs::write(&public_key_path, &public_key_b64)
        .with_context(|| format!("Failed to write public key: {}", public_key_path.display()))?;

    Ok(base_path)
}

/// Delete key files from disk
pub fn delete_key_files(keyid: &str) -> Result<()> {
    let base_path = get_key_file_path(keyid)?;
    let private_key_path = base_path.with_extension("key");
    let public_key_path = base_path.with_extension("pub");

    // Remove private key if it exists
    if private_key_path.exists() {
        fs::remove_file(&private_key_path).with_context(|| {
            format!(
                "Failed to delete private key: {}",
                private_key_path.display()
            )
        })?;
    }

    // Remove public key if it exists
    if public_key_path.exists() {
        fs::remove_file(&public_key_path).with_context(|| {
            format!("Failed to delete public key: {}", public_key_path.display())
        })?;
    }

    Ok(())
}

/// Check if a URI is a file:// URI
pub fn is_file_uri(uri: &str) -> bool {
    uri.starts_with("file://")
}

/// Check if a URI is a pkcs11: URI
pub fn is_pkcs11_uri(uri: &str) -> bool {
    uri.starts_with("pkcs11:")
}

/// Create a file:// URI from a path
pub fn path_to_file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Validate that all signing key names exist in the global registry
///
/// Returns Ok(()) if all keys exist, or an error listing the missing keys
#[allow(dead_code)] // Public API for future use
pub fn validate_signing_keys(key_names: &[String]) -> Result<()> {
    if key_names.is_empty() {
        return Ok(());
    }

    let registry = KeysRegistry::load()?;
    let missing: Vec<_> = key_names
        .iter()
        .filter(|name| !registry.keys.contains_key(*name))
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "The following signing keys are referenced in the config but not found in the global registry: {}",
            missing.iter().map(|s| format!("'{}'", s)).collect::<Vec<_>>().join(", ")
        )
    }
}

/// Get key entries for a list of key names from the global registry
///
/// Returns the key entries for the specified keys, or an error if any are missing
#[allow(dead_code)] // Public API for future use
pub fn get_key_entries(key_names: &[String]) -> Result<Vec<(String, KeyEntry)>> {
    if key_names.is_empty() {
        return Ok(Vec::new());
    }

    let registry = KeysRegistry::load()?;
    let mut entries = Vec::new();
    let mut missing = Vec::new();

    for name in key_names {
        // Try to find by name first
        if let Some(entry) = registry.keys.get(name) {
            entries.push((name.clone(), entry.clone()));
        } else {
            // Try to find by key ID
            let mut found = false;
            for (key_name, entry) in &registry.keys {
                if entry.keyid == *name {
                    entries.push((key_name.clone(), entry.clone()));
                    found = true;
                    break;
                }
            }

            if !found {
                missing.push(name.clone());
            }
        }
    }

    if !missing.is_empty() {
        anyhow::bail!(
            "The following signing keys are not found in the global registry: {}",
            missing
                .iter()
                .map(|s| format!("'{}'", s))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    Ok(entries)
}

// Add hex encoding since we need it for keyid generation
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        use std::fmt::Write;
        bytes
            .iter()
            .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
                let _ = write!(acc, "{:02x}", b);
                acc
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keyid() {
        let (_, verifying_key) = generate_keypair();
        let keyid = generate_keyid(&verifying_key);
        // Key ID is the full SHA-256 hash, base16 encoded (64 hex chars)
        assert_eq!(keyid.len(), 64);
        // Verify it's valid hex
        assert!(keyid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_key_serialization() {
        // Test that we can save and load keys using the seed
        let (sk, pk) = generate_keypair();

        // Serialize the seed (this is what we store on disk)
        let seed = sk.seed();
        let seed_bytes = seed.as_ref();
        assert_eq!(seed_bytes.len(), 32, "Seed should be 32 bytes");

        // Reconstruct the key from the seed (this is what we do when loading)
        let seed_reconstructed =
            Seed::from_slice(seed_bytes).expect("Should parse seed from bytes");
        let keypair_reconstructed = KeyPair::from_seed(seed_reconstructed);

        // The reconstructed key should produce the same public key
        assert_eq!(
            pk.as_ref(),
            keypair_reconstructed.pk.as_ref(),
            "Public keys should match"
        );
    }

    #[test]
    fn test_is_file_uri() {
        assert!(is_file_uri("file:///path/to/key"));
        assert!(!is_file_uri("pkcs11:token=YubiKey"));
        assert!(!is_file_uri("/path/to/key"));
    }

    #[test]
    fn test_is_pkcs11_uri() {
        assert!(is_pkcs11_uri("pkcs11:token=YubiKey"));
        assert!(!is_pkcs11_uri("file:///path/to/key"));
        assert!(!is_pkcs11_uri("/path/to/key"));
    }

    #[test]
    fn test_registry_serialization() {
        let mut registry = KeysRegistry::default();
        registry.keys.insert(
            "test-key".to_string(),
            KeyEntry {
                keyid: "abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
                    .to_string(),
                algorithm: "ed25519".to_string(),
                created_at: Utc::now(),
                uri: "file:///path/to/key".to_string(),
            },
        );

        let json = serde_json::to_string(&registry).unwrap();
        let parsed: KeysRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.keys.len(), 1);
        assert!(parsed.keys.contains_key("test-key"));
    }

    #[test]
    fn test_sign_and_verify_with_keypair() {
        // Generate a keypair
        let (secret_key, public_key) = generate_keypair();

        // Create a test message
        let message = b"test message to sign";

        // Sign the message
        let signature = secret_key.sign(message, None);

        // Verify the signature with the public key
        let result = public_key.verify(message, &signature);
        assert!(
            result.is_ok(),
            "Signature verification should succeed with correct public key"
        );

        // Verify that wrong message fails
        let wrong_message = b"different message";
        let result = public_key.verify(wrong_message, &signature);
        assert!(
            result.is_err(),
            "Signature verification should fail with wrong message"
        );

        // Verify that wrong public key fails
        let (_, wrong_public_key) = generate_keypair();
        let result = wrong_public_key.verify(message, &signature);
        assert!(
            result.is_err(),
            "Signature verification should fail with wrong public key"
        );
    }

    #[test]
    fn test_sign_and_verify_sha256_hash() {
        use sha2::{Digest, Sha256};

        // Generate a keypair
        let (secret_key, public_key) = generate_keypair();

        // Create a test file content and hash it
        let file_content = b"This is a test file for signing";
        let mut hasher = Sha256::new();
        hasher.update(file_content);
        let hash = hasher.finalize();

        // Sign the hash (this is what avocado does)
        let signature = secret_key.sign(hash, None);

        // Verify the signature
        let result = public_key.verify(hash, &signature);
        assert!(
            result.is_ok(),
            "Signature verification should succeed for SHA256 hash"
        );

        // Verify signature is 64 bytes
        assert_eq!(
            signature.as_ref().len(),
            64,
            "ED25519 signature should be 64 bytes"
        );
    }

    #[test]
    fn test_sign_and_verify_blake3_hash() {
        // Generate a keypair
        let (secret_key, public_key) = generate_keypair();

        // Create a test file content and hash it with BLAKE3
        let file_content = b"This is a test file for BLAKE3 signing";
        let hash = blake3::hash(file_content);

        // Sign the hash
        let signature = secret_key.sign(hash.as_bytes(), None);

        // Verify the signature
        let result = public_key.verify(hash.as_bytes(), &signature);
        assert!(
            result.is_ok(),
            "Signature verification should succeed for BLAKE3 hash"
        );
    }

    #[test]
    fn test_signature_encoding_decoding() {
        // Generate a keypair
        let (secret_key, _public_key) = generate_keypair();

        // Create and sign a message
        let message = b"test message";
        let signature = secret_key.sign(message, None);

        // Encode signature to hex (this is what avocado does)
        let signature_hex: String = signature
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();

        // Verify hex encoding
        assert_eq!(
            signature_hex.len(),
            128,
            "Hex-encoded signature should be 128 characters"
        );
        assert!(
            signature_hex.chars().all(|c| c.is_ascii_hexdigit()),
            "Signature should be valid hex"
        );

        // Decode back from hex
        let decoded_bytes: Vec<u8> = (0..signature_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&signature_hex[i..i + 2], 16).unwrap())
            .collect();

        // Verify decoding works
        assert_eq!(
            decoded_bytes.len(),
            64,
            "Decoded signature should be 64 bytes"
        );
        assert_eq!(
            decoded_bytes,
            signature.as_ref(),
            "Decoded signature should match original"
        );
    }

    #[test]
    fn test_public_key_encoding_decoding() {
        use base64::prelude::*;

        // Generate a keypair
        let (_secret_key, public_key) = generate_keypair();

        // Encode public key to base64 (this is what avocado does when saving .pub files)
        let public_key_b64 = BASE64_STANDARD.encode(public_key.as_ref());

        // Verify encoding
        assert_eq!(
            public_key.as_ref().len(),
            32,
            "ED25519 public key should be 32 bytes"
        );

        // Decode back from base64
        let decoded_bytes = BASE64_STANDARD.decode(&public_key_b64).unwrap();

        // Verify decoding works
        assert_eq!(
            decoded_bytes.len(),
            32,
            "Decoded public key should be 32 bytes"
        );
        assert_eq!(
            decoded_bytes,
            public_key.as_ref(),
            "Decoded public key should match original"
        );

        // Reconstruct public key from decoded bytes
        let reconstructed_key = PublicKey::from_slice(&decoded_bytes).unwrap();

        // Verify reconstructed key matches original
        assert_eq!(
            reconstructed_key.as_ref(),
            public_key.as_ref(),
            "Reconstructed key should match original"
        );
    }

    #[test]
    fn test_keyid_is_hash_of_public_key() {
        use sha2::{Digest, Sha256};

        // Generate a keypair
        let (_secret_key, public_key) = generate_keypair();

        // Generate keyid using the function
        let keyid = generate_keyid(&public_key);

        // Manually compute the hash
        let mut hasher = Sha256::new();
        hasher.update(public_key.as_ref());
        let manual_hash = hasher.finalize();
        let manual_keyid = hex::encode(&manual_hash);

        // Verify they match
        assert_eq!(
            keyid, manual_keyid,
            "keyid should be SHA256 hash of public key"
        );
        assert_eq!(keyid.len(), 64, "keyid should be 64 hex characters");
    }
}
