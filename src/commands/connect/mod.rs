pub mod auth;
pub mod claim_tokens;
pub mod clean;
pub mod client;
pub mod cohorts;
pub mod deploy;
pub mod device_reclaim;
pub mod devices;
pub mod ext;
pub mod init;
pub mod keys;
pub mod orgs;
pub mod projects;
pub mod server_key;
pub mod trust;
pub mod upload;

use anyhow::Result;

/// Resolve the active profile's `organization_id` from `credentials.json`.
///
/// Errors if `--profile <name>` is given but `<name>` does not exist. Returns
/// `Ok(None)` when no config file exists, or when the resolved profile has no
/// `organization_id` set (e.g. a legacy profile or an unscoped token).
pub fn profile_organization_id(profile: Option<&str>) -> Result<Option<String>> {
    let Some(cfg) = client::load_config()? else {
        return Ok(None);
    };
    let (_, profile) = cfg.resolve_profile(profile, None)?;
    Ok(profile.organization_id.clone())
}

/// Resolve organization from CLI flag, `connect.org` in `avocado.yaml`, or the
/// active profile's `organization_id`. Precedence: flag > yaml > profile.
pub fn resolve_org(
    flag: Option<String>,
    config_path: &str,
    profile_org: Option<String>,
) -> Result<String> {
    let yaml_org = load_yaml_org(config_path);

    flag.or(yaml_org)
        .or(profile_org)
        .ok_or_else(|| anyhow::anyhow!(no_org_error(config_path)))
}

/// Resolve both organization and project. Org follows the same precedence as
/// `resolve_org`. Project only falls back to `connect.project` in `avocado.yaml`
/// (no profile fallback — profiles are tied to orgs, not projects).
pub fn resolve_org_and_project(
    org: Option<String>,
    project: Option<String>,
    config_path: &str,
    profile_org: Option<String>,
) -> Result<(String, String)> {
    let connect_config = std::path::Path::new(config_path)
        .exists()
        .then(|| crate::utils::config::load_config(config_path).ok())
        .flatten()
        .and_then(|c| c.connect);

    let yaml_org = connect_config.as_ref().and_then(|c| c.org.clone());
    let yaml_project = connect_config.as_ref().and_then(|c| c.project.clone());

    let resolved_org = org
        .or(yaml_org)
        .or(profile_org)
        .ok_or_else(|| anyhow::anyhow!(no_org_error(config_path)))?;

    let resolved_project = project.or(yaml_project).ok_or_else(|| {
        anyhow::anyhow!(
            "--project is required (or set connect.project in {config_path})\n\
             Tip: Run 'avocado connect projects list --org {resolved_org}' to see projects"
        )
    })?;

    Ok((resolved_org, resolved_project))
}

fn load_yaml_org(config_path: &str) -> Option<String> {
    std::path::Path::new(config_path)
        .exists()
        .then(|| crate::utils::config::load_config(config_path).ok())
        .flatten()
        .and_then(|c| c.connect)
        .and_then(|c| c.org)
}

fn no_org_error(config_path: &str) -> String {
    format!(
        "no organization selected. Pass --org <id>, set connect.org in {config_path}, \
         or pass --profile <name> to use that profile's organization.\n\
         Tip: Run 'avocado connect auth status' to see your profiles and their organizations"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_yaml_with_connect_org(dir: &TempDir, org: &str) -> String {
        let path = dir.path().join("avocado.yaml");
        fs::write(
            &path,
            format!("connect:\n  org: {org}\n  project: project-from-yaml\n"),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    fn nonexistent_config_path(dir: &TempDir) -> String {
        dir.path()
            .join("does-not-exist.yaml")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn flag_wins_over_yaml_and_profile() {
        let dir = TempDir::new().unwrap();
        let config = write_yaml_with_connect_org(&dir, "yaml-org");
        let got =
            resolve_org(Some("flag-org".into()), &config, Some("profile-org".into())).unwrap();
        assert_eq!(got, "flag-org");
    }

    #[test]
    fn yaml_wins_over_profile_when_flag_absent() {
        let dir = TempDir::new().unwrap();
        let config = write_yaml_with_connect_org(&dir, "yaml-org");
        let got = resolve_org(None, &config, Some("profile-org".into())).unwrap();
        assert_eq!(got, "yaml-org");
    }

    #[test]
    fn profile_fills_in_when_flag_and_yaml_absent() {
        let dir = TempDir::new().unwrap();
        let config = nonexistent_config_path(&dir);
        let got = resolve_org(None, &config, Some("profile-org".into())).unwrap();
        assert_eq!(got, "profile-org");
    }

    #[test]
    fn errors_when_all_three_absent() {
        let dir = TempDir::new().unwrap();
        let config = nonexistent_config_path(&dir);
        let err = resolve_org(None, &config, None).unwrap_err().to_string();
        assert!(err.contains("--org"));
        assert!(err.contains("connect.org"));
        assert!(err.contains("profile"));
    }

    #[test]
    fn yaml_present_but_org_unset_falls_through_to_profile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("avocado.yaml");
        // Valid YAML with no connect section at all.
        fs::write(&path, "ext:\n  foo: bar\n").unwrap();
        let config = path.to_string_lossy().into_owned();
        let got = resolve_org(None, &config, Some("profile-org".into())).unwrap();
        assert_eq!(got, "profile-org");
    }

    #[test]
    fn resolve_org_and_project_uses_profile_org_when_yaml_missing_org() {
        let dir = TempDir::new().unwrap();
        let config = nonexistent_config_path(&dir);
        let (org, project) = resolve_org_and_project(
            None,
            Some("project-flag".into()),
            &config,
            Some("profile-org".into()),
        )
        .unwrap();
        assert_eq!(org, "profile-org");
        assert_eq!(project, "project-flag");
    }

    #[test]
    fn resolve_org_and_project_project_flag_wins_over_yaml() {
        let dir = TempDir::new().unwrap();
        let config = write_yaml_with_connect_org(&dir, "yaml-org");
        let (org, project) =
            resolve_org_and_project(None, Some("project-flag".into()), &config, None).unwrap();
        assert_eq!(org, "yaml-org");
        assert_eq!(project, "project-flag");
    }

    #[test]
    fn resolve_org_and_project_errors_with_no_project() {
        let dir = TempDir::new().unwrap();
        let config = nonexistent_config_path(&dir);
        let err = resolve_org_and_project(None, None, &config, Some("profile-org".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--project"));
    }
}
