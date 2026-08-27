//! Import an existing PEM key + certificate (RSA, for boot-FIT signing).

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::PathBuf;

use crate::utils::signing_keys::{
    is_pem_algorithm, keyid_for_pem_cert, path_to_file_uri, save_pem_keypair, KeyEntry,
    KeysRegistry,
};

/// `avocado signing-keys import <name> --key FIT.key --cert FIT.crt`
///
/// Registers an RSA private key / X.509 certificate pair under the registry so
/// a runtime can name it in `signing.fit_key`. The key id is the SHA-256 of the
/// certificate's DER.
pub struct SigningKeysImportCommand {
    pub name: String,
    pub key: PathBuf,
    pub cert: PathBuf,
    pub algorithm: String,
}

impl SigningKeysImportCommand {
    pub fn new(name: String, key: PathBuf, cert: PathBuf, algorithm: String) -> Self {
        Self {
            name,
            key,
            cert,
            algorithm,
        }
    }

    pub fn execute(&self) -> Result<()> {
        if !is_pem_algorithm(&self.algorithm) {
            anyhow::bail!(
                "--algorithm {}: import handles RSA PEM keys only (rsa2048, rsa4096)",
                self.algorithm
            );
        }
        let key_pem = std::fs::read(&self.key)
            .with_context(|| format!("Failed to read {}", self.key.display()))?;
        let cert_pem = std::fs::read_to_string(&self.cert)
            .with_context(|| format!("Failed to read {}", self.cert.display()))?;
        let keyid = keyid_for_pem_cert(&cert_pem)?;

        let mut registry = KeysRegistry::load()?;
        if registry.get_key(&self.name).is_some() {
            anyhow::bail!("A key with name '{}' already exists", self.name);
        }
        let base_path = save_pem_keypair(&keyid, &key_pem, cert_pem.as_bytes())?;
        registry.add_key(
            self.name.clone(),
            KeyEntry {
                keyid: keyid.clone(),
                algorithm: self.algorithm.clone(),
                created_at: Utc::now(),
                uri: path_to_file_uri(&base_path),
            },
        )?;
        registry.save()?;

        println!("Imported signing key:");
        println!("  Name:      {}", self.name);
        println!("  Key ID:    {keyid}");
        println!("  Algorithm: {}", self.algorithm);
        println!(
            "  Files:     {}.key / {}.crt",
            base_path.display(),
            base_path.display()
        );
        println!(
            "Use it with: runtimes.<name>.signing.fit_key: {}",
            self.name
        );
        Ok(())
    }
}
