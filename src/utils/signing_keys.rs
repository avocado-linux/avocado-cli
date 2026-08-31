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
    /// Unique key identifier: SHA-256 of the public key, or of the X.509
    /// certificate's DER for RSA (`rsa2048`/`rsa4096`) entries
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
            anyhow::bail!("A key with name '{name}' already exists");
        }
        self.keys.insert(name, entry);
        Ok(())
    }

    /// Remove a key entry from the registry
    pub fn remove_key(&mut self, name: &str) -> Result<KeyEntry> {
        self.keys
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("No key found with name '{name}'"))
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

/// Name used for the auto-generated development signing key.
const DEV_SIGNING_KEY_NAME: &str = "dev-signing-key";

/// Ensure a development signing key exists, creating one if necessary.
///
/// Checks the global registry for a key named "dev-signing-key". If it doesn't
/// exist, generates a new ed25519 keypair and registers it. Returns the key name.
pub fn ensure_dev_signing_key() -> Result<String> {
    let mut registry = KeysRegistry::load()?;

    if registry.get_key(DEV_SIGNING_KEY_NAME).is_some() {
        return Ok(DEV_SIGNING_KEY_NAME.to_string());
    }

    let (signing_key, verifying_key) = generate_keypair();
    let keyid = generate_keyid(&verifying_key);
    let key_path = save_keypair(&keyid, &signing_key, &verifying_key)?;
    let uri = path_to_file_uri(&key_path);

    let entry = KeyEntry {
        keyid,
        algorithm: "ed25519".to_string(),
        created_at: chrono::Utc::now(),
        uri,
    };

    registry.add_key(DEV_SIGNING_KEY_NAME.to_string(), entry)?;
    registry.save()?;

    println!(
        "Auto-generated development signing key '{DEV_SIGNING_KEY_NAME}'. \
         This key is for local development only."
    );

    Ok(DEV_SIGNING_KEY_NAME.to_string())
}

/// Delete key files from disk
pub fn delete_key_files(keyid: &str) -> Result<()> {
    delete_key_files_at(&get_key_file_path(keyid)?)
}

/// Every file a file-backed entry may own: ed25519 `.key`/`.pub`, RSA
/// `.key`/`.crt`, hmac-sha256 `.secret`.
pub const KEY_FILE_EXTENSIONS: &[&str] = &["key", "pub", "crt", "secret"];

fn delete_key_files_at(base_path: &Path) -> Result<()> {
    for ext in KEY_FILE_EXTENSIONS {
        let path = base_path.with_extension(ext);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete key file: {}", path.display()))?;
        }
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
            missing.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ")
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
                .map(|s| format!("'{s}'"))
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
                let _ = write!(acc, "{b:02x}");
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
            .map(|b| format!("{b:02x}"))
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

// ---------------------------------------------------------------------------
// PEM keys for external signers (U-Boot mkimage FIT signing)
// ---------------------------------------------------------------------------

/// Algorithms whose material is a PEM RSA private key + X.509 certificate,
/// consumed by external tools (`mkimage -k <dir>` expects `<hint>.key` and
/// `<hint>.crt`) rather than by the cli's own ed25519 signer.
pub fn is_pem_algorithm(algorithm: &str) -> bool {
    matches!(algorithm, "rsa2048" | "rsa4096")
}

/// Key ID for a certificate: SHA-256 of its DER (the PEM body decoded), so the
/// same certificate imported twice gets the same id regardless of line
/// wrapping or trailing whitespace.
pub fn keyid_for_pem_cert(cert_pem: &str) -> Result<String> {
    // Insist on the CERTIFICATE label: a private key or any other PEM block
    // would also base64-decode, and a key id silently derived from the wrong
    // file only surfaces later as a FIT that does not verify.
    let mut lines = cert_pem.lines().map(str::trim).filter(|l| !l.is_empty());
    match lines.next() {
        Some("-----BEGIN CERTIFICATE-----") => {}
        Some(other) => anyhow::bail!(
            "expected a PEM X.509 certificate (-----BEGIN CERTIFICATE-----), found {other:?}"
        ),
        None => anyhow::bail!("certificate file is empty"),
    }
    let body: String = lines.take_while(|l| !l.starts_with("-----END")).collect();
    let der = BASE64_STANDARD
        .decode(body.as_bytes())
        .context("certificate is not a PEM-encoded X.509 certificate")?;
    if der.is_empty() {
        anyhow::bail!("certificate PEM body is empty");
    }
    let mut hasher = Sha256::new();
    hasher.update(&der);
    Ok(hex::encode(&hasher.finalize()))
}

/// Store a PEM private key and certificate under the registry as
/// `<keyid>.key` (0600) and `<keyid>.crt`. Returns the base path the registry
/// URI points at.
pub fn save_pem_keypair(keyid: &str, key_pem: &[u8], cert_pem: &[u8]) -> Result<PathBuf> {
    if !key_pem.starts_with(b"-----BEGIN") {
        anyhow::bail!("private key is not PEM (expected -----BEGIN ... PRIVATE KEY-----)");
    }
    let keys_dir = get_signing_keys_dir()?;
    fs::create_dir_all(&keys_dir).with_context(|| {
        format!(
            "Failed to create signing keys directory: {}",
            keys_dir.display()
        )
    })?;
    let base_path = get_key_file_path(keyid)?;
    let key_path = base_path.with_extension("key");
    let cert_path = base_path.with_extension("crt");
    // Create the key file 0600 from the start rather than tightening after the
    // write: with a permissive umask the bytes would otherwise be readable in
    // the window between the two. Never overwrite: the key id is the
    // certificate's, so an existing file is another entry's private key.
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&key_path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(key_pem)
        })
        .with_context(|| format!("Failed to write private key: {}", key_path.display()))?;
    fs::write(&cert_path, cert_pem)
        .with_context(|| format!("Failed to write certificate: {}", cert_path.display()))?;
    Ok(base_path)
}

