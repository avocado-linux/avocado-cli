//! `avocado vm rubicon add|list|remove` — the Rubicon boards this VM hub
//! manages. Desired state lives in `~/.avocado/vm/config.yaml` (survives
//! `vm reset`); every change is pushed to the in-VM agent when it runs.

use anyhow::{bail, Result};

use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::{config::VmConfig, hub, lifecycle, state::VmPaths};

pub enum RubiconAction {
    Add {
        address: String,
        label: Option<String>,
    },
    List,
    Remove {
        address: String,
    },
}

pub struct RubiconCommand {
    pub action: RubiconAction,
}

impl RubiconCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let mut cfg = VmConfig::load(&paths)?;
        let mut list = hub::rubicons(&cfg)?;
        match self.action {
            RubiconAction::Add { address, label } => {
                if address.is_empty() || address.contains(char::is_whitespace) {
                    bail!("not an address: {address:?}");
                }
                match list.iter_mut().find(|r| r.address == address) {
                    Some(r) => r.label = label.or(r.label.take()),
                    None => list.push(hub::RubiconEntry {
                        address: address.clone(),
                        label,
                    }),
                }
                hub::set_rubicons(&mut cfg, &list)?;
                cfg.save(&paths)?;
                print_success(&format!("Rubicon {address} added"), OutputLevel::Normal);
                push(&paths).await
            }
            RubiconAction::Remove { address } => {
                let before = list.len();
                list.retain(|r| r.address != address);
                if list.len() == before {
                    bail!("no Rubicon {address} configured");
                }
                hub::set_rubicons(&mut cfg, &list)?;
                cfg.save(&paths)?;
                print_success(&format!("Rubicon {address} removed"), OutputLevel::Normal);
                push(&paths).await
            }
            RubiconAction::List => list_live(&list).await,
        }
    }
}

/// Push to the agent if the VM is up; otherwise the next `vm start` does.
async fn push(paths: &VmPaths) -> Result<()> {
    if !lifecycle::status().await?.running {
        print_info(
            "VM not running; the change applies on the next start.",
            OutputLevel::Normal,
        );
        return Ok(());
    }
    hub::push_configure(paths, &lifecycle::ssh_target_for_running()?).await
}

async fn list_live(list: &[hub::RubiconEntry]) -> Result<()> {
    if list.is_empty() {
        println!("No Rubicons configured. Add one with `avocado vm rubicon add <address>`.");
        return Ok(());
    }
    let live = if lifecycle::status().await?.running {
        let t = lifecycle::ssh_target_for_running()?;
        hub::agent_request(&t, &serde_json::json!({"op": "inventory"}))
            .await
            .ok()
    } else {
        None
    };
    for r in list {
        let board = live.as_ref().and_then(|v| {
            v["inventory"]["rubicons"]
                .as_array()?
                .iter()
                .find(|b| b["address"] == r.address.as_str())
                .cloned()
        });
        let label = r
            .label
            .as_deref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default();
        match board {
            None => println!("{}{label}: unknown (VM not running)", r.address),
            Some(b) => {
                let state = if b["online"] == true {
                    format!(
                        "online  {} fw {}  dut power={} recovery={}",
                        b["name"].as_str().unwrap_or("?"),
                        b["fw"].as_str().unwrap_or("?"),
                        b["dut"]["power"],
                        b["dut"]["recovery"],
                    )
                } else {
                    format!("offline ({})", b["error"].as_str().unwrap_or("connecting"))
                };
                println!("{}{label}: {state}", r.address);
                for d in b["devices"].as_array().into_iter().flatten() {
                    println!(
                        "    {}  {}:{}{}",
                        d["busid"].as_str().unwrap_or("?"),
                        d["vid"].as_str().unwrap_or("?"),
                        d["pid"].as_str().unwrap_or("?"),
                        if d["exported"] == true {
                            "  exported"
                        } else {
                            ""
                        },
                    );
                }
            }
        }
    }
    Ok(())
}
