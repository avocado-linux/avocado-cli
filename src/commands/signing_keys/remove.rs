//! Remove signing key command.

use anyhow::Result;

use crate::utils::signing_keys::{delete_key_files, is_file_uri, KeysRegistry};

/// Command to remove a signing key from the registry and filesystem
pub struct SigningKeysRemoveCommand {
    /// Name of the key to remove
    pub name: String,
}

impl SigningKeysRemoveCommand {
    pub fn new(name: String) -> Self {
        Self { name }
    }

    pub fn execute(&self) -> Result<()> {
        let mut registry = KeysRegistry::load()?;

        // Get the key entry before removing
        let entry = registry.get_key(&self.name).cloned();

        if entry.is_none() {
            anyhow::bail!("No signing key found with name '{}'", self.name);
        }

        let entry = entry.unwrap();

        // Remove from registry
        registry.remove_key(&self.name)?;
        registry.save()?;

        // If it's a file-based key, delete the key files
        if is_file_uri(&entry.uri) {
            match delete_key_files(&entry.keyid) {
                Ok(()) => {
                    println!("Removed signing key '{}'", self.name);
                    println!("  Key ID:  {}", entry.keyid);
                    println!("  Deleted key files from disk");
                }
                Err(e) => {
                    // Key was removed from registry, but file deletion failed
                    println!("Removed signing key '{}' from registry", self.name);
                    println!("  Warning: Failed to delete key files: {}", e);
                }
            }
        } else {
            // PKCS#11 key - just remove from registry
            println!("Removed signing key '{}'", self.name);
            println!("  Key ID: {}", entry.keyid);
            println!("  Note: PKCS#11 key reference removed (hardware key unchanged)");
        }

        Ok(())
    }
}