/// Check that `key` is the private key of `cert` and return the RSA modulus
/// size in bits. Uses the host `openssl` that `signing-keys create` already
/// needs: the FIT is signed with `.key` while U-Boot gets `.crt`, and nothing
/// downstream compares the two, so a mismatch here means a board that does
/// not boot with no build-time error.
pub fn rsa_pem_pair_bits(key: &Path, cert: &Path) -> Result<u32> {
    fn openssl(args: &[&std::ffi::OsStr]) -> Result<String> {
        let out = std::process::Command::new("openssl")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("could not run openssl ({e}); it is needed to check the key against the certificate"))?;
        if !out.status.success() {
            anyhow::bail!(
                "openssl {}: {}",
                args[0].to_string_lossy(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
    let from_key = openssl(&[
        "pkey".as_ref(),
        "-in".as_ref(),
        key.as_os_str(),
        "-pubout".as_ref(),
    ])?;
    let from_cert = openssl(&[
        "x509".as_ref(),
        "-in".as_ref(),
        cert.as_os_str(),
        "-noout".as_ref(),
        "-pubkey".as_ref(),
    ])?;
    if from_key.trim() != from_cert.trim() {
        anyhow::bail!(
            "{} is not the private key of {}",
            key.display(),
            cert.display()
        );
    }
    let text = openssl(&[
        "x509".as_ref(),
        "-in".as_ref(),
        cert.as_os_str(),
        "-noout".as_ref(),
        "-text".as_ref(),
    ])?;
    if !text.contains("rsaEncryption") {
        anyhow::bail!("{} is not an RSA certificate", cert.display());
    }
    text.split("Public-Key: (")
        .nth(1)
        .and_then(|rest| rest.split(" bit").next())
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("could not read the RSA key size from {}", cert.display()))
}

/// The PEM files behind a registry entry, for handing to an external signer.
///
/// Resolves `name` as a registry name or a key id. Refuses anything that is
/// not a file-backed PEM key: the ed25519 seeds are for the cli's own signer,
/// and a PKCS#11 URI cannot be handed to `mkimage` as a directory.
pub fn pem_key_files(name: &str) -> Result<(PathBuf, PathBuf, String)> {
    let entries = get_key_entries(std::slice::from_ref(&name.to_string()))?;
    let (registry_name, entry) = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("signing key '{name}' is not in the registry"))?;
    if !is_pem_algorithm(&entry.algorithm) {
        anyhow::bail!(
            "signing key '{registry_name}' is {} - FIT signing needs an RSA PEM key \
             (rsa2048/rsa4096). Import one with `avocado signing-keys import`.",
            entry.algorithm
        );
    }
    if !is_file_uri(&entry.uri) {
        anyhow::bail!(
            "signing key '{registry_name}' is {} - FIT signing needs a file-backed PEM key; \
             mkimage cannot use a PKCS#11 URI",
            entry.uri
        );
    }
    let base = PathBuf::from(entry.uri.trim_start_matches("file://"));
    let key = base.with_extension("key");
    let cert = base.with_extension("crt");
    for f in [&key, &cert] {
        if !f.is_file() {
            anyhow::bail!(
                "signing key '{registry_name}' is registered but {} is missing",
                f.display()
            );
        }
    }
    Ok((key, cert, entry.algorithm))
}

#[cfg(test)]
mod pem_tests {
    use super::*;

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nAAECAwQFBgc=\n-----END CERTIFICATE-----\n";

    #[test]
    fn a_cert_keyid_is_the_sha256_of_its_der() {
        let id = keyid_for_pem_cert(CERT).unwrap();
        let mut h = Sha256::new();
        h.update([0u8, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(id, hex::encode(&h.finalize()));
        // Same certificate, different wrapping and whitespace: same id.
        let rewrapped =
            "-----BEGIN CERTIFICATE-----\nAAEC\n AwQF\nBgc= \n-----END CERTIFICATE-----";
        assert_eq!(keyid_for_pem_cert(rewrapped).unwrap(), id);
    }

    #[test]
    fn a_non_certificate_is_refused() {
        assert!(keyid_for_pem_cert("hello").is_err());
        assert!(
            keyid_for_pem_cert("-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----").is_err()
        );
        // A PEM block that is not a certificate - the private key handed in by
        // mistake - is refused by its label, not accepted because it decodes.
        assert!(keyid_for_pem_cert(
            "-----BEGIN PRIVATE KEY-----\nAAECAwQFBgc=\n-----END PRIVATE KEY-----\n"
        )
        .is_err());
    }

    #[test]
    fn removing_an_entry_deletes_its_certificate_too() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc");
        for ext in KEY_FILE_EXTENSIONS {
            fs::write(base.with_extension(ext), b"x").unwrap();
        }
        delete_key_files_at(&base).unwrap();
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
        // Nothing to delete is not an error.
        delete_key_files_at(&base).unwrap();
    }

    #[test]
    fn a_second_pair_cannot_overwrite_a_stored_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("k.key");
        fs::write(&key, b"old").unwrap();
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        assert!(opts.open(&key).is_err());
        assert_eq!(fs::read(&key).unwrap(), b"old");
    }

    /// Needs the host openssl, like the command under test; skipped without it.
    #[test]
    fn a_key_is_checked_against_its_certificate() {
        if std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let gen = |n: &str, bits: &str| {
            let (k, c) = (
                dir.path().join(format!("{n}.key")),
                dir.path().join(format!("{n}.crt")),
            );
            let ok = std::process::Command::new("openssl")
                .args([
                    "req", "-batch", "-new", "-x509", "-nodes", "-days", "1", "-subj", "/CN=t",
                ])
                .arg("-newkey")
                .arg(format!("rsa:{bits}"))
                .arg("-keyout")
                .arg(&k)
                .arg("-out")
                .arg(&c)
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok);
            (k, c)
        };
        let (k1, c1) = gen("a", "2048");
        let (k2, c2) = gen("b", "2048");
        assert_eq!(rsa_pem_pair_bits(&k1, &c1).unwrap(), 2048);
        // The wrong key for the certificate is refused, not registered.
        assert!(rsa_pem_pair_bits(&k1, &c2).is_err());
        assert!(rsa_pem_pair_bits(&k2, &c1).is_err());
    }

    #[test]
    fn only_rsa_is_a_pem_algorithm() {
        assert!(is_pem_algorithm("rsa2048"));
        assert!(is_pem_algorithm("rsa4096"));
        assert!(!is_pem_algorithm("ed25519"));
        assert!(!is_pem_algorithm("ecdsa-p256"));
    }
}
// ---------------------------------------------------------------------------
// Secret (symmetric) keys: `--algorithm hmac-sha256`. A master the operator
// holds; per-device material is *derived* from it and never stored.
// ---------------------------------------------------------------------------

