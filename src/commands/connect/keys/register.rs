use anyhow::{Context, Result};
use base64::prelude::*;

use crate::commands::connect::client::{self, ConnectClient, RegisterDelegateKeyRequest};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::signing_keys::{get_key_file_path, KeysRegistry};

pub struct ConnectKeysRegisterCommand {
    pub org: String,
    pub key_name: String,
    pub key_type: String,
    pub profile: Option<String>,
}

impl ConnectKeysRegisterCommand {
    pub async fn execute(&self) -> Result<()> {
        // 1. Load the signing key from the local registry
        let registry = KeysRegistry::load()?;
        let entry = registry
            .get_key(&self.key_name)
            .with_context(|| format!("Signing key '{}' not found. Run 'avocado signing-keys list' to see available keys.", self.key_name))?;

        // 2. Read the public key file and hex-encode it
        let public_key_hex = read_public_key_hex(&entry.keyid)?;

        print_info(
            &format!(
                "Registering {} key '{}' with org {}...",
                self.key_type, self.key_name, self.org
            ),
            OutputLevel::Normal,
        );

        // 3. Connect to the API
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = ConnectClient::from_profile(profile)?;

        // 4. Register the key
        let result = connect
            .register_delegate_key(
                &self.org,
                &RegisterDelegateKeyRequest {
                    public_key_hex,
                    key_type: Some(self.key_type.clone()),
                },
            )
            .await?;

        print_success(
            &format!(
                "Key staged (keyid: {}, type: {}). An org admin must approve before it can be used.",
                result.keyid, result.key_type
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

/// Read the public key file for a given keyid and return its hex encoding.
fn read_public_key_hex(keyid: &str) -> Result<String> {
    let pub_path = get_key_file_path(keyid)?.with_extension("pub");
    let pub_b64 = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("Failed to read public key file: {}", pub_path.display()))?;
    let pub_bytes = BASE64_STANDARD
        .decode(pub_b64.trim())
        .context("Failed to decode public key from base64")?;
    Ok(hex_encode(&pub_bytes))
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
