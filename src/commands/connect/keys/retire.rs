use anyhow::{Context, Result};

use crate::commands::connect::client::{self, ConnectClient};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectKeysRetireCommand {
    pub org: String,
    pub keyid: String,
    pub profile: Option<String>,
}

impl ConnectKeysRetireCommand {
    pub async fn execute(&self) -> Result<()> {
        print_info(
            &format!("Discarding key {} in org {}...", self.keyid, self.org),
            OutputLevel::Normal,
        );

        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = ConnectClient::from_profile(profile)?;

        connect.discard_staged_key(&self.org, &self.keyid).await?;

        print_success("Staged key discarded.", OutputLevel::Normal);

        Ok(())
    }
}
