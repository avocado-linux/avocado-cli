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
        let (_, profile) = config.resolve_profile(self.profile.as_deref())?;
        let client = ConnectClient::from_profile(profile)?;

        let me = client.get_me_full().await?;

        if me.organizations.is_empty() {
            print_info("No organizations found.", OutputLevel::Normal);
            return Ok(());
        }

        let max_slug = me
            .organizations
            .iter()
            .map(|o| o.slug.len())
            .max()
            .unwrap_or(0);
        let max_name = me
            .organizations
            .iter()
            .map(|o| o.name.len())
            .max()
            .unwrap_or(0);

        println!(
            "{:<slug_w$}  {:<name_w$}  ROLE",
            "SLUG",
            "NAME",
            slug_w = max_slug,
            name_w = max_name,
        );
        for org in &me.organizations {
            println!(
                "{:<slug_w$}  {:<name_w$}  {}",
                org.slug,
                org.name,
                org.role,
                slug_w = max_slug,
                name_w = max_name,
            );
        }

        Ok(())
    }
}
