//! Reaching a Rubicon through the avocado-vm hub that drives it, for
//! `avocado provision --rubicon <board>` and `avocado rubicon …`.
//!
//! Which hub:
//! - local: this machine's avocado-vm has the board in its Rubicon list.
//!   Commands go to the agent over SSH; a provision's bundle is packed into
//!   the VM's workspace share, so nothing is copied.
//! - connect: otherwise, the org's VMs (as Connect sees them) are searched
//!   for the board. Commands are queued through Connect; a provision
//!   uploads only the blobs Connect lacks. Works from anywhere.
//!
//! A board is named by its firmware name, its label or its address. A name
//! that matches several boards is an error listing them: never the first.
//!
//! The provision itself is packed here (the normal provision path,
//! stopping at the hook; see `provision_bundle`) and run by the hub's job
//! runner next to the board. Its log streams back here and its result is
//! this command's exit code.

use anyhow::{bail, Context, Result};
use base64::Engine;
use reqwest::Method;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::commands::connect::client::ConnectClient;
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

#[derive(Clone, Debug, PartialEq)]
pub enum Hub {
    /// This machine's avocado-vm.
    Local,
    /// A VM in the Connect org.
    Connect { vm: String, vm_name: String },
}

/// A Rubicon as one hub reports it. `info` is that hub's record of it (the
/// agent's inventory entry, or Connect's Rubicon row).
#[derive(Clone, Debug)]
pub struct Board {
    pub address: String,
    pub name: Option<String>,
    pub label: Option<String>,
    pub hub: Hub,
    pub info: Value,
    /// Connect boards: the instance id of the VM that reported it (this
    /// machine's own VM shows up in Connect too).
    pub vm_instance: Option<String>,
}

impl Board {
    /// How to name this board unambiguously in a list of candidates.
    pub fn describe(&self) -> String {
        let hub = match &self.hub {
            Hub::Local => "this machine's VM".to_string(),
            Hub::Connect { vm_name, .. } => format!("VM {vm_name}"),
        };
        let mut s = self.name.clone().unwrap_or_else(|| self.address.clone());
        if let Some(l) = &self.label {
            s.push_str(&format!(" ({l})"));
        }
        format!("{s} at {} on {hub}", self.address)
    }
}

/// Where to look for boards.
pub struct Scope<'a> {
    pub via: Via,
    /// Connect only: which VM, when several know a board.
    pub vm: Option<&'a str>,
    pub connect_profile: Option<&'a str>,
}

/// Does `wanted` name this board (label, firmware name, or address)?
fn matches(wanted: &str, address: &str, label: Option<&str>, name: Option<&str>) -> bool {
    let w = wanted.trim();
    [Some(address), label, name]
        .into_iter()
        .flatten()
        .any(|v| v.eq_ignore_ascii_case(w))
}

