//! Remove signing key command.

use anyhow::Result;
use std::io::{self, Write};

use crate::utils::pkcs11_devices::delete_pkcs11_key;
use crate::utils::signing_keys::{delete_key_files, is_file_uri, KeysRegistry};

/// Command to remove a signing key from the registry and filesystem
pub struct SigningKeysRemoveCommand {
    /// Name of the key to remove
    pub name: String,
    /// Delete hardware key from device
    pub delete: bool,
}

impl SigningKeysRemoveCommand {
    pub fn new(name: String, delete: bool) -> Self {
        Self { name, delete }
    }

    /// Prompt user for confirmation (returns true if user confirms)
    fn confirm_deletion(key_name: &str, key_type: &str) -> Result<bool> {
        println!(
            "⚠️  WARNING: This will PERMANENTLY delete the {} key '{}' from the hardware device.",
            key_type, key_name
        );
        print!("This action cannot be undone. Continue? [y/N]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
    }

    pub fn execute(&self) -> Result<()> {
        let mut registry = KeysRegistry::load()?;

        // Try to find by name first, then by key ID
        let (key_name, entry) = if let Some(entry) = registry.get_key(&self.name).cloned() {
            // Found by name
            (self.name.clone(), entry)
        } else {
            // Try to find by key ID
            let mut found = None;
            for (name, entry) in &registry.keys {
                if entry.keyid == self.name {
                    found = Some((name.clone(), entry.clone()));
                    break;
                }
            }

            if let Some((name, entry)) = found {
                (name, entry)
            } else {
                anyhow::bail!("No signing key found with name or key ID '{}'", self.name);
            }
        };

        // Remove from registry
        registry.remove_key(&key_name)?;
        registry.save()?;

        // If it's a file-based key, delete the key files
        if is_file_uri(&entry.uri) {
            // File-based key - always delete files
            if self.delete {
                // Prompt for confirmation
                if !Self::confirm_deletion(&key_name, "file")? {
                    println!("Deletion cancelled.");
                    // Re-add to registry since we removed it earlier
                    registry.add_key(key_name.clone(), entry)?;
                    registry.save()?;
                    return Ok(());
                }
            }

            match delete_key_files(&entry.keyid) {
                Ok(()) => {
                    println!("Removed signing key '{}'", key_name);
                    println!("  Key ID:  {}", entry.keyid);
                    println!("  Deleted key files from disk");
                }
                Err(e) => {
                    // Key was removed from registry, but file deletion failed
                    println!("Removed signing key '{}' from registry", key_name);
                    println!("  Warning: Failed to delete key files: {}", e);
                }
            }
        } else {
            // PKCS#11 key
            if self.delete {
                // Prompt for confirmation
                if !Self::confirm_deletion(&key_name, "hardware")? {
                    println!("Deletion cancelled.");
                    // Re-add to registry since we removed it earlier
                    registry.add_key(key_name.clone(), entry)?;
                    registry.save()?;
                    return Ok(());
                }

                // Attempt to delete from hardware
                match delete_pkcs11_key(&entry.uri) {
                    Ok(()) => {
                        println!("Removed signing key '{}'", key_name);
                        println!("  Key ID: {}", entry.keyid);
                        println!("  ✓ Deleted from registry");
                        println!("  ✓ Deleted from hardware device");
                    }
                    Err(e) => {
                        println!("Removed signing key '{}' from registry", key_name);
                        println!("  Key ID: {}", entry.keyid);
                        println!("  ⚠️  Warning: Failed to delete from hardware: {}", e);
                        println!(
                            "  You may need to delete it manually using device-specific tools."
                        );
                    }
                }
            } else {
                // Just remove from registry
                println!("Removed signing key '{}'", key_name);
                println!("  Key ID: {}", entry.keyid);
                println!("  Note: PKCS#11 key reference removed (hardware key unchanged)");
                println!("  Tip: Use --delete to permanently delete the hardware key");
            }
        }

        Ok(())
    }
}