/// The one secret-key algorithm the registry knows.
pub const SECRET_ALGORITHM: &str = "hmac-sha256";
pub const SECRET_KEY_BYTES: usize = 32;
/// Domain separator so a var recovery passphrase can never collide with any
/// other derivation from the same master.
const VAR_RECOVERY_INFO: &[u8] = b"avocado-var-recovery\0";

pub fn is_secret_algorithm(algorithm: &str) -> bool {
    algorithm == SECRET_ALGORITHM
}

/// keyid of a secret: SHA-256 over a fixed prefix and the secret, so the id is
/// stable, never equal to a hash anyone else computes over the bare bytes, and
/// leaks nothing about a 256-bit random master.
pub fn keyid_for_secret(secret: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"avocado-secret-keyid\0");
    h.update(secret);
    hex::encode(&h.finalize()[..])
}

/// Store a secret master as `<keyid>.secret`, mode 0600, and return the file URI.
pub fn save_secret_key(keyid: &str, secret: &[u8]) -> Result<String> {
    use std::io::Write;
    let path = get_key_file_path(keyid)?.with_extension("secret");
    // The ed25519 and PEM paths create this first; without it the very first
    // `signing-keys create --algorithm hmac-sha256` on a clean install fails
    // with ENOENT, since there is no registry directory yet.
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    f.write_all(secret)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path_to_file_uri(&path))
}

