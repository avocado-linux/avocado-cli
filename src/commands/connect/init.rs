use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

use crate::commands::connect::client::{
    self, CohortInfo, ConnectClient, CreateClaimTokenParams, CreateClaimTokenRequest, OrgInfo,
    Profile, ProjectInfo,
};
use crate::utils::config_edit;
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};

pub struct ConnectInitCommand {
    pub org: Option<String>,
    pub project: Option<String>,
    pub cohort: Option<String>,
    pub runtime: String,
    pub config_path: String,
    pub profile: Option<String>,
}

impl ConnectInitCommand {
    pub async fn execute(&self) -> Result<()> {
        // 1. Verify login
        let mut config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, initial_profile) = config.resolve_profile(self.profile.as_deref(), None)?;
        let mut client = ConnectClient::from_profile(initial_profile)?;
        let initial_api_url = initial_profile.api_url.clone();
        let initial_user = initial_profile.user.clone();

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

        // 3. Ensure we have an org-scoped profile for the selected org (unless --profile was explicit)
        if self.profile.is_none() {
            if let Some((_, org_profile)) = config.find_profile_by_org(&selected_org.id) {
                // Reuse existing org-scoped profile.
                print_info(
                    &format!(
                        "Using existing org-scoped profile for '{}'.",
                        selected_org.name
                    ),
                    OutputLevel::Normal,
                );
                client = ConnectClient::from_profile(org_profile)?;
            } else {
                // Create a new org-scoped token and profile.
                let hostname = std::env::var("HOSTNAME")
                    .or_else(|_| std::env::var("COMPUTERNAME"))
                    .unwrap_or_else(|_| "unknown".to_string());
                let token_name = format!(
                    "avocado-cli-{hostname}-{}",
                    selected_org.name.to_lowercase().replace(' ', "-")
                );
                print_info(
                    &format!("Creating org-scoped token for '{}'...", selected_org.name),
                    OutputLevel::Normal,
                );
                let (new_token, org_id) = client
                    .create_org_token(&selected_org.id, &token_name)
                    .await?;

                // Derive a profile name from the org name.
                let profile_name = selected_org.name.to_lowercase().replace(' ', "-");
                let new_profile = Profile {
                    api_url: initial_api_url.clone(),
                    token: new_token,
                    user: initial_user.clone(),
                    created_at: Utc::now().to_rfc3339(),
                    organization_id: Some(org_id),
                };
                client = ConnectClient::from_profile(&new_profile)?;
                config.upsert_profile(&profile_name, new_profile);
                client::save_config(&config)?;
                print_success(
                    &format!("Created org-scoped profile '{profile_name}'."),
                    OutputLevel::Normal,
                );
            }
        }

        // 4. Fetch projects for selected org
        let projects = client.list_projects(&selected_org.id).await?;

        if projects.is_empty() {
            anyhow::bail!(
                "No projects found in org '{}'. Create a project in the web UI first.",
                selected_org.name
            );
        }

        // 5. Select project
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

        // 6. Select cohort (optional — for scoping the claim token)
        let cohorts = client
            .list_cohorts(&selected_org.id, &selected_project.id)
            .await?;

        let selected_cohort = if let Some(ref cohort_flag) = self.cohort {
            Some(
                cohorts
                    .iter()
                    .find(|c| c.id == *cohort_flag)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Cohort '{}' not found. Available: {}",
                            cohort_flag,
                            cohorts
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
            )
        } else if cohorts.len() == 1 {
            let cohort = cohorts[0].clone();
            print_info(
                &format!("Auto-selected cohort: {} ({})", cohort.name, cohort.id),
                OutputLevel::Normal,
            );
            Some(cohort)
        } else if cohorts.len() > 1 {
            Some(prompt_select_cohort(&cohorts)?)
        } else {
            print_info(
                "No cohorts found — claim token will be org-scoped.",
                OutputLevel::Normal,
            );
            None
        };

        // 7. Fetch server key
        print_info("Fetching server signing key...", OutputLevel::Normal);
        let server_key = client.get_tuf_server_key(&selected_org.id).await?;

        // 8. Write connect: block to avocado.yaml
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

        // 9. Add connect extensions to avocado.yaml
        let extensions_added = config_edit::ensure_connect_extensions(config_path, &self.runtime)?;
        if extensions_added {
            print_success(
                &format!("Added connect extensions to runtime '{}'.", self.runtime),
                OutputLevel::Normal,
            );
        } else {
            print_info(
                "Connect extensions already present in config.",
                OutputLevel::Normal,
            );
        }

