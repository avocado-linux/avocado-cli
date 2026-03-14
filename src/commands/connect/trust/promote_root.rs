use anyhow::{Context, Result};
use base64::prelude::*;
use ed25519_compact::{KeyPair, Seed};

use crate::commands::connect::client;
use crate::utils::jcs;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::signing_keys::{get_key_file_path, KeysRegistry};

pub struct ConnectTrustPromoteRootCommand {
    pub key: String,
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectTrustPromoteRootCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let connect = client::ConnectClient::from_profile(profile)?;

        // Load the local signing key
        let registry = KeysRegistry::load()?;
        let key_entry = registry
            .get_key(&self.key)
            .ok_or_else(|| anyhow::anyhow!("Key '{}' not found in registry", self.key))?;

        let key_file = get_key_file_path(&key_entry.keyid)?;
        let private_key_b64 = std::fs::read_to_string(key_file.with_extension("key"))
            .context("Failed to read private key file")?;
        let seed_bytes = BASE64_STANDARD
            .decode(private_key_b64.trim())
            .context("Failed to decode private key")?;
        let seed = Seed::from_slice(&seed_bytes).context("Invalid seed bytes")?;
        let keypair = KeyPair::from_seed(seed);

        // Get the public key hex
        let public_hex = jcs::hex_encode(keypair.pk.as_ref());

        print_info(
            &format!("Proposing root trust transfer for org: {}", self.org),
            OutputLevel::Normal,
        );

        // Step 1: Propose — server generates pending root.json signed by old root key
        let propose = connect.propose_promote_root(&self.org).await?;
        print_info(
            &format!(
                "Server prepared transition (configuration version {}).",
                propose.version
            ),
            OutputLevel::Normal,
        );

        // Step 2: Sign — parse pending root, extract "signed" map, JCS-canonicalize, sign
        let pending: serde_json::Value = serde_json::from_str(&propose.pending_root_json)
            .context("Failed to parse pending root JSON")?;
        let signed_map = pending
            .get("signed")
            .context("Pending root JSON missing 'signed' field")?;

        let canonical = jcs::jcs_encode(signed_map);

        print_info(
            &format!("Signing with key: {}...", self.key),
            OutputLevel::Normal,
        );

        let signature = keypair.sk.sign(canonical.as_bytes(), None);
        let sig_hex = jcs::hex_encode(signature.as_ref());
        let tuf_key_id = jcs::compute_tuf_key_id(&public_hex);

        let sig_obj = serde_json::json!({
            "keyid": tuf_key_id,
            "sig": sig_hex
        });

        // Step 3: Commit — send signature back to server
        print_info("Submitting signed configuration...", OutputLevel::Normal);
        let commit = connect.commit_promote_root(&self.org, &sig_obj).await?;

        print_success(
            &format!(
                "\nRoot trust promoted successfully.\n  \
                 Security level: {} (user-controlled root)\n  \
                 Configuration version: {}\n\n\
                 Existing devices will transition on next update check.\n\
                 For new manufacturing runs, rebuild device images to include the updated trust anchor.",
                commit.security_level, commit.version
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
