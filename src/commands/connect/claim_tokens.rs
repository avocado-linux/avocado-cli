use anyhow::{Context, Result};

use crate::commands::connect::client::{
    self, CohortInfo, ConnectClient, CreateClaimTokenParams, CreateClaimTokenRequest, ProjectInfo,
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
    pub project: Option<String>,
    pub cohort: Option<String>,
    pub name: String,
    pub tags: Vec<String>,
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

        // Select project → cohort (interactive if flags not provided)
        let selected_cohort = self.resolve_cohort(&client).await?;

        let expires_at = if self.no_expiration {
            Some("2099-12-31T23:59:59Z".to_string())
        } else {
            None
        };

        let req = CreateClaimTokenRequest {
            claim_token: CreateClaimTokenParams {
                name: self.name.clone(),
                cohort_id: selected_cohort.as_ref().map(|c| c.id.clone()),
                max_uses: self.max_uses,
                expires_at,
                tags: self.tags.clone(),
            },
        };

        let token = client.create_claim_token(&self.org, &req).await?;

        print_success(
            &format!("Created claim token '{}' (id: {})", self.name, token.id),
            OutputLevel::Normal,
        );

        if let Some(ref cohort) = selected_cohort {
            if cohort.name.is_empty() {
                println!("  Cohort: {}", cohort.id);
            } else {
                println!("  Cohort: {} ({})", cohort.name, cohort.id);
            }
        }
        if !self.tags.is_empty() {
            println!("  Tags:   {}", self.tags.join(", "));
        }

        // The raw token is only shown on creation
        if let Some(ref raw_token) = token.token {
            println!("\nToken value (save this — it cannot be retrieved later):");
            println!("  {}", raw_token);
        }

        Ok(())
    }

    async fn resolve_cohort(&self, client: &ConnectClient) -> Result<Option<CohortInfo>> {
        // If --cohort provided directly, use it as-is (API validates the ID)
        if let Some(ref cohort_flag) = self.cohort {
            return Ok(Some(CohortInfo {
                id: cohort_flag.clone(),
                name: String::new(),
            }));
        }

        // Interactive: select project then cohort
        let projects = client.list_projects(&self.org).await?;

        if projects.is_empty() {
            print_info(
                "No projects found — claim token will be org-scoped.",
                OutputLevel::Normal,
            );
            return Ok(None);
        }

        let selected_project = if let Some(ref proj_flag) = self.project {
            projects
                .iter()
                .find(|p| p.id == *proj_flag)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Project '{}' not found. Available: {}",
                        proj_flag,
                        projects
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?
        } else if projects.len() == 1 {
            let proj = projects[0].clone();
            print_info(
                &format!("Auto-selected project: {} ({})", proj.name, proj.id),
                OutputLevel::Normal,
            );
            proj
        } else {
            prompt_select_project(&projects)?
        };

        let cohorts = client.list_cohorts(&self.org, &selected_project.id).await?;

        if cohorts.is_empty() {
            anyhow::bail!(
                "No cohorts found in project '{}'. Create a cohort first, or use --cohort to specify one directly.",
                selected_project.name
            );
        }

        let selected_cohort = if cohorts.len() == 1 {
            let cohort = cohorts[0].clone();
            print_info(
                &format!("Auto-selected cohort: {} ({})", cohort.name, cohort.id),
                OutputLevel::Normal,
            );
            cohort
        } else {
            prompt_select_cohort(&cohorts)?
        };

        Ok(Some(selected_cohort))
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

fn prompt_select_project(projects: &[ProjectInfo]) -> Result<ProjectInfo> {
    println!("\nSelect a project:");
    for (i, proj) in projects.iter().enumerate() {
        println!("  [{}] {} (id: {})", i + 1, proj.name, proj.id);
    }
    eprint!("\nEnter number (1-{}): ", projects.len());

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let choice: usize = input.trim().parse().context("Invalid number")?;

    if choice < 1 || choice > projects.len() {
        anyhow::bail!("Selection out of range");
    }

    Ok(projects[choice - 1].clone())
}

fn prompt_select_cohort(cohorts: &[CohortInfo]) -> Result<CohortInfo> {
    println!("\nSelect a cohort:");
    for (i, cohort) in cohorts.iter().enumerate() {
        println!("  [{}] {} (id: {})", i + 1, cohort.name, cohort.id);
    }
    eprint!("\nEnter number (1-{}): ", cohorts.len());

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let choice: usize = input.trim().parse().context("Invalid number")?;

    if choice < 1 || choice > cohorts.len() {
        anyhow::bail!("Selection out of range");
    }

    Ok(cohorts[choice - 1].clone())
}
