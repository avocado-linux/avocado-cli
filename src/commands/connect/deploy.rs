use anyhow::{Context, Result};
use chrono::Utc;

use crate::commands::connect::client::{
    self, CohortInfo, ConnectClient, CreateDeploymentParams, CreateDeploymentRequest,
    RuntimeListItem,
};
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};

pub struct ConnectDeployCommand {
    pub org: String,
    pub project: String,
    pub runtime: Option<String>,
    pub cohort: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub activate: bool,
    pub profile: Option<String>,
}

impl ConnectDeployCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        // Select runtime (fetches full record even when --runtime is passed)
        let selected_runtime = self.resolve_runtime(&client).await?;

        // Select cohort (fetches full record even when --cohort is passed)
        let selected_cohort = self.resolve_cohort(&client).await?;

        // Generate deployment name if not provided
        let version_display = selected_runtime
            .display_version
            .as_deref()
            .unwrap_or(&selected_runtime.version);
        let deploy_name = self.name.clone().unwrap_or_else(|| {
            let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
            format!("{version_display}-{timestamp}")
        });

        // Create deployment
        print_info(
            &format!("Creating deployment '{deploy_name}'..."),
            OutputLevel::Normal,
        );

        let req = CreateDeploymentRequest {
            deployment: CreateDeploymentParams {
                name: deploy_name.clone(),
                cohort_id: selected_cohort.id.clone(),
                runtime_id: selected_runtime.id.clone(),
                description: self.description.clone(),
                filter_tags: self.tags.clone(),
            },
        };

        let deployment = client
            .create_deployment(&self.org, &self.project, &req)
            .await?;

        // Always print ID first — if activation fails, user still has the deployment ID
        println!();
        print_success(
            &format!(
                "Deployment '{}' created (id: {})",
                deploy_name, deployment.id
            ),
            OutputLevel::Normal,
        );
        println!("  Runtime:  {} ({})", version_display, selected_runtime.id);
        if !selected_cohort.name.is_empty() {
            println!(
                "  Cohort:   {} ({})",
                selected_cohort.name, selected_cohort.id
            );
        } else {
            println!("  Cohort:   {}", selected_cohort.id);
        }
        if !self.tags.is_empty() {
            println!("  Tags:     {}", self.tags.join(", "));
        }

        // Optionally activate
        if self.activate {
            print_info("Activating deployment...", OutputLevel::Normal);
            match client
                .activate_deployment(&self.org, &self.project, &deployment.id)
                .await
            {
                Ok(activated) => {
                    println!("  Status:   {}", activated.status);
                }
                Err(e) => {
                    print_warning(
                        &format!(
                            "Deployment created but activation failed: {e}\n  \
                             The deployment is still in draft. Activate manually or investigate the error."
                        ),
                        OutputLevel::Normal,
                    );
                }
            }
        } else {
            println!("  Status:   {}", deployment.status);
        }

        Ok(())
    }

    async fn resolve_runtime(&self, client: &ConnectClient) -> Result<RuntimeListItem> {
        let runtimes = client.list_runtimes(&self.org, &self.project).await?;

        if let Some(ref runtime_flag) = self.runtime {
            return runtimes
                .into_iter()
                .find(|r| r.id == *runtime_flag)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Runtime '{}' not found in project. Use 'avocado connect upload' first.",
                        runtime_flag
                    )
                });
        }

        if runtimes.is_empty() {
            anyhow::bail!("No runtimes found. Upload one first with 'avocado connect upload'.");
        }

        if runtimes.len() == 1 {
            let rt = runtimes[0].clone();
            print_info(
                &format!(
                    "Auto-selected runtime: {} ({})",
                    rt.display_version.as_deref().unwrap_or(&rt.version),
                    rt.id
                ),
                OutputLevel::Normal,
            );
            return Ok(rt);
        }

        prompt_select_runtime(&runtimes)
    }

    async fn resolve_cohort(&self, client: &ConnectClient) -> Result<CohortInfo> {
        let cohorts = client.list_cohorts(&self.org, &self.project).await?;

        if let Some(ref cohort_flag) = self.cohort {
            return cohorts
                .into_iter()
                .find(|c| c.id == *cohort_flag)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cohort '{}' not found in project. Available: {}",
                        cohort_flag,
                        "none (or check project membership)"
                    )
                });
        }

        if cohorts.is_empty() {
            anyhow::bail!("No cohorts found in project. Create one first in the web UI.");
        }

        if cohorts.len() == 1 {
            let cohort = cohorts[0].clone();
            print_info(
                &format!("Auto-selected cohort: {} ({})", cohort.name, cohort.id),
                OutputLevel::Normal,
            );
            return Ok(cohort);
        }

        prompt_select_cohort(&cohorts)
    }
}

