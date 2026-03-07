use anyhow::{Context, Result};

use crate::commands::connect::client;
use crate::utils::output::{print_info, OutputLevel};

pub struct ConnectServerKeyCommand {
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectServerKeyCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = client::ConnectClient::from_profile(profile)?;

        let key = connect.get_tuf_server_key(&self.org).await?;

        print_info(
            &format!("Server signing key for org: {}", self.org),
            OutputLevel::Normal,
        );
        print_info(&format!("  Key ID:     {}", key.keyid), OutputLevel::Normal);
        print_info(
            &format!("  Public key: {}", key.public_key_hex),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