        // 10. Ensure avocado-ext-connect-config extension has overlay: set
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let overlay_dir = config_edit::ensure_extension_overlay(
            config_path,
            "avocado-ext-connect-config",
            "overlay",
        )?;
        let overlay_path = config_dir.join(&overlay_dir);
        let config_toml_path = overlay_path.join("etc/avocado-conn/config.toml");

        if config_toml_path.exists() {
            print_warning(
                "Device already has connect configuration at:",
                OutputLevel::Normal,
            );
            println!("  {}", config_toml_path.display());
            eprint!("Overwrite with new claim token? [y/N]: ");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("Failed to read input")?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Skipping config.toml and claim token creation.");
                print_final_summary(
                    &selected_org,
                    &selected_project,
                    &selected_cohort,
                    &server_key.public_key_hex,
                    &server_key.keyid,
                    None,
                    &self.config_path,
                );
                return Ok(());
            }
        }

        // 11. Create claim token
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let token_name = format!("connect-init-{timestamp}");
        print_info(
            &format!("Creating claim token '{token_name}'..."),
            OutputLevel::Normal,
        );

        let claim_token = client
            .create_claim_token(
                &selected_org.id,
                &CreateClaimTokenRequest {
                    claim_token: CreateClaimTokenParams {
                        name: token_name.clone(),
                        cohort_id: selected_cohort.as_ref().map(|c| c.id.clone()),
                        max_uses: None,
                        expires_at: Some("2099-12-31T23:59:59Z".to_string()),
                        tags: vec![],
                    },
                },
            )
            .await?;

        let raw_token = claim_token
            .token
            .as_ref()
            .context("Claim token response missing raw token value")?;

        print_success(
            &format!(
                "Created claim token '{}' (id: {})",
                token_name, claim_token.id
            ),
            OutputLevel::Normal,
        );

        // 12. Write overlay/etc/avocado-conn/config.toml
        let config_toml_dir = config_toml_path.parent().unwrap();
        std::fs::create_dir_all(config_toml_dir)
            .with_context(|| format!("Failed to create {}", config_toml_dir.display()))?;

        let config_toml_content = format!(
            r#"# Avocado Connect — device config (generated by avocado connect init)
#
# On first boot the daemon claims the device using the token below,
# receives credentials, and persists them. Subsequent boots reuse
# saved credentials automatically.

# Persistent storage for credentials after claim
data_dir = "/var/lib/avocado/connect"

# Claim token — created {timestamp} via connect init.
claim_token = "{raw_token}"

# How to derive the hardware fingerprint sent during claim.
# nic-mac uses the first permanent NIC MAC address.
device_id_source = "nic-mac"

[intervals]
keepalive_secs = 30
"#
        );

        std::fs::write(&config_toml_path, &config_toml_content)
            .with_context(|| format!("Failed to write {}", config_toml_path.display()))?;

        print_success(
            &format!("Wrote {}", config_toml_path.display()),
            OutputLevel::Normal,
        );

        // 13. Print summary
        print_final_summary(
            &selected_org,
            &selected_project,
            &selected_cohort,
            &server_key.public_key_hex,
            &server_key.keyid,
            Some(&token_name),
            &self.config_path,
        );

        Ok(())
    }
}

fn print_final_summary(
    org: &OrgInfo,
    project: &ProjectInfo,
    cohort: &Option<CohortInfo>,
    public_key_hex: &str,
    keyid: &str,
    claim_token_name: Option<&str>,
    config_path: &str,
) {
    let key_short = if public_key_hex.len() > 16 {
        &public_key_hex[..16]
    } else {
        public_key_hex
    };
    let keyid_short = if keyid.len() > 12 {
        &keyid[..12]
    } else {
        keyid
    };

    println!();
    print_success("Connect initialized:", OutputLevel::Normal);
    println!("  Org:          {} ({})", org.name, org.id);
    println!("  Project:      {} ({})", project.name, project.id);
    if let Some(ref c) = cohort {
        println!("  Cohort:       {} ({})", c.name, c.id);
    }
    println!(
        "  Server key:   {}... (keyid: {}...)",
        key_short, keyid_short
    );
    if let Some(name) = claim_token_name {
        println!("  Claim token:  {name} (no expiration)");
    }
    println!();
    println!("Updated {} with connect settings.", config_path);
    println!("Project is ready. Build and boot your device — it will auto-claim on first connect.");
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

fn prompt_select_cohort(cohorts: &[CohortInfo]) -> Result<CohortInfo> {
    println!("\nSelect a cohort for the claim token:");
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