/// The master bytes of a registry secret. File-backed only for now; a
/// `pkcs11:` secret is refused with a pointer to what is missing rather than
/// pretending (the HSM would compute the HMAC itself, see the recovery-key brief).
pub fn secret_key_bytes(name: &str) -> Result<Vec<u8>> {
    let entries = get_key_entries(std::slice::from_ref(&name.to_string()))?;
    let (registry_name, entry) = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("key '{name}' is not in the registry"))?;
    if !is_secret_algorithm(&entry.algorithm) {
        anyhow::bail!(
            "key '{registry_name}' is {} - a var recovery master must be a secret key \
             (`avocado signing-keys create {registry_name} --algorithm {SECRET_ALGORITHM}`)",
            entry.algorithm
        );
    }
    if !is_file_uri(&entry.uri) {
        anyhow::bail!(
            "key '{registry_name}' is {} - deriving a recovery passphrase from a PKCS#11 \
             secret is not supported yet (the token would have to compute the HMAC)",
            entry.uri
        );
    }
    let path = PathBuf::from(entry.uri.trim_start_matches("file://"));
    let bytes = fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() >= 16,
        "secret '{registry_name}' at {} is too short to be a master key",
        path.display()
    );
    Ok(bytes)
}

/// HMAC-SHA256 (RFC 2104) over sha2 - no extra dependency for ten lines.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key)[..]);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new()
        .chain_update(&ipad)
        .chain_update(msg)
        .finalize();
    let mac = Sha256::new()
        .chain_update(&opad)
        .chain_update(&inner[..])
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac[..]);
    out
}

/// The /var recovery passphrase of one device: HMAC(master, info || SoC UID).
/// The device only ever sees this value; recovering any unit later is the same
/// computation from its UID. 32 raw bytes, fed to the device verbatim.
pub fn derive_var_recovery_passphrase(master: &[u8], soc_uid: &str) -> [u8; 32] {
    let uid = soc_uid.trim();
    let mut msg = Vec::with_capacity(VAR_RECOVERY_INFO.len() + uid.len());
    msg.extend_from_slice(VAR_RECOVERY_INFO);
    msg.extend_from_slice(uid.as_bytes());
    hmac_sha256(master, &msg)
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_handles_a_key_longer_than_the_block() {
        // RFC 4231 case 6: 131-byte key.
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex::encode(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn recovery_passphrase_is_per_device_and_stable() {
        let master = [7u8; 32];
        let a = derive_var_recovery_passphrase(&master, "0a16040100000100");
        let b = derive_var_recovery_passphrase(&master, "0a16040100000100\n");
        let c = derive_var_recovery_passphrase(&master, "0a16040100000101");
        assert_eq!(a, b, "surrounding whitespace on the UID must not matter");
        assert_ne!(a, c, "a different UID yields a different passphrase");
        assert_ne!(
            a,
            hmac_sha256(&master, b"0a16040100000100"),
            "domain-separated"
        );
    }

    #[test]
    fn secret_keyid_is_not_a_bare_hash_of_the_secret() {
        use sha2::{Digest, Sha256};
        let secret = [1u8; 32];
        assert_ne!(
            keyid_for_secret(&secret),
            hex::encode(&Sha256::digest(secret)[..])
        );
        assert_eq!(keyid_for_secret(&secret).len(), 64);
    }
}
