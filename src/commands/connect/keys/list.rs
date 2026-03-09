use anyhow::{Context, Result};

use crate::commands::connect::client::{self, ConnectClient};
use crate::utils::output::{print_info, OutputLevel};

pub struct ConnectKeysListCommand {
    pub org: String,
    pub key_type: Option<String>,
    pub profile: Option<String>,
}

impl ConnectKeysListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = ConnectClient::from_profile(profile)?;

        let keys = connect
            .list_delegate_keys(&self.org, self.key_type.as_deref())
            .await?;

        if keys.is_empty() {
            print_info("No delegate keys found.", OutputLevel::Normal);
            return Ok(());
        }

        // Print header
        println!(
            "{:<12} {:<10} {:<10} {:<66} {:<24}",
            "TYPE", "STATUS", "USER", "KEYID", "ACTIVATED"
        );
        println!("{}", "-".repeat(122));

        for key in &keys {
            let user = key
                .user_id
                .as_deref()
                .map(|u| &u[..8.min(u.len())])
                .unwrap_or("-");
            let activated = key.activated_at.as_deref().unwrap_or("-");

            println!(
                "{:<12} {:<10} {:<10} {:<66} {:<24}",
                key.key_type, key.status, user, &key.keyid, activated
            );
        }

        println!("\n{} key(s) total", keys.len());

        Ok(())
    }
}
