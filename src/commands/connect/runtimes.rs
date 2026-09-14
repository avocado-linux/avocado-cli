use anyhow::Result;
use serde_json::json;

use crate::commands::connect::client::{self, ConnectClient};
use crate::utils::output::{print_info, OutputLevel};
use crate::utils::output_format::{emit_json_object, OutputFormat};

pub struct ConnectRuntimesListCommand {
    pub org: String,
    pub project: String,
    pub profile: Option<String>,
    pub output: OutputFormat,
}

impl ConnectRuntimesListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let runtimes = client.list_runtimes(&self.org, &self.project).await?;

        if self.output.is_json() {
            emit_json_object(&json!({
                "runtimes": runtimes.iter().map(|r| json!({
                    "id": r.id,
                    "version": r.version,
                    "display_version": r.display_version,
                    "status": r.status,
                    "has_sbom": r.has_sbom,
                })).collect::<Vec<_>>()
            }));
            return Ok(());
        }

        if runtimes.is_empty() {
            print_info("No runtimes found.", OutputLevel::Normal);
            return Ok(());
        }

        let max_version = runtimes
            .iter()
            .map(|r| r.display_version.as_deref().unwrap_or(&r.version).len())
            .max()
            .unwrap_or(0);

        println!(
            "{:<ver_w$}  {:<10}  {:<4}  ID",
            "VERSION",
            "STATUS",
            "SBOM",
            ver_w = max_version
        );
        for rt in &runtimes {
            let ver = rt.display_version.as_deref().unwrap_or(&rt.version);
            // `None` means the server doesn't report it yet — shown as `?`,
            // never as "no", so an old Connect doesn't read as SBOM-less.
            let sbom = match rt.has_sbom {
                Some(true) => "yes",
                Some(false) => "no",
                None => "?",
            };
            println!(
                "{:<ver_w$}  {:<10}  {:<4}  {}",
                ver,
                rt.status,
                sbom,
                rt.id,
                ver_w = max_version,
            );
        }

        Ok(())
    }
}
