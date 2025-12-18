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
            // PKCS#11 key - just remove from registry
            println!("Removed signing key '{}'", key_name);
            println!("  Key ID: {}", entry.keyid);
            println!("  Note: PKCS#11 key reference removed (hardware key unchanged)");
        }

        Ok(())
    }
}
