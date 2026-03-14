use anyhow::Result;

use crate::commands::connect::client::{
    self, ConnectClient, CreateProjectParams, CreateProjectRequest,
};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectProjectsListCommand {
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectProjectsListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let projects = client.list_projects(&self.org).await?;

        if projects.is_empty() {
            print_info(
                &format!("No projects found in org '{}'.", self.org),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        let max_name = projects.iter().map(|p| p.name.len()).max().unwrap_or(0);

        println!("{:<name_w$}  ID", "NAME", name_w = max_name);
        for project in &projects {
            println!(
                "{:<name_w$}  {}",
                project.name,
                project.id,
                name_w = max_name,
            );
        }

        Ok(())
    }
}

pub struct ConnectProjectsCreateCommand {
    pub org: String,
    pub name: String,
    pub description: Option<String>,
    pub profile: Option<String>,
}

impl ConnectProjectsCreateCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let req = CreateProjectRequest {
            project: CreateProjectParams {
                name: self.name.clone(),
                description: self.description.clone(),
            },
        };

        let project = client.create_project(&self.org, &req).await?;

        print_success(
            &format!("Created project '{}' (id: {})", project.name, project.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

pub struct ConnectProjectsDeleteCommand {
    pub org: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectProjectsDeleteCommand {
    pub async fn execute(&self) -> Result<()> {
        if !self.yes {
            eprint!(
                "Are you sure you want to delete project '{}'? This cannot be undone. [y/N]: ",
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

        client.delete_project(&self.org, &self.id).await?;

        print_success(
            &format!("Deleted project '{}'.", self.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
