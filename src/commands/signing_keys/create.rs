//! Create signing key command.

use anyhow::Result;
use chrono::Utc;

use crate::utils::signing_keys::{
    generate_keyid, generate_keypair, get_signing_keys_dir, is_pkcs11_uri, path_to_file_uri,
    save_keypair, KeyEntry, KeysRegistry,
};

/// Command to create a new signing key or register an external key
pub struct SigningKeysCreateCommand {
    /// Optional name for the key (defaults to keyid if not provided)
    pub name: Option<String>,
    /// Optional PKCS#11 URI for hardware-backed keys
    pub uri: Option<String>,
    /// Hardware device type (tpm, yubikey, auto)
    pub pkcs11_device: Option<String>,
    /// PKCS#11 token label
    pub token: Option<String>,
    /// Label of existing key to reference in the device
    pub key_label: Option<String>,
    /// Generate a new key in the device
    pub generate: bool,
    /// Authentication method for PKCS#11 device
    pub auth: String,
}

impl SigningKeysCreateCommand {
    pub fn new(
        name: Option<String>,
        uri: Option<String>,
        pkcs11_device: Option<String>,
        token: Option<String>,
        key_label: Option<String>,
        generate: bool,
        auth: String,
    ) -> Self {
        Self {
            name,
            uri,
            pkcs11_device,
            token,
            key_label,
            generate,
            auth,
        }
    }

    pub fn execute(&self) -> Result<()> {
        use crate::utils::pkcs11_devices::{
            build_pkcs11_uri, find_existing_key, generate_keypair as generate_pkcs11_keypair,
            get_device_auth, init_pkcs11_session, DeviceType, KeyAlgorithm, Pkcs11AuthMethod,
        };
        use std::str::FromStr;

        let mut registry = KeysRegistry::load()?;

        let (keyid, uri, algorithm, key_type) = if let Some(device_type_str) = &self.pkcs11_device {
            // PKCS#11 hardware device flow
            let device_type = DeviceType::from_str(device_type_str)?;
            let auth_method = Pkcs11AuthMethod::from_str(&self.auth)?;

            // Get authentication
            let auth = get_device_auth(&auth_method)?;

            // Initialize PKCS#11 and open session
            let (_pkcs11, session) =
                init_pkcs11_session(&device_type, self.token.as_deref(), &auth, &auth_method)?;

            let (_public_key_bytes, keyid, algorithm, private_key_label) = if self.generate {
                // Generate new key in device
                let label = self.name.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("--name is required when generating a hardware key")
                })?;

                // Default to ECC P-256 (most widely supported)
                let key_algorithm = KeyAlgorithm::EccP256;

                let (pub_key, kid, algo) =
                    generate_pkcs11_keypair(&session, label, &key_algorithm)?;
                (pub_key, kid, algo, label.clone())
            } else if let Some(label) = &self.key_label {
                // Reference existing key in device
                find_existing_key(&session, label)?
            } else {
                anyhow::bail!("Either --generate or --key-label is required with --pkcs11-device");
            };

            // Get token info for building URI
            let slot = session.get_session_info()?.slot_id();
            let token_info = _pkcs11.get_token_info(slot)?;
            let token_label = token_info.label();

            // Build PKCS#11 URI using the private key label (for signing operations)
            let pkcs11_uri = build_pkcs11_uri(token_label, &private_key_label);

            (
                keyid,
                pkcs11_uri,
                algorithm,
                format!("{device_type}/PKCS#11"),
            )
        } else if let Some(pkcs11_uri) = &self.uri {
            // Manual PKCS#11 URI registration (existing flow)
            if !is_pkcs11_uri(pkcs11_uri) {
                anyhow::bail!(
                    "Invalid URI: '{pkcs11_uri}'. Expected a pkcs11: URI (e.g., 'pkcs11:token=YubiKey;object=signing-key')"
                );
            }

            // For manually registered PKCS#11 keys, we generate a keyid from the URI itself
            // since we don't have direct access to the public key
            let keyid = generate_keyid_from_uri(pkcs11_uri);
            (
                keyid,
                pkcs11_uri.clone(),
                "unknown".to_string(),
                "PKCS#11".to_string(),
            )
        } else {
            // Generate a new ed25519 keypair (file-based, existing flow)
            let (signing_key, verifying_key) = generate_keypair();
            let keyid = generate_keyid(&verifying_key);

            // Save the keypair to disk
            let key_path = save_keypair(&keyid, &signing_key, &verifying_key)?;
            let uri = path_to_file_uri(&key_path);

            (keyid, uri, "ed25519".to_string(), "file".to_string())
        };

        // Determine the name (use provided name or fall back to keyid)
        let name = self.name.clone().unwrap_or_else(|| keyid.clone());

        // Check if name already exists
        if registry.get_key(&name).is_some() {
            anyhow::bail!("A key with name '{name}' already exists");
        }

        // Create the key entry
        let entry = KeyEntry {
            keyid: keyid.clone(),
            algorithm: algorithm.clone(),
            created_at: Utc::now(),
            uri: uri.clone(),
        };

        // Add to registry and save
        registry.add_key(name.clone(), entry)?;
        registry.save()?;

        // Print success message
        println!("Created signing key:");
        println!("  Name:      {name}");
        println!("  Key ID:    {keyid}");
        println!("  Algorithm: {algorithm}");
        println!("  Type:      {key_type}");

        if key_type == "file" {
            let keys_dir = get_signing_keys_dir()?;
            println!("  Location:  {}", keys_dir.display());
        } else {
            println!("  URI:       {uri}");
        }

        Ok(())
    }
}

/// Generate a keyid from a PKCS#11 URI
/// Since we can't access the actual public key from PKCS#11 without additional libraries,
/// we generate a hash from the URI itself as an identifier
fn generate_keyid_from_uri(uri: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(uri.as_bytes());
    let hash = hasher.finalize();
    hex_encode(&hash)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keyid_from_uri() {
        let uri = "pkcs11:token=YubiKey;object=signing-key";
        let keyid = generate_keyid_from_uri(uri);
        // Key ID is the full SHA-256 hash, base16 encoded (64 hex chars)
        assert_eq!(keyid.len(), 64);
        // Verify it's valid hex
        assert!(keyid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_keyid_from_uri_deterministic() {
        let uri = "pkcs11:token=YubiKey;object=signing-key";
        let keyid1 = generate_keyid_from_uri(uri);
        let keyid2 = generate_keyid_from_uri(uri);
        assert_eq!(keyid1, keyid2);
    }
}
