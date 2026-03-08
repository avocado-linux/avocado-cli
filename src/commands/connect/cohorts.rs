use anyhow::Result;

use crate::commands::connect::client::{
    self, ConnectClient, CreateCohortParams, CreateCohortRequest,
};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectCohortsListCommand {
    pub org: String,
    pub project: String,
    pub profile: Option<String>,
}

impl ConnectCohortsListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref())?;
        let client = ConnectClient::from_profile(profile)?;

        let cohorts = client.list_cohorts(&self.org, &self.project).await?;

        if cohorts.is_empty() {
            print_info("No cohorts found.", OutputLevel::Normal);
            return Ok(());
        }

        let max_name = cohorts.iter().map(|c| c.name.len()).max().unwrap_or(0);

        println!("{:<name_w$}  ID", "NAME", name_w = max_name);
        for cohort in &cohorts {
            println!("{:<name_w$}  {}", cohort.name, cohort.id, name_w = max_name,);
        }

        Ok(())
    }
}

pub struct ConnectCohortsCreateCommand {
    pub org: String,
    pub project: String,
    pub name: String,
    pub description: Option<String>,
    pub profile: Option<String>,
}

impl ConnectCohortsCreateCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref())?;
        let client = ConnectClient::from_profile(profile)?;

        let req = CreateCohortRequest {
            cohort: CreateCohortParams {
                name: self.name.clone(),
                description: self.description.clone(),
            },
        };

        let cohort = client.create_cohort(&self.org, &self.project, &req).await?;

        print_success(
            &format!("Created cohort '{}' (id: {})", cohort.name, cohort.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

pub struct ConnectCohortsDeleteCommand {
    pub org: String,
    pub project: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectCohortsDeleteCommand {
    pub async fn execute(&self) -> Result<()> {
        if !self.yes {
            eprint!(
                "Are you sure you want to delete cohort '{}'? This cannot be undone. [y/N]: ",
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
        let (_, profile) = config.resolve_profile(self.profile.as_deref())?;
        let client = ConnectClient::from_profile(profile)?;

        client
            .delete_cohort(&self.org, &self.project, &self.id)
            .await?;

        print_success(
            &format!("Deleted cohort '{}'.", self.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