fn s(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// This machine's VM's boards: its configured list, with each board's live
/// state when the VM is running. Empty when there is no VM here.
pub async fn local_boards() -> Result<Vec<Board>> {
    let Ok(paths) = VmPaths::resolve() else {
        return Ok(vec![]);
    };
    let cfg = VmConfig::load(&paths)?;
    let list = hub::rubicons(&cfg)?;
    if list.is_empty() {
        return Ok(vec![]);
    }
    let live = if lifecycle::status().await?.running {
        let t = lifecycle::ssh_target_for_running()?;
        hub::agent_request(&t, &json!({"op": "inventory"}))
            .await
            .ok()
    } else {
        None
    };
    Ok(list
        .into_iter()
        .map(|r| {
            let info = live
                .as_ref()
                .and_then(|v| {
                    v["inventory"]["rubicons"]
                        .as_array()?
                        .iter()
                        .find(|b| b["address"] == r.address.as_str())
                        .cloned()
                })
                .unwrap_or(Value::Null);
            Board {
                name: s(&info["name"]),
                label: r.label,
                address: r.address,
                hub: Hub::Local,
                info,
                vm_instance: None,
            }
        })
        .collect())
}

/// This machine's VM's instance id, if it has one (read, never created).
pub fn local_instance() -> Option<String> {
    let paths = VmPaths::resolve().ok()?;
    std::fs::read_to_string(paths.instance_id_file())
        .ok()
        .map(|s| s.trim().to_lowercase())
}

/// The boards of the org's VMs, as Connect sees them.
pub async fn connect_boards(profile: Option<&str>, vm: Option<&str>) -> Result<Vec<Board>> {
    let (c, org) = connect(profile, None)?;
    let vms = c
        .json(Method::GET, &format!("/api/orgs/{org}/vms"), None)
        .await?;
    let mut out = Vec::new();
    for v in vms["data"].as_array().into_iter().flatten() {
        let id = v["id"].as_str().unwrap_or_default();
        if vm.is_some_and(|want| want != id) {
            continue;
        }
        let vm_name = s(&v["vm"]["hostname"])
            .or(s(&v["name"]))
            .unwrap_or_else(|| id.to_string());
        for r in v["rubicons"].as_array().into_iter().flatten() {
            out.push(Board {
                address: r["address"].as_str().unwrap_or_default().to_string(),
                name: s(&r["name"]),
                label: s(&r["label"]),
                hub: Hub::Connect {
                    vm: id.to_string(),
                    vm_name: vm_name.clone(),
                },
                info: r.clone(),
                vm_instance: s(&v["vm"]["instance_id"]),
            });
        }
    }
    Ok(out)
}

/// The one board in `boards` that `wanted` names, if any; several is an
/// error that lists them.
pub fn pick(wanted: &str, boards: Vec<Board>) -> Result<Option<Board>> {
    let mut found: Vec<Board> = boards
        .into_iter()
        .filter(|b| matches(wanted, &b.address, b.label.as_deref(), b.name.as_deref()))
        .collect();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => bail!(
            "{wanted:?} matches several Rubicons; name one by its name or address:\n  {}{}",
            found
                .iter()
                .map(Board::describe)
                .collect::<Vec<_>>()
                .join("\n  "),
            if found.iter().any(|b| matches!(b.hub, Hub::Connect { .. })) {
                "\n(--vm picks the VM)"
            } else {
                ""
            }
        ),
    }
}

/// The board `wanted` names: this machine's VM first, then Connect.
pub async fn resolve(wanted: &str, scope: &Scope<'_>) -> Result<Board> {
    if scope.via != Via::Connect {
        if let Some(b) = pick(wanted, local_boards().await?)? {
            if !lifecycle::status().await?.running {
                bail!(
                    "Rubicon {:?} belongs to this machine's avocado-vm, which is not running (`avocado vm start`)",
                    wanted
                );
            }
            return Ok(b);
        }
        if scope.via == Via::Local {
            bail!("this machine's avocado-vm has no Rubicon {wanted:?} (`avocado rubicon list`)");
        }
    }
    pick(
        wanted,
        connect_boards(scope.connect_profile, scope.vm).await?,
    )?
    .with_context(|| format!("no VM in this Connect org drives a Rubicon named {wanted:?}"))
}

/// The board to use when none is named: the project's `rubicon.default`.
pub fn default_board(config_path: &str) -> Result<String> {
    let config = crate::utils::config::Config::load(config_path)?;
    config
        .rubicon
        .as_ref()
        .and_then(|r| r.default.clone())
        .context(
            "no Rubicon named, and the project sets no `rubicon.default`; \
             name one (`avocado rubicon list` shows them)",
        )
}

/// The scopes a hub command uses on a claimed board (as Connect mints them
/// for its own commands).
fn command_scopes(command: &str) -> &'static [&'static str] {
    match command {
        "rubicon_dut" => &["dut"],
        "rubicon_update" => &["update"],
        "rubicon_console" => &["console"],
        "rubicon_claim" => &["admin"],
        _ => &["dut", "usbip", "console"],
    }
}

