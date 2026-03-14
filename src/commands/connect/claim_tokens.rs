use anyhow::Result;

use crate::commands::connect::client::{
    self, ConnectClient, CreateClaimTokenParams, CreateClaimTokenRequest,
};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectClaimTokensListCommand {
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectClaimTokensListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let tokens = client.list_claim_tokens(&self.org).await?;

        if tokens.is_empty() {
            print_info(
                &format!("No claim tokens found in org '{}'.", self.org),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        let max_name = tokens
            .iter()
            .map(|t| t.name.as_deref().unwrap_or("-").len())
            .max()
            .unwrap_or(0)
            .max(4);

        println!("{:<name_w$}  ID", "NAME", name_w = max_name);
        for token in &tokens {
            println!(
                "{:<name_w$}  {}",
                token.name.as_deref().unwrap_or("-"),
                token.id,
                name_w = max_name,
            );
        }

        Ok(())
    }
}

pub struct ConnectClaimTokensCreateCommand {
    pub org: String,
    pub name: String,
    pub cohort_id: Option<String>,
    pub max_uses: Option<i64>,
    pub no_expiration: bool,
    pub profile: Option<String>,
}

impl ConnectClaimTokensCreateCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let expires_at = if self.no_expiration {
            // Far-future date to effectively disable expiration
            Some("2099-12-31T23:59:59Z".to_string())
        } else {
            None
        };

        let req = CreateClaimTokenRequest {
            claim_token: CreateClaimTokenParams {
                name: self.name.clone(),
                cohort_id: self.cohort_id.clone(),
                max_uses: self.max_uses,
                expires_at,
            },
        };

        let token = client.create_claim_token(&self.org, &req).await?;

        print_success(
            &format!("Created claim token '{}' (id: {})", self.name, token.id),
            OutputLevel::Normal,
        );

        // The raw token is only shown on creation
        if let Some(ref raw_token) = token.token {
            println!("\nToken value (save this — it cannot be retrieved later):");
            println!("  {}", raw_token);
        }

        Ok(())
    }
}

pub struct ConnectClaimTokensDeleteCommand {
    pub org: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectClaimTokensDeleteCommand {
    pub async fn execute(&self) -> Result<()> {
        if !self.yes {
            eprint!(
                "Are you sure you want to delete claim token '{}'? This cannot be undone. [y/N]: ",
                self.id
            );
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Cancelled.");
                return Ok(());
            }
        }

        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        client.delete_claim_token(&self.org, &self.id).await?;

        print_success(
            &format!("Deleted claim token '{}'.", self.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
