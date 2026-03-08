use anyhow::{Context, Result};
use base64::prelude::*;
use ed25519_compact::{KeyPair, Seed};

use crate::commands::connect::client;
use crate::utils::jcs;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::signing_keys::{get_key_file_path, KeysRegistry};

pub struct ConnectTrustRotateServerKeyCommand {
    pub key: Option<String>,
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectTrustRotateServerKeyCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = client::ConnectClient::from_profile(profile)?;

        // Check trust status to determine security level
        let status = connect.get_trust_status(&self.org).await?;

        if status.security_level < 2 {
            // Level 0/1: server-only rotation, no user signing needed
            print_info(
                &format!(
                    "Rotating server signing key for org: {} (security level {})...",
                    self.org, status.security_level
                ),
                OutputLevel::Normal,
            );

            let result = connect.rotate_server_key(&self.org).await?;

            print_success(
                &format!(
                    "\nServer signing key rotated successfully.\n  \
                     Configuration version: {}\n\n\
                     If you have provisioned device images, rebuild them to include \
                     the updated trust anchor with 'avocado connect init'.",
                    result.version
                ),
                OutputLevel::Normal,
            );
        } else {
            // Level 2: requires co-signing with user's root key
            let key_name = self.key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "At security level 2, --key is required to co-sign the key rotation.\n\
                     Usage: avocado connect trust rotate-server-key --key <key-name> --org <org>"
                )
            })?;

            let registry = KeysRegistry::load()?;
            let key_entry = registry
                .get_key(key_name)
                .ok_or_else(|| anyhow::anyhow!("Key '{}' not found in registry", key_name))?;

            let key_file = get_key_file_path(&key_entry.keyid)?;
            let private_key_b64 = std::fs::read_to_string(key_file.with_extension("key"))
                .context("Failed to read private key file")?;
            let seed_bytes = BASE64_STANDARD
                .decode(private_key_b64.trim())
                .context("Failed to decode private key")?;
            let seed = Seed::from_slice(&seed_bytes).context("Invalid seed bytes")?;
            let keypair = KeyPair::from_seed(seed);
            let public_hex = jcs::hex_encode(keypair.pk.as_ref());

            print_info(
                &format!(
                    "Proposing server key rotation for org: {} (security level 2)...",
                    self.org
                ),
                OutputLevel::Normal,
            );

            // Step 1: Propose
            let propose = connect.propose_rotate_server_key(&self.org).await?;
            print_info(
                &format!(
                    "Server prepared rotation (configuration version {}).",
                    propose.version
                ),
                OutputLevel::Normal,
            );

            // Step 2: Sign
            let pending: serde_json::Value = serde_json::from_str(&propose.pending_root_json)
                .context("Failed to parse pending root JSON")?;
            let signed_map = pending
                .get("signed")
                .context("Pending root JSON missing 'signed' field")?;

            let canonical = jcs::jcs_encode(signed_map);

            print_info(
                &format!("Signing with key: {}...", key_name),
                OutputLevel::Normal,
            );

            let signature = keypair.sk.sign(canonical.as_bytes(), None);
            let sig_hex = jcs::hex_encode(signature.as_ref());
            let tuf_key_id = jcs::compute_tuf_key_id(&public_hex);

            let sig_obj = serde_json::json!({
                "keyid": tuf_key_id,
                "sig": sig_hex
            });

            // Step 3: Commit
            print_info("Submitting signed configuration...", OutputLevel::Normal);
            let commit = connect
                .commit_rotate_server_key(&self.org, &sig_obj)
                .await?;

            print_success(
                &format!(
                    "\nServer signing key rotated successfully.\n  \
                     Configuration version: {}\n\n\
                     Existing devices will transition on next update check.\n\
                     For new manufacturing runs, rebuild device images to include the updated trust anchor.",
                    commit.version
                ),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }
}
