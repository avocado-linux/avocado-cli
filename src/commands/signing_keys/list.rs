//! List signing keys command.

use anyhow::Result;

use crate::utils::pkcs11_devices::parse_pkcs11_uri;
use crate::utils::signing_keys::{is_file_uri, is_pkcs11_uri, KeysRegistry};

/// Command to list all registered signing keys
pub struct SigningKeysListCommand;

impl SigningKeysListCommand {
    pub fn new() -> Self {
        Self
    }

    pub fn execute(&self) -> Result<()> {
        let registry = KeysRegistry::load()?;

        if registry.keys.is_empty() {
            println!("No signing keys registered.");
            println!();
            println!("Create a new key with: avocado signing-keys create [NAME]");
            return Ok(());
        }

        println!("Registered signing keys:");
        println!();

        // Sort keys by name for consistent output
        let mut keys: Vec<_> = registry.keys.iter().collect();
        keys.sort_by_key(|(name, _)| name.as_str());

        for (name, entry) in keys {
            let key_type = if is_file_uri(&entry.uri) {
                "file".to_string()
            } else if is_pkcs11_uri(&entry.uri) {
                // Parse the URI to determine device type from token label
                if let Ok((token_label, _)) = parse_pkcs11_uri(&entry.uri) {
                    let token_lower = token_label.to_lowercase();
                    if token_lower.contains("tpm") || token_lower == "avocado" {
                        "tpm".to_string()
                    } else if token_lower.contains("yubi") || token_lower.contains("piv") {
                        "yubikey".to_string()
                    } else {
                        "pkcs11".to_string()
                    }
                } else {
                    "pkcs11".to_string()
                }
            } else {
                "unknown".to_string()
            };

            println!("  {name}");
            println!("    Key ID:    {}", entry.keyid);
            println!("    Algorithm: {}", entry.algorithm);
            println!("    Type:      {key_type}");
            println!(
                "    Created:   {}",
                entry.created_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            println!();
        }

        Ok(())
    }
}

impl Default for SigningKeysListCommand {
    fn default() -> Self {
        Self::new()
    }
}