/// A token for a command through this machine's VM on a claimed board,
/// from Connect (Connect adds its own to commands it relays). `None` for an
/// unclaimed board.
pub async fn local_token(
    board: &Board,
    command: &str,
    connect_profile: Option<&str>,
) -> Result<Option<String>> {
    if board.hub != Hub::Local || board.info["claimed"] != true {
        return Ok(None);
    }
    let name = board
        .name
        .as_deref()
        .context("the board is claimed but has not said its name yet")?;
    let (c, org) = connect(connect_profile, None).context(
        "the board is claimed: commands need a token from Connect (`avocado connect auth login`)",
    )?;
    let r = c
        .json(
            Method::POST,
            &format!("/api/orgs/{org}/rubicon-tokens"),
            Some(&json!({"name": name, "scopes": command_scopes(command)})),
        )
        .await
        .with_context(|| format!("token for {name} ({})", command_scopes(command).join(", ")))?;
    Ok(r["data"]["token"].as_str().map(String::from))
}

/// Run a hub command on `board` and follow it to its result: the agent's
/// `command_result` line (local) or Connect's command record, normalized
/// to `{status, result}`. The job log streams to stdout.
pub async fn run_command(
    board: &Board,
    command: &str,
    params: Value,
    connect_profile: Option<&str>,
) -> Result<Value> {
    match &board.hub {
        Hub::Local => {
            let mut params = params;
            if let Some(t) = local_token(board, command, connect_profile).await? {
                params["rubicon_token"] = json!(t);
            }
            let req = json!({
                "type": "command",
                "id": uuid::Uuid::new_v4().to_string(),
                "command": command,
                "params": params,
            });
            let r = follow_local(req).await?;
            Ok(json!({"status": r["status"], "result": r["result"]}))
        }
        Hub::Connect { vm, .. } => {
            let (c, org) = connect(connect_profile, None)?;
            let cmd = c
                .json(
                    Method::POST,
                    &format!("/api/orgs/{org}/vms/{vm}/commands"),
                    Some(&json!({"command": command, "params": params})),
                )
                .await?;
            let id = cmd["data"]["id"].as_str().context("no command id")?;
            follow_connect(&c, &org, vm, id).await
        }
    }
}

