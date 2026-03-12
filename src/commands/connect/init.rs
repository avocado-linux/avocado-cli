use anyhow::{Context, Result};
use std::path::Path;

use crate::commands::connect::client::{self, ConnectClient, OrgInfo, ProjectInfo};
use crate::utils::config_edit;
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectInitCommand {
    pub org: Option<String>,
    pub project: Option<String>,
    pub config_path: String,
    pub profile: Option<String>,
}

impl ConnectInitCommand {
    pub async fn execute(&self) -> Result<()> {
        // 1. Verify login
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref())?;
        let client = ConnectClient::from_profile(profile)?;

        print_info("Verifying authentication...", OutputLevel::Normal);
        let me = client.get_me_full().await?;
        print_info(
            &format!("Authenticated as {} ({})", me.user.name, me.user.email),
            OutputLevel::Normal,
        );

        if me.organizations.is_empty() {
            anyhow::bail!("No organizations found for your account.");
        }

        // 2. Select organization
        let selected_org = if let Some(ref org_flag) = self.org {
            // Non-interactive: use provided org
            me.organizations
                .iter()
                .find(|o| o.id == *org_flag)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Organization '{}' not found. Available: {}",
                        org_flag,
                        me.organizations
                            .iter()
                            .map(|o| o.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?
        } else if me.organizations.len() == 1 {
            let org = me.organizations[0].clone();
            print_info(
                &format!("Auto-selected organization: {} ({})", org.name, org.id),
                OutputLevel::Normal,
            );
            org
        } else {
            prompt_select_org(&me.organizations)?
        };

        // 3. Fetch projects for selected org
        let projects = client.list_projects(&selected_org.id).await?;

        if projects.is_empty() {
            anyhow::bail!(
                "No projects found in org '{}'. Create a project in the web UI first.",
                selected_org.name
            );
        }

        // 4. Select project
        let selected_project = if let Some(ref proj_flag) = self.project {
            projects
                .iter()
                .find(|p| p.name == *proj_flag || p.id == *proj_flag)
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

        // 5. Fetch server key
        print_info("Fetching server signing key...", OutputLevel::Normal);
        let server_key = client.get_tuf_server_key(&selected_org.id).await?;

        // 6. Write to avocado.yaml
        let config_path = Path::new(&self.config_path);
        if !config_path.exists() {
            anyhow::bail!(
                "Config file '{}' not found. Run this command from your project directory.",
                self.config_path
            );
        }

        config_edit::set_connect_fields(
            config_path,
            &selected_org.id,
            &selected_project.id,
            &server_key.public_key_hex,
        )?;

        // 7. Print summary
        let key_short = if server_key.public_key_hex.len() > 16 {
            &server_key.public_key_hex[..16]
        } else {
            &server_key.public_key_hex
        };
        let keyid_short = if server_key.keyid.len() > 12 {
            &server_key.keyid[..12]
        } else {
            &server_key.keyid
        };

        println!();
        print_success("Connect configured:", OutputLevel::Normal);
        println!("  Org:        {} ({})", selected_org.name, selected_org.id);
        println!(
            "  Project:    {} (id: {})",
            selected_project.name, selected_project.id
        );
        println!("  Server key: {}... (keyid: {}...)", key_short, keyid_short);
        println!();
        println!("Updated {} with connect settings.", self.config_path);
        println!("You can now run: avocado build -r <runtime> && avocado connect upload <runtime>");

        Ok(())
    }
}

fn prompt_select_org(orgs: &[OrgInfo]) -> Result<OrgInfo> {
    println!("\nSelect an organization:");
    for (i, org) in orgs.iter().enumerate() {
        println!(
            "  [{}] {} ({}) - role: {}",
            i + 1,
            org.name,
            org.id,
            org.role
        );
    }
    eprint!("\nEnter number (1-{}): ", orgs.len());

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let choice: usize = input.trim().parse().context("Invalid number")?;

    if choice < 1 || choice > orgs.len() {
        anyhow::bail!("Selection out of range");
    }

    Ok(orgs[choice - 1].clone())
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
