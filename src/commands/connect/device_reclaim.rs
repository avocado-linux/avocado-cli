use anyhow::Result;

use crate::commands::connect::client::{self, ConnectClient, ReclaimRequest};
use crate::utils::output::{print_info, print_success, OutputLevel};

fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N]: ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

/// Normalize a deny-reason string so empty or whitespace-only inputs map
/// to `None` (i.e. omit the field on the request body) rather than
/// `Some("")`. Used uniformly for both `--reason "..."` flag values and
/// the interactive prompt path.
fn normalize_reason(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn device_label(req: &ReclaimRequest) -> String {
    match &req.device {
        Some(d) => match &d.name {
            Some(name) => format!("{name} ({})", d.identifier),
            None => d.identifier.clone(),
        },
        None => req.device_id.clone(),
    }
}

pub struct ConnectDeviceReclaimListCommand {
    pub org: String,
    pub status: String,
    pub device_id: Option<String>,
    pub profile: Option<String>,
}

impl ConnectDeviceReclaimListCommand {
    pub async fn execute(&self) -> Result<()> {
        // No runtime status validation here — clap's ValueEnum on
        // ReclaimStatusFilter rejects bad values at parse time. The string
        // passed in here came from `ReclaimStatusFilter::as_str()`.
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        // Always pass --status through explicitly, including "all". Sending
        // no status param makes the API fall back to its `pending` default
        // (chosen for the SPA's "what needs my attention" UX), which would
        // silently drop completed/denied/expired rows from `--status all`.
        let requests = client
            .list_reclaim_requests(
                &self.org,
                Some(self.status.as_str()),
                self.device_id.as_deref(),
            )
            .await?;

        if requests.is_empty() {
            let scope = if self.status == "all" {
                "any status".to_string()
            } else {
                format!("status '{}'", self.status)
            };
            let device_clause = match &self.device_id {
                Some(id) => format!(" for device '{id}'"),
                None => String::new(),
            };
            print_info(
                &format!(
                    "No reclaim requests found in org '{}' with {scope}{device_clause}.",
                    self.org
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        let max_id = requests
            .iter()
            .map(|r| r.id.len())
            .max()
            .unwrap_or(0)
            .max(2);
        let max_device = requests
            .iter()
            .map(|r| device_label(r).len())
            .max()
            .unwrap_or(0)
            .max(6);
        let max_status = requests
            .iter()
            .map(|r| r.status.len())
            .max()
            .unwrap_or(0)
            .max(6);
        let max_requested = requests
            .iter()
            .map(|r| r.requested_at.as_deref().unwrap_or("-").len())
            .max()
            .unwrap_or(0)
            .max(9);
        let max_expires = requests
            .iter()
            .map(|r| r.expires_at.as_deref().unwrap_or("-").len())
            .max()
            .unwrap_or(0)
            .max(7);

        println!(
            "{:<id_w$}  {:<dev_w$}  {:<st_w$}  {:<req_w$}  {:<exp_w$}  IP",
            "ID",
            "DEVICE",
            "STATUS",
            "REQUESTED",
            "EXPIRES",
            id_w = max_id,
            dev_w = max_device,
            st_w = max_status,
            req_w = max_requested,
            exp_w = max_expires,
        );
        for req in &requests {
            println!(
                "{:<id_w$}  {:<dev_w$}  {:<st_w$}  {:<req_w$}  {:<exp_w$}  {}",
                req.id,
                device_label(req),
                req.status,
                req.requested_at.as_deref().unwrap_or("-"),
                req.expires_at.as_deref().unwrap_or("-"),
                req.request_ip.as_deref().unwrap_or("-"),
                id_w = max_id,
                dev_w = max_device,
                st_w = max_status,
                req_w = max_requested,
                exp_w = max_expires,
            );
        }

        Ok(())
    }
}

pub struct ConnectDeviceReclaimApproveCommand {
    pub org: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectDeviceReclaimApproveCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        if !self.yes {
            let prompt = format!("Approve reclaim request '{}'?", self.id);
            if !confirm(&prompt)? {
                println!("Cancelled.");
                return Ok(());
            }
        }

        let request = client.approve_reclaim_request(&self.org, &self.id).await?;

        print_success(
            &format!(
                "Reclaim approved for device '{}'. Device will receive new credentials on next /claim attempt.",
                device_label(&request)
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

pub struct ConnectDeviceReclaimDenyCommand {
    pub org: String,
    pub id: String,
    pub reason: Option<String>,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectDeviceReclaimDenyCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        // `--reason ""` (or whitespace-only) takes the same "skip" path as
        // pressing Enter at the interactive prompt — omits the field rather
        // than sending an empty string. `normalize_reason` enforces that.
        let reason = match self.reason.as_deref() {
            Some(r) => normalize_reason(r),
            None => {
                eprint!("Reason for denial (optional, press Enter to skip): ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                normalize_reason(&input)
            }
        };

        if !self.yes {
            let prompt = format!("Deny reclaim request '{}'?", self.id);
            if !confirm(&prompt)? {
                println!("Cancelled.");
                return Ok(());
            }
        }

        let request = client
            .deny_reclaim_request(&self.org, &self.id, reason.as_deref())
            .await?;

        print_success(
            &format!(
                "Reclaim denied for device '{}'. Device's existing credentials are revoked.",
                device_label(&request)
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

pub struct ConnectDeviceReclaimDeleteCommand {
    pub org: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectDeviceReclaimDeleteCommand {
    pub async fn execute(&self) -> Result<()> {
        if !self.yes {
            let prompt = format!(
                "Delete denied reclaim request '{}'? This removes the audit row.",
                self.id
            );
            if !confirm(&prompt)? {
                println!("Cancelled.");
                return Ok(());
            }
        }

        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        client.delete_reclaim_request(&self.org, &self.id).await?;

        print_success(
            &format!("Deleted reclaim request '{}'.", self.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_reason_keeps_real_text() {
        assert_eq!(
            normalize_reason("decommissioned"),
            Some("decommissioned".to_string())
        );
    }

    #[test]
    fn normalize_reason_trims_surrounding_whitespace() {
        assert_eq!(
            normalize_reason("  device retired  "),
            Some("device retired".to_string())
        );
    }

    #[test]
    fn normalize_reason_treats_empty_as_none() {
        // `--reason ""` should be equivalent to omitting the flag — the
        // API receives no reason field, not `denied_reason: ""`.
        assert_eq!(normalize_reason(""), None);
    }

    #[test]
    fn normalize_reason_treats_whitespace_only_as_none() {
        assert_eq!(normalize_reason("   "), None);
        assert_eq!(normalize_reason("\t\n"), None);
    }

    #[test]
    fn device_label_prefers_name_with_identifier() {
        let req = ReclaimRequest {
            id: "r-1".to_string(),
            device_id: "d-1".to_string(),
            device: Some(client::ReclaimRequestDevice {
                id: "d-1".to_string(),
                name: Some("warehouse-1".to_string()),
                identifier: "fp-abc".to_string(),
                status: Some("registered".to_string()),
            }),
            status: "pending".to_string(),
            requested_fingerprint: None,
            requested_at: None,
            resolved_at: None,
            expires_at: None,
            request_ip: None,
            request_user_agent: None,
            denied_reason: None,
        };
        assert_eq!(device_label(&req), "warehouse-1 (fp-abc)");
    }

    #[test]
    fn device_label_falls_back_to_identifier_when_name_missing() {
        let req = ReclaimRequest {
            id: "r-1".to_string(),
            device_id: "d-1".to_string(),
            device: Some(client::ReclaimRequestDevice {
                id: "d-1".to_string(),
                name: None,
                identifier: "fp-abc".to_string(),
                status: None,
            }),
            status: "pending".to_string(),
            requested_fingerprint: None,
            requested_at: None,
            resolved_at: None,
            expires_at: None,
            request_ip: None,
            request_user_agent: None,
            denied_reason: None,
        };
        assert_eq!(device_label(&req), "fp-abc");
    }

    #[test]
    fn device_label_falls_back_to_device_id_when_no_device_embedded() {
        let req = ReclaimRequest {
            id: "r-1".to_string(),
            device_id: "d-1".to_string(),
            device: None,
            status: "pending".to_string(),
            requested_fingerprint: None,
            requested_at: None,
            resolved_at: None,
            expires_at: None,
            request_ip: None,
            request_user_agent: None,
            denied_reason: None,
        };
        assert_eq!(device_label(&req), "d-1");
    }
}
