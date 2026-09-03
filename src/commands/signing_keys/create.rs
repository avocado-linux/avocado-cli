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
    /// Key algorithm: ed25519 (default, the cli's own signer) or rsa2048/rsa4096
    /// (a PEM key + self-signed certificate for boot-FIT signing, generated
    /// with the host's `openssl`).
    pub algorithm: String,
}

impl SigningKeysCreateCommand {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: Option<String>,
        uri: Option<String>,
        pkcs11_device: Option<String>,
        token: Option<String>,
        key_label: Option<String>,
        generate: bool,
        auth: String,
        algorithm: String,
    ) -> Self {
        Self {
            name,
            uri,
            pkcs11_device,
            token,
            key_label,
            generate,
            auth,
            algorithm,
        }
    }

    /// RSA: `openssl req -x509 -newkey rsa:N` into a temp dir, then the same
    /// storage path as `import`. Returns (keyid, uri).
    fn create_rsa_pem(&self) -> Result<(String, String)> {
        use crate::utils::signing_keys::{keyid_for_pem_cert, save_pem_keypair};
        let bits = self.algorithm.trim_start_matches("rsa");
        let subject = format!("/CN={}", self.name.as_deref().unwrap_or("avocado-fit"));
        let dir = tempfile::Builder::new()
            .prefix("avocado-rsa-key-")
            .tempdir()?;
        let key = dir.path().join("key.pem");
        let cert = dir.path().join("cert.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req", "-batch", "-new", "-x509", "-sha256", "-nodes", "-days", "3650",
                "-newkey", &format!("rsa:{bits}"), "-subj", &subject,
            ])
            .arg("-keyout").arg(&key)
            .arg("-out").arg(&cert)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| anyhow::anyhow!("could not run openssl ({e}); install it, or generate the key elsewhere and use `avocado signing-keys import`"))?;
        if !status.success() {
            anyhow::bail!("openssl failed to generate the {} key", self.algorithm);
        }
        let key_pem = std::fs::read(&key)?;
        let cert_pem = std::fs::read_to_string(&cert)?;
        let keyid = keyid_for_pem_cert(&cert_pem)?;
        let base = save_pem_keypair(&keyid, &key_pem, cert_pem.as_bytes())?;
        Ok((keyid, path_to_file_uri(&base)))
    }

    pub fn execute(&self) -> Result<()> {
        use crate::utils::pkcs11_devices::{
            build_pkcs11_uri, find_existing_key, generate_keypair as generate_pkcs11_keypair,
            get_device_auth, init_pkcs11_session, DeviceType, KeyAlgorithm, Pkcs11AuthMethod,
        };
        use std::str::FromStr;

        let mut registry = KeysRegistry::load()?;

        // Before generating anything: a duplicate name is fatal, and key
        // generation (RSA via openssl, or a hardware key in the device) is
        // expensive and leaves files behind. When no name is given it defaults
        // to the keyid, which isn't known until after generation; add_key still
        // catches that case.
        if let Some(name) = &self.name {
            if registry.get_key(name).is_some() {
                anyhow::bail!("A key with name '{name}' already exists");
            }
        }

        let (keyid, uri, algorithm, key_type) = if crate::utils::signing_keys::is_secret_algorithm(
            &self.algorithm,
        ) {
            if self.pkcs11_device.is_some() || self.uri.is_some() {
                anyhow::bail!(
                    "--algorithm {} is a file-backed secret for now; it cannot be combined with PKCS#11 options",
                    self.algorithm
                );
            }
            // A 256-bit random master. Per-device material is derived from it
            // (see derive_var_recovery_passphrase); the master itself never
            // leaves this machine's registry.
            let secret: [u8; crate::utils::signing_keys::SECRET_KEY_BYTES] = {
                use rand::RngExt;
                rand::rng().random()
            };
            let keyid = crate::utils::signing_keys::keyid_for_secret(&secret);
            let uri = crate::utils::signing_keys::save_secret_key(&keyid, &secret)?;
            (keyid, uri, self.algorithm.clone(), "file".to_string())
        } else if crate::utils::signing_keys::is_pem_algorithm(&self.algorithm) {
            if self.pkcs11_device.is_some() || self.uri.is_some() {
                anyhow::bail!(
                    "--algorithm {} is file-based; it cannot be combined with PKCS#11 options",
                    self.algorithm
                );
            }
            let (keyid, uri) = self.create_rsa_pem()?;
            (keyid, uri, self.algorithm.clone(), "file".to_string())
        } else if self.algorithm != "ed25519" {
            anyhow::bail!(
                "--algorithm {}: expected ed25519, rsa2048, rsa4096 or hmac-sha256",
                self.algorithm
            );
        } else if let Some(device_type_str) = &self.pkcs11_device {
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
