//! `avocado provision --rubicon <board>`: provision on a remote device
//! through the avocado-vm hub that drives that Rubicon.
//!
//! The provision is packed here (the normal provision path, stopping at the
//! hook; see `provision_bundle`), the bundle is brought to the hub, and the
//! hub's job runner does the rest on the LAN next to the board: recovery,
//! USB/IP attach, the provisioning hook, the boot check. The job's log
//! streams back here and its result is this command's exit code.
//!
//! Which hub:
//! - local: this machine's avocado-vm is running and has the board in its
//!   Rubicon list. The bundle is packed into the VM's workspace share, so
//!   nothing is copied; the job goes to the agent over SSH.
//! - connect: otherwise, the org's VMs (as Connect sees them) are searched
//!   for the board. Only blobs Connect lacks are uploaded, then the job is
//!   queued for that VM. Works from anywhere.

use anyhow::{bail, Context, Result};
use base64::Engine;
use reqwest::Method;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::commands::connect::hub::{connect, upload_bundle, ProvisionJobCommand};
use crate::commands::provision::ProvisionCommand;
use crate::commands::provision_bundle::BundleMeta;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::{config::VmConfig, hub, lifecycle, share, state::VmPaths};

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Via {
    /// This machine's VM if it drives the board, else Connect.
    Auto,
    /// Only this machine's avocado-vm.
    Local,
    /// Only through Connect.
    Connect,
}

pub struct RemoteProvision {
    pub rubicon: String,
    pub via: Via,
    /// Connect only: which VM, when several know a board by this name.
    pub vm: Option<String>,
    pub connect_profile: Option<String>,
    pub provision_profile: Option<String>,
    /// Project directory (where `--pack` stages into).
    pub src_dir: PathBuf,
}

/// Does `wanted` name this board (label, hello name, or address)?
fn matches(wanted: &str, address: &str, label: Option<&str>, name: Option<&str>) -> bool {
    let w = wanted.trim();
    [Some(address), label, name]
        .into_iter()
        .flatten()
        .any(|v| v.eq_ignore_ascii_case(w))
}

enum Hub {
    Local {
        address: String,
    },
    Connect {
        vm: String,
        vm_name: String,
        address: String,
    },
}

impl RemoteProvision {
    async fn local_hub(&self) -> Result<Option<String>> {
        let paths = VmPaths::resolve()?;
        let cfg = VmConfig::load(&paths)?;
        let Some(r) = hub::rubicons(&cfg)?
            .into_iter()
            .find(|r| matches(&self.rubicon, &r.address, r.label.as_deref(), None))
        else {
            return Ok(None);
        };
        if !lifecycle::status().await?.running {
            bail!(
                "Rubicon {:?} belongs to this machine's avocado-vm, which is not running (`avocado vm start`)",
                self.rubicon
            );
        }
        Ok(Some(r.address))
    }

    async fn connect_hub(&self) -> Result<Hub> {
        let (c, org) = connect(self.connect_profile.as_deref(), None)?;
        let vms = c
            .json(Method::GET, &format!("/api/orgs/{org}/vms"), None)
            .await?;
        let mut found = Vec::new();
        for vm in vms["data"].as_array().into_iter().flatten() {
            let id = vm["id"].as_str().unwrap_or_default();
            if self.vm.as_deref().is_some_and(|want| want != id) {
                continue;
            }
            for r in vm["rubicons"].as_array().into_iter().flatten() {
                let address = r["address"].as_str().unwrap_or_default();
                if matches(
                    &self.rubicon,
                    address,
                    r["label"].as_str(),
                    r["name"].as_str(),
                ) {
                    let vm_name = vm["vm"]["hostname"]
                        .as_str()
                        .or(vm["name"].as_str())
                        .unwrap_or(id)
                        .to_string();
                    found.push(Hub::Connect {
                        vm: id.into(),
                        vm_name,
                        address: address.into(),
                    });
                }
            }
        }
        match found.len() {
            1 => Ok(found.pop().unwrap()),
            0 => bail!(
                "no VM in this Connect org drives a Rubicon named {:?}",
                self.rubicon
            ),
            n => bail!(
                "{n} VMs drive a Rubicon named {:?}; pick one with --vm",
                self.rubicon
            ),
        }
    }

    async fn resolve(&self) -> Result<Hub> {
        if self.via != Via::Connect {
            if let Some(address) = self.local_hub().await? {
                return Ok(Hub::Local { address });
            }
            if self.via == Via::Local {
                bail!(
                    "this machine's avocado-vm has no Rubicon {:?} (`avocado vm rubicon list`)",
                    self.rubicon
                );
            }
        }
        self.connect_hub().await
    }

