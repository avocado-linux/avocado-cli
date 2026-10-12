//! `avocado vm connect` — associate this VM with a Connect org.
//!
//! Mints a single-use, short-lived claim token (kind `vm`) with the user's
//! Connect login, writes avocado-conn's config into the guest over SSH and
//! starts it; avocado-conn claims with the VM's DMI UUID. Credentials stay
//! in the guest's /var (var.btrfs), so `vm reset` means connecting again.

use anyhow::{bail, Context, Result};
use base64::Engine;

use crate::commands::connect::client::{
    self, ConnectClient, CreateClaimTokenParams, CreateClaimTokenRequest,
};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::{hub, lifecycle};

/// How long the minted token stays valid; the claim happens within seconds.
const TOKEN_TTL_MINUTES: i64 = 15;

pub enum ConnectAction {
    Connect {
        org: Option<String>,
        name: Option<String>,
        force: bool,
    },
    Status,
    Disconnect,
}

pub struct VmConnectCommand {
    pub action: ConnectAction,
    pub profile: Option<String>,
}

impl VmConnectCommand {
    pub async fn execute(self) -> Result<()> {
        if !lifecycle::status().await?.running {
            bail!("the VM is not running; start it first (`avocado vm start` or `avocado vm service install`)");
        }
        let target = lifecycle::ssh_target_for_running()?;
        let state_file = format!("{}/claimed_state.json", hub::CONN_DIR);
        match self.action {
            ConnectAction::Status => {
                let (out, _) = target
                    .exec(&format!(
                        "systemctl is-active avocado-conn 2>/dev/null; cat {state_file} 2>/dev/null || true"
                    ))
                    .await?;
                let mut lines = out.lines();
                let active = lines.next().unwrap_or("inactive");
                let state: Option<serde_json::Value> =
                    serde_json::from_str(&lines.collect::<Vec<_>>().join("\n")).ok();
                match state {
                    Some(s) => println!(
                        "connected: device {} (avocado-conn {active}, claimed {})",
                        s["device_id"].as_str().unwrap_or("?"),
                        s["claimed_at"].as_str().unwrap_or("?"),
                    ),
                    None => println!("not connected (avocado-conn {active})"),
                }
                Ok(())
            }
            ConnectAction::Disconnect => {
                target
                    .exec(&format!(
                        "systemctl stop avocado-conn 2>/dev/null; rm -f {d}/config.toml {state_file}",
                        d = hub::CONN_DIR
                    ))
                    .await?;
                print_success(
                    "VM disconnected locally. Delete its record in Connect to remove it there.",
                    OutputLevel::Normal,
                );
                Ok(())
            }
            ConnectAction::Connect { org, name, force } => {
                let (out, _) = target
                    .exec(&format!("test -f {state_file} && echo claimed || true"))
                    .await?;
                if out.trim() == "claimed" && !force {
                    bail!("this VM is already connected (`avocado vm connect --status`); use --force to claim again");
                }
                let config = client::load_config()?.ok_or_else(|| {
                    anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'")
                })?;
                let (_, profile) =
                    config.resolve_profile(self.profile.as_deref(), org.as_deref())?;
                let org_id = org
                    .or_else(|| profile.organization_id.clone())
                    .context("no organization: pass --org or log in with an org-scoped profile")?;
                let api_url = profile.api_url.clone();
                let connect = ConnectClient::from_profile(profile)?;

                let facts = hub::host_facts(&crate::utils::vm::state::VmPaths::resolve()?);
                let name = name.unwrap_or_else(|| {
                    format!("vm-{}", facts["hostname"].as_str().unwrap_or("host"))
                });
                let expires = chrono::Utc::now() + chrono::Duration::minutes(TOKEN_TTL_MINUTES);
                let token = connect
                    .create_claim_token(
                        &org_id,
                        &CreateClaimTokenRequest {
                            claim_token: CreateClaimTokenParams {
                                name: name.clone(),
                                cohort_id: None,
                                max_uses: Some(1),
                                expires_at: Some(
                                    expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                ),
                                tags: vec![],
                                kind: Some("vm".into()),
                            },
                        },
                    )
                    .await?;
                let raw = token
                    .token
                    .context("Connect did not return the claim token value")?;

                let cfg = hub::conn_config(&api_url, &raw);
                let b64 = base64::engine::general_purpose::STANDARD.encode(cfg);
                target
                    .exec(&format!(
                        "systemctl stop avocado-conn 2>/dev/null; mkdir -p {d} && \
                         for f in config.toml claimed_state.json; do [ -f {d}/$f ] && mv {d}/$f {d}/$f.prev; done; \
                         echo {b64} | base64 -d > {d}/config.toml && chmod 0600 {d}/config.toml && sync && \
                         systemctl restart avocado-conn",
                        d = hub::CONN_DIR
                    ))
                    .await
                    .context("could not start avocado-conn in the VM")?;
                print_info("Claiming…", OutputLevel::Normal);
                for _ in 0..30 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let (out, _) = target
                        .exec(&format!("cat {state_file} 2>/dev/null || true"))
                        .await?;
                    if let Ok(s) = serde_json::from_str::<serde_json::Value>(out.trim()) {
                        let _ = target
                            .exec(&format!("rm -f {d}/*.prev", d = hub::CONN_DIR))
                            .await;
                        print_success(
                            &format!(
                                "VM connected as '{name}' (device {})",
                                s["device_id"].as_str().unwrap_or("?")
                            ),
                            OutputLevel::Normal,
                        );
                        return Ok(());
                    }
                }
                // Put the previous identity back rather than leave none.
                let _ = target
                    .exec(&format!(
                        "systemctl stop avocado-conn; cd {d} && for f in config.toml claimed_state.json; do \
                         [ -f $f.prev ] && mv $f.prev $f; done; sync; \
                         [ -f claimed_state.json ] && systemctl start avocado-conn; true",
                        d = hub::CONN_DIR
                    ))
                    .await;
                bail!(
                    "avocado-conn did not claim within 30 s (previous connection restored, if any); \
                     check `avocado vm shell -- journalctl -u avocado-conn`"
                )
            }
        }
    }
}
