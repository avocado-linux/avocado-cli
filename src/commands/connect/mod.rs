pub mod auth;
pub mod claim_tokens;
pub mod clean;
pub mod client;
pub mod cohorts;
pub mod deploy;
pub mod devices;
pub mod init;
pub mod keys;
pub mod orgs;
pub mod projects;
pub mod server_key;
pub mod trust;
pub mod upload;

use anyhow::Result;

/// Resolve organization from CLI flag or avocado.yaml connect config.
pub fn resolve_org(flag: Option<String>, config_path: &str) -> Result<String> {
    let connect_config = std::path::Path::new(config_path)
        .exists()
        .then(|| crate::utils::config::load_config(config_path).ok())
        .flatten()
        .and_then(|c| c.connect);

    flag.or_else(|| connect_config.as_ref().and_then(|c| c.org.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--org is required (or set connect.org in {config_path})\n\
                 Tip: Run 'avocado connect auth status' to see your organizations"
            )
        })
}

/// Resolve both organization and project from CLI flags or avocado.yaml connect config.
pub fn resolve_org_and_project(
    org: Option<String>,
    project: Option<String>,
    config_path: &str,
) -> Result<(String, String)> {
    let connect_config = std::path::Path::new(config_path)
        .exists()
        .then(|| crate::utils::config::load_config(config_path).ok())
        .flatten()
        .and_then(|c| c.connect);

    let resolved_org = org
        .or_else(|| connect_config.as_ref().and_then(|c| c.org.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--org is required (or set connect.org in {config_path})\n\
                 Tip: Run 'avocado connect auth status' to see your organizations"
            )
        })?;

    let resolved_project = project
        .or_else(|| connect_config.as_ref().and_then(|c| c.project.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--project is required (or set connect.project in {config_path})\n\
                 Tip: Run 'avocado connect projects list --org {resolved_org}' to see projects"
            )
        })?;

    Ok((resolved_org, resolved_project))
}