    pub async fn execute(&self, provision: ProvisionCommand) -> Result<()> {
        let hub = self.resolve().await?;
        let out = match &hub {
            // Inside the VM's workspace share: the guest reads it in place.
            Hub::Local { .. } => {
                let paths = VmPaths::resolve()?;
                share::read_recorded_workspace(&paths)?
                    .map(Ok)
                    .unwrap_or_else(|| share::resolve_workspace(None))?
                    .join(".avocado/hub-bundles")
            }
            Hub::Connect { .. } => self.src_dir.join(".avocado/rubicon-bundle"),
        };
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out)?;
        print_info("Packing the provision for the hub...", OutputLevel::Normal);
        provision.with_pack(Some(out.clone())).execute().await?;
        let bundle = std::fs::read_dir(&out)?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "bundle"))
            .context("the pack produced no bundle")?;
        match hub {
            Hub::Local { address } => self.run_local(&bundle, &address).await,
            Hub::Connect {
                vm,
                vm_name,
                address,
            } => {
                let (c, org) = connect(self.connect_profile.as_deref(), None)?;
                print_info(
                    &format!("Syncing to Connect for VM {vm_name}, Rubicon {address}..."),
                    OutputLevel::Normal,
                );
                let name = bundle
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let id = upload_bundle(&c, &org, &bundle, &name, json!({})).await?;
                let _ = std::fs::remove_dir_all(&out);
                ProvisionJobCommand {
                    vm,
                    rubicon: address,
                    bundle: Some(id),
                    runtime: None,
                    profile: self.provision_profile.clone(),
                    repository: None,
                    pr_url: None,
                    commit: None,
                    run_url: None,
                    wait: true,
                    org: Some(org),
                    profile_name: self.connect_profile.clone(),
                }
                .execute()
                .await
            }
        }
    }

    /// Hand the job to this machine's VM agent and follow it to the end.
    async fn run_local(&self, bundle: &Path, address: &str) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let ws = share::read_recorded_workspace(&paths)?
            .map(Ok)
            .unwrap_or_else(|| share::resolve_workspace(None))?;
        let meta: BundleMeta = serde_json::from_slice(&std::fs::read(bundle.join("bundle.json"))?)?;
        let blobs: Vec<Value> = meta
            .blobs()
            .iter()
            .map(|b| {
                let host = bundle.join("blobs").join(&b.sha256);
                share::translate_to_vm(&host, &ws).map(|vm| {
                    json!({"sha256": b.sha256, "size_bytes": b.size, "url": vm.to_string_lossy()})
                })
            })
            .collect::<Result<_>>()?;
        let id = uuid::Uuid::new_v4().to_string();
        let name = bundle
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let req = json!({
            "type": "command",
            "id": id,
            "command": "provision",
            "params": {
                "address": address,
                "profile": self.provision_profile,
                "bundle": {"name": name, "bundle_json": meta, "blobs": blobs},
            },
        });
        let b64 = base64::engine::general_purpose::STANDARD.encode(req.to_string());
        let target = lifecycle::ssh_target_for_running()?;
        print_info(
            &format!("Provisioning through this machine's avocado-vm, Rubicon {address}"),
            OutputLevel::Normal,
        );
        let mut child = Command::new("ssh")
            .args(target.base_args())
            .arg(format!("echo {b64} | base64 -d | avocado-vm-agent ctl"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("ssh to the VM")?;
        let stdout = child.stdout.take().unwrap();
        // Blocking reads on a dedicated thread; the job takes minutes.
        let result = tokio::task::spawn_blocking(move || {
            let mut result = None;
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match v["type"].as_str() {
                    Some("command_ack") => {
                        println!("job {} accepted by the hub", v["id"].as_str().unwrap_or(""))
                    }
                    Some("command_log") => {
                        for l in v["lines"].as_array().into_iter().flatten() {
                            println!("{}", l.as_str().unwrap_or_default());
                        }
                    }
                    Some("command_result") => result = Some(v),
                    _ => {}
                }
            }
            result
        })
        .await?;
        let _ = child.wait();
        let Some(r) = result else {
            bail!("the hub connection closed before the job finished (it keeps running in the VM)");
        };
        for check in r["result"]["checks"].as_array().into_iter().flatten() {
            println!(
                "check {}: {}",
                check["name"].as_str().unwrap_or("?"),
                check["status"].as_str().unwrap_or("?")
            );
        }
        if r["status"] != "completed" || r["result"]["status"] != "pass" {
            bail!(
                "provision on Rubicon {address} did not pass: {}",
                r["result"]["error"]
                    .as_str()
                    .unwrap_or(r["result"]["status"].as_str().unwrap_or("failed"))
            );
        }
        print_success(
            &format!("Provisioned through Rubicon {address}"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rubicon_matches_label_name_or_address() {
        use super::matches;
        assert!(matches(
            "jetson bench",
            "192.168.1.52",
            Some("Jetson Bench"),
            None
        ));
        assert!(matches(
            "rubicon-1206c2",
            "192.168.1.52",
            None,
            Some("rubicon-1206c2")
        ));
        assert!(matches(" 192.168.1.52 ", "192.168.1.52", None, None));
        assert!(!matches(
            "jetson",
            "192.168.1.52",
            Some("jetson bench"),
            None
        ));
    }
}