/// Follow a Connect command to its end; `{status, result}`.
pub async fn follow_connect(c: &ConnectClient, org: &str, vm: &str, id: &str) -> Result<Value> {
    let base = format!("/api/orgs/{org}/vms/{vm}/commands/{id}");
    let mut after: i64 = -1;
    loop {
        let logs = c
            .json(Method::GET, &format!("{base}/logs?after_seq={after}"), None)
            .await?;
        for chunk in logs["data"].as_array().into_iter().flatten() {
            for l in chunk["lines"].as_array().into_iter().flatten() {
                println!("{}", l.as_str().unwrap_or_default());
            }
        }
        if let Some(n) = logs["next_after_seq"].as_i64() {
            after = n;
        }
        let state = c.json(Method::GET, &base, None).await?;
        let st = state["data"]["state"].as_str().unwrap_or("");
        if matches!(st, "completed" | "failed" | "expired") {
            return Ok(json!({"status": st, "result": state["data"]["result"]}));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Send one command to this machine's VM agent and follow it; returns its
/// `command_result` line.
async fn follow_local(req: Value) -> Result<Value> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(req.to_string());
    let target = lifecycle::ssh_target_for_running()?;
    let mut child = Command::new("ssh")
        .args(target.base_args())
        .arg(format!("echo {b64} | base64 -d | avocado-vm-agent ctl"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("ssh to the VM")?;
    let stdout = child.stdout.take().unwrap();
    // Blocking reads on a dedicated thread; a job takes minutes.
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
    result.context("the hub connection closed before the job finished (it keeps running in the VM)")
}

/// Print a job's checks; Err unless it completed and passed.
pub fn report(r: &Value, what: &str) -> Result<()> {
    for check in r["result"]["checks"].as_array().into_iter().flatten() {
        println!(
            "check {}: {}",
            check["name"].as_str().unwrap_or("?"),
            check["status"].as_str().unwrap_or("?")
        );
    }
    if r["status"] != "completed" || r["result"]["status"] != "pass" {
        bail!(
            "{what} did not pass: {}",
            r["result"]["error"]
                .as_str()
                .or(r["result"]["status"].as_str())
                .or(r["status"].as_str())
                .unwrap_or("failed")
        );
    }
    Ok(())
}

pub struct RemoteProvision {
    /// The board; empty: the project's `rubicon.default`.
    pub rubicon: String,
    pub via: Via,
    pub vm: Option<String>,
    pub connect_profile: Option<String>,
    pub provision_profile: Option<String>,
    /// Project directory (where `--pack` stages into).
    pub src_dir: PathBuf,
    pub config_path: String,
}

impl RemoteProvision {
    pub async fn execute(&self, provision: ProvisionCommand) -> Result<()> {
        let wanted = match self.rubicon.trim() {
            "" => default_board(&self.config_path)?,
            w => w.to_string(),
        };
        let scope = Scope {
            via: self.via,
            vm: self.vm.as_deref(),
            connect_profile: self.connect_profile.as_deref(),
        };
        let board = resolve(&wanted, &scope).await?;
        let out = match &board.hub {
            // Inside the VM's workspace share: the guest reads it in place.
            Hub::Local => {
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
        match &board.hub {
            Hub::Local => {
                let token =
                    local_token(&board, "provision", self.connect_profile.as_deref()).await?;
                self.run_local(&bundle, &board.address, token).await
            }
            Hub::Connect { vm, vm_name } => {
                let (c, org) = connect(self.connect_profile.as_deref(), None)?;
                print_info(
                    &format!(
                        "Syncing to Connect for VM {vm_name}, Rubicon {}...",
                        board.address
                    ),
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
                    vm: vm.clone(),
                    rubicon: board.address.clone(),
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
    async fn run_local(&self, bundle: &Path, address: &str, token: Option<String>) -> Result<()> {
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
        let name = bundle
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        print_info(
            &format!("Provisioning through this machine's avocado-vm, Rubicon {address}"),
            OutputLevel::Normal,
        );
        let req = json!({
            "type": "command",
            "id": uuid::Uuid::new_v4().to_string(),
            "command": "provision",
            "params": {
                "address": address,
                "profile": self.provision_profile,
                "bundle": {"name": name, "bundle_json": meta, "blobs": blobs},
                "rubicon_token": token,
            },
        });
        let r = follow_local(req).await?;
        report(&r, &format!("provision on Rubicon {address}"))?;
        print_success(
            &format!("Provisioned through Rubicon {address}"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(address: &str, name: &str, label: Option<&str>, hub: Hub) -> Board {
        Board {
            address: address.into(),
            name: Some(name.into()),
            label: label.map(String::from),
            hub,
            info: Value::Null,
            vm_instance: None,
        }
    }

    #[test]
    fn rubicon_matches_label_name_or_address() {
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

    #[test]
    fn an_ambiguous_name_is_an_error_listing_the_boards_never_the_first() {
        let vm = |n: &str| Hub::Connect {
            vm: n.into(),
            vm_name: n.into(),
        };
        let boards = vec![
            board("10.0.0.1", "rubicon-a", Some("bench"), Hub::Local),
            board("10.0.0.2", "rubicon-b", Some("bench"), vm("lab")),
            board("10.0.0.3", "rubicon-c", None, vm("lab")),
        ];
        let e = pick("bench", boards.clone()).unwrap_err().to_string();
        assert!(
            e.contains("rubicon-a") && e.contains("rubicon-b") && e.contains("--vm"),
            "{e}"
        );
        assert_eq!(
            pick("rubicon-b", boards.clone()).unwrap().unwrap().address,
            "10.0.0.2"
        );
        assert!(pick("nope", boards).unwrap().is_none());
    }
}
