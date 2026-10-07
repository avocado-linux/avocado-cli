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
                    "sbom_state": r.sbom_state,
                })).collect::<Vec<_>>()
            }));
            return Ok(());
        }

        if runtimes.is_empty() {
            print_info("No runtimes found.", OutputLevel::Normal);
            return Ok(());
        }

        println!("{}", runtime_table(&runtimes));

        Ok(())
    }
}

fn runtime_table(runtimes: &[client::RuntimeListItem]) -> String {
    let mut lines = Vec::new();
    let max_version = runtimes
        .iter()
        .map(|r| r.display_version.as_deref().unwrap_or(&r.version).len())
        .max()
        .unwrap_or(0);

    lines.push(format!(
        "{:<ver_w$}  {:<10}  {:<8}  ID",
        "VERSION",
        "STATUS",
        "SBOM",
        ver_w = max_version
    ));
    for rt in runtimes {
        let ver = rt.display_version.as_deref().unwrap_or(&rt.version);
        lines.push(format!(
            "{:<ver_w$}  {:<10}  {:<8}  {}",
            ver,
            rt.status,
            sbom_column(rt.sbom_state.as_deref()),
            rt.id,
            ver_w = max_version,
        ));
    }

    lines.join("\n")
}

fn sbom_column(state: Option<&str>) -> &str {
    state.unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_list_accepts_new_and_old_servers() {
        for state in [None, Some("pending")] {
            let mut value = json!({"id": "r", "version": "1", "status": "draft"});
            if let Some(state) = state {
                value["sbom_state"] = json!(state);
            }
            let item: client::RuntimeListItem = serde_json::from_value(value).unwrap();
            assert_eq!(item.sbom_state.as_deref(), state);
        }
    }

    #[test]
    fn table_renders_unknown_and_current_sbom_states() {
        let rows: Vec<client::RuntimeListItem> = serde_json::from_value(json!([
            {"id":"old", "version":"1", "status":"draft"},
            {"id":"new", "version":"2", "status":"draft", "sbom_state":"pending"}
        ]))
        .unwrap();
        let table = runtime_table(&rows);
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["VERSION", "STATUS", "SBOM", "ID"]
        );
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            ["1", "draft", "?", "old"]
        );
        assert_eq!(
            lines[2].split_whitespace().collect::<Vec<_>>(),
            ["2", "draft", "pending", "new"]
        );
        assert!(lines[1].contains("?         old"));
    }

    #[test]
    fn sbom_column_preserves_states_and_marks_older_servers_unknown() {
        assert_eq!(sbom_column(None), "?");
        for state in ["indexed", "pending", "failed", "none"] {
            assert_eq!(sbom_column(Some(state)), state);
        }
    }
}
