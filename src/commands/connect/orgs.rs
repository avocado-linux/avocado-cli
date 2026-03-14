use anyhow::Result;

use crate::commands::connect::client::{self, ConnectClient};
use crate::utils::output::{print_info, OutputLevel};

pub struct ConnectOrgsListCommand {
    pub profile: Option<String>,
}

impl ConnectOrgsListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), None)?;
        let client = ConnectClient::from_profile(profile)?;

        let me = client.get_me_full().await?;

        if me.organizations.is_empty() {
            print_info("No organizations found.", OutputLevel::Normal);
            return Ok(());
        }

        let max_name = me
            .organizations
            .iter()
            .map(|o| o.name.len())
            .max()
            .unwrap_or(0);

        println!(
            "{:<name_w$}  {:<id_w$}  ROLE",
            "NAME",
            "ID",
            name_w = max_name,
            id_w = 36,
        );
        for org in &me.organizations {
            println!(
                "{:<name_w$}  {:<id_w$}  {}",
                org.name,
                org.id,
                org.role,
                name_w = max_name,
                id_w = 36,
            );
        }

        Ok(())
    }
}
