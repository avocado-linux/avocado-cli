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
}

impl SigningKeysCreateCommand {
    pub fn new(name: Option<String>, uri: Option<String>) -> Self {
        Self { name, uri }
    }

    pub fn execute(&self) -> Result<()> {
        let mut registry = KeysRegistry::load()?;

        let (keyid, uri, key_type) = if let Some(pkcs11_uri) = &self.uri {
            // Register an external PKCS#11 key
            if !is_pkcs11_uri(pkcs11_uri) {
                anyhow::bail!(
                    "Invalid URI: '{}'. Expected a pkcs11: URI (e.g., 'pkcs11:token=YubiKey;object=signing-key')",
                    pkcs11_uri
                );
            }

            // For PKCS#11 keys, we generate a keyid from the URI itself
            // since we don't have direct access to the public key
            let keyid = generate_keyid_from_uri(pkcs11_uri);
            (keyid, pkcs11_uri.clone(), "PKCS#11")
        } else {
            // Generate a new ed25519 keypair
            let (signing_key, verifying_key) = generate_keypair();
            let keyid = generate_keyid(&verifying_key);

            // Save the keypair to disk
            let key_path = save_keypair(&keyid, &signing_key, &verifying_key)?;
            let uri = path_to_file_uri(&key_path);

            (keyid, uri, "file")
        };

        // Determine the name (use provided name or fall back to keyid)
        let name = self.name.clone().unwrap_or_else(|| keyid.clone());

        // Check if name already exists
        if registry.get_key(&name).is_some() {
            anyhow::bail!("A key with name '{}' already exists", name);
        }

        // Create the key entry
        let entry = KeyEntry {
            keyid: keyid.clone(),
            algorithm: "ed25519".to_string(),
            created_at: Utc::now(),
            uri: uri.clone(),
        };

        // Add to registry and save
        registry.add_key(name.clone(), entry)?;
        registry.save()?;

        // Print success message
        println!("Created signing key:");
        println!("  Name:      {}", name);
        println!("  Key ID:    {}", keyid);
        println!("  Algorithm: ed25519");
        println!("  Type:      {}", key_type);

        if key_type == "file" {
            let keys_dir = get_signing_keys_dir()?;
            println!("  Location:  {}", keys_dir.display());
        } else {
            println!("  URI:       {}", uri);
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
    format!("sha256-{}", hex_encode(&hash[..8]))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keyid_from_uri() {
        let uri = "pkcs11:token=YubiKey;object=signing-key";
        let keyid = generate_keyid_from_uri(uri);
        assert!(keyid.starts_with("sha256-"));
        assert_eq!(keyid.len(), 7 + 16); // "sha256-" + 16 hex chars
    }

    #[test]
    fn test_generate_keyid_from_uri_deterministic() {
        let uri = "pkcs11:token=YubiKey;object=signing-key";
        let keyid1 = generate_keyid_from_uri(uri);
        let keyid2 = generate_keyid_from_uri(uri);
        assert_eq!(keyid1, keyid2);
    }
}