/// Parameters for deploy-after-upload.
pub struct DeployAfterUploadParams<'a> {
    pub client: &'a ConnectClient,
    pub org: &'a str,
    pub project: &'a str,
    pub runtime_id: &'a str,
    pub runtime_version: &'a str,
    pub cohort_id: &'a str,
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub tags: &'a [String],
    pub activate: bool,
}

/// Deploy after a successful upload. Called from upload.rs when --deploy-* flags are present.
pub async fn deploy_after_upload(params: &DeployAfterUploadParams<'_>) -> Result<()> {
    let DeployAfterUploadParams {
        client,
        org,
        project,
        runtime_id,
        runtime_version,
        cohort_id,
        name,
        description,
        tags,
        activate,
    } = params;
    let deploy_name = name.map(|s| s.to_string()).unwrap_or_else(|| {
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        format!("{runtime_version}-{timestamp}")
    });

    print_info(
        &format!("Creating deployment '{deploy_name}'..."),
        OutputLevel::Normal,
    );

    let req = CreateDeploymentRequest {
        deployment: CreateDeploymentParams {
            name: deploy_name.clone(),
            cohort_id: cohort_id.to_string(),
            runtime_id: runtime_id.to_string(),
            description: description.map(|s| s.to_string()),
            filter_tags: tags.to_vec(),
        },
    };

    let deployment = client.create_deployment(org, project, &req).await?;

    // Always print ID first
    print_success(
        &format!(
            "Deployment '{}' created (id: {})",
            deploy_name, deployment.id
        ),
        OutputLevel::Normal,
    );

    if *activate {
        print_info("Activating deployment...", OutputLevel::Normal);
        match client
            .activate_deployment(org, project, &deployment.id)
            .await
        {
            Ok(activated) => {
                println!("  Status: {}", activated.status);
            }
            Err(e) => {
                print_warning(
                    &format!(
                        "Activation failed: {e}\n  \
                         Deployment is still in draft. Activate manually or investigate the error."
                    ),
                    OutputLevel::Normal,
                );
            }
        }
    } else {
        println!("  Status: {}", deployment.status);
    }

    Ok(())
}

/// Validate that --deploy-* flags are consistent. Called from upload before execution.
pub fn validate_deploy_flags(
    deploy_cohort: &Option<String>,
    deploy_name: &Option<String>,
    deploy_tags: &[String],
    deploy_activate: bool,
) -> Result<()> {
    if deploy_cohort.is_none()
        && (deploy_name.is_some() || !deploy_tags.is_empty() || deploy_activate)
    {
        anyhow::bail!(
            "--deploy-cohort is required when using --deploy-name, --deploy-tag, or --deploy-activate"
        );
    }
    Ok(())
}

fn prompt_select_runtime(runtimes: &[RuntimeListItem]) -> Result<RuntimeListItem> {
    println!("\nSelect a runtime:");
    for (i, rt) in runtimes.iter().enumerate() {
        let display = rt.display_version.as_deref().unwrap_or(&rt.version);
        println!(
            "  [{}] {} (id: {}, status: {})",
            i + 1,
            display,
            rt.id,
            rt.status
        );
    }
    eprint!("\nEnter number (1-{}): ", runtimes.len());

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let choice: usize = input.trim().parse().context("Invalid number")?;

    if choice < 1 || choice > runtimes.len() {
        anyhow::bail!("Selection out of range");
    }

    Ok(runtimes[choice - 1].clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_deploy_flags_no_flags_is_ok() {
        assert!(validate_deploy_flags(&None, &None, &[], false).is_ok());
    }

    #[test]
    fn validate_deploy_flags_cohort_only_is_ok() {
        assert!(validate_deploy_flags(&Some("id".into()), &None, &[], false).is_ok());
    }

    #[test]
    fn validate_deploy_flags_all_flags_with_cohort_is_ok() {
        assert!(validate_deploy_flags(
            &Some("id".into()),
            &Some("name".into()),
            &["tag1".into()],
            true
        )
        .is_ok());
    }

    #[test]
    fn validate_deploy_flags_name_without_cohort_errors() {
        let result = validate_deploy_flags(&None, &Some("name".into()), &[], false);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("--deploy-cohort is required"));
    }

    #[test]
    fn validate_deploy_flags_tags_without_cohort_errors() {
        let result = validate_deploy_flags(&None, &None, &["tag".into()], false);
        assert!(result.is_err());
    }

    #[test]
    fn validate_deploy_flags_activate_without_cohort_errors() {
        let result = validate_deploy_flags(&None, &None, &[], true);
        assert!(result.is_err());
    }
}
