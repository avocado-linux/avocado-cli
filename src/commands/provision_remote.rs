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
use std::collections::HashMap;
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
    /// The avocado-vm on another machine this one can ssh to (`--hub
    /// user@host`): jobs go straight to its agent, bundles by rsync, no
    /// Connect in between. Same queue and job as `Local`, one ssh hop on.
    Host { ssh: String },
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
            Hub::Host { ssh } => format!("the avocado-vm on {ssh}"),
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
    /// `--hub user@host`: only that machine's avocado-vm, over ssh.
    pub host: Option<&'a str>,
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
                    let mut b = v["inventory"]["rubicons"]
                        .as_array()?
                        .iter()
                        .find(|b| b["address"] == r.address.as_str())
                        .cloned()?;
                    // The hub's clock, for how long its queue entries have waited.
                    b["hub_now"] = v["now"].clone();
                    Some(b)
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

/// The command that runs `inner` inside `ssh_host`'s avocado-vm: ssh to the
/// host, then through the CLI's own `~/.avocado/vm/ssh-config` there into
/// its VM (the VM's key never leaves that host). `inner` may not hold a
/// single quote.
fn host_vm_command(ssh_host: &str, inner: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15"])
        .arg(ssh_host)
        .arg(format!("ssh -F ~/.avocado/vm/ssh-config avocado-vm '{inner}'"));
    c
}

fn agent_ctl_line(req: &Value) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(req.to_string());
    format!("echo {b64} | base64 -d | avocado-vm-agent ctl")
}

/// The boards of the avocado-vm on `ssh_host`, from its agent's inventory.
pub async fn host_boards(ssh_host: &str) -> Result<Vec<Board>> {
    let mut cmd = host_vm_command(ssh_host, &agent_ctl_line(&json!({"op": "inventory"})));
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await?
        .with_context(|| format!("ssh {ssh_host}"))?;
    if !out.status.success() {
        bail!(
            "the avocado-vm on {ssh_host} did not answer: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("unexpected agent reply from {ssh_host}"))?;
    if v.get("ok") != Some(&json!(true)) {
        bail!("the agent on {ssh_host} refused: {}", v.get("error").unwrap_or(&v));
    }
    Ok(v["inventory"]["rubicons"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut info = r.clone();
            info["hub_now"] = v["now"].clone();
            Board {
                address: r["address"].as_str().unwrap_or_default().to_string(),
                name: s(&r["name"]),
                label: s(&r["label"]),
                hub: Hub::Host { ssh: ssh_host.to_string() },
                info,
                vm_instance: None,
            }
        })
        .collect())
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
    if let Some(h) = scope.host {
        return pick(wanted, host_boards(h).await?)?
            .with_context(|| format!("the avocado-vm on {h} has no Rubicon {wanted:?}"));
    }
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
    if matches!(board.hub, Hub::Connect { .. }) || board.info["claimed"] != true {
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
        Hub::Local | Hub::Host { .. } => {
            let mut params = params;
            if let Some(t) = local_token(board, command, connect_profile).await? {
                params["rubicon_token"] = json!(t);
            }
            params["requested_by"] = json!(requester());
            let req = json!({
                "type": "command",
                "id": uuid::Uuid::new_v4().to_string(),
                "command": command,
                "params": params,
            });
            let r = follow_hub(&board.hub, req).await?;
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

/// Who is asking, for the hub's queue: `user@host`.
pub fn requester() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "someone".into());
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    let host = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..n]).into_owned()
    } else {
        "?".into()
    };
    format!("{user}@{host}")
}

/// A board's queue, holder first, from either hub's record of it.
pub fn queue_of(b: &Board) -> Vec<Value> {
    let q = if b.info["queue"].is_array() {
        &b.info["queue"]
    } else {
        &b.info["state"]["queue"]
    };
    q.as_array().cloned().unwrap_or_default()
}

/// One queue entry: `provision 3f2a1c0d by josh@riptide, 12m`.
pub fn queue_entry(e: &Value, now: u64) -> String {
    let id = e["id"].as_str().unwrap_or("?");
    format!(
        "{} {}{}",
        e["what"].as_str().unwrap_or("?"),
        id.get(..8).unwrap_or(id),
        who_and_age(e, now)
    )
}

/// Who typed on the board's console: `console (writer) by justin@mac, 50m`.
/// Not a job, so it is shown apart from the queue.
pub fn console_writer(b: &Board, now: u64) -> Option<String> {
    let w = if b.info["console_writer"].is_object() {
        &b.info["console_writer"]
    } else {
        &b.info["state"]["console_writer"]
    };
    w.is_object()
        .then(|| format!("console (writer){}", who_and_age(w, now)))
}

/// ` by josh@riptide, 12m`, whichever parts are known.
fn who_and_age(e: &Value, now: u64) -> String {
    let mut s = String::new();
    if let Some(who) = e["requested_by"].as_str() {
        s.push_str(&format!(" by {who}"));
    }
    if let Some(since) = e["since"].as_u64().filter(|&t| t > 0 && t <= now) {
        let m = (now - since) / 60;
        s.push_str(&if m < 60 {
            format!(", {m}m")
        } else {
            format!(", {}h{:02}m", m / 60, m % 60)
        });
    }
    s
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A `command_queued` event as one line.
fn queued_line(v: &Value) -> String {
    let now = v["now"].as_u64().unwrap_or_else(unix_now);
    let ahead: Vec<String> = v["ahead"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| queue_entry(e, now))
        .collect();
    format!(
        "queued: position {} on the board, behind {}",
        v["position"].as_u64().unwrap_or(0),
        ahead.join("; ")
    )
}

/// One request to this machine's VM agent, streamed back line by line. Its
/// own process group: Ctrl-C at the terminal is ours to handle, not ssh's.
fn agent_ctl(hub: &Hub, req: &Value) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let mut cmd = match hub {
        Hub::Host { ssh } => host_vm_command(ssh, &agent_ctl_line(req)),
        _ => {
            let target = lifecycle::ssh_target_for_running()?;
            let mut c = Command::new("ssh");
            c.args(target.base_args()).arg(agent_ctl_line(req));
            c
        }
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("ssh to the VM")
}

/// Cancel a command on a hub this machine talks to directly (its own VM,
/// or one over ssh); the agent's answer.
pub async fn cancel_on(hub: &Hub, id: &str) -> Result<Value> {
    let req = json!({"type": "command", "id": uuid::Uuid::new_v4().to_string(),
                     "command": "cancel", "params": {"id": id}});
    let mut child = agent_ctl(hub, &req)?;
    let stdout = child.stdout.take().unwrap();
    let r = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .find(|v| v["type"] == "command_result")
    })
    .await?;
    let _ = child.wait();
    r.context("no answer from the hub")
}

/// Send one command to this machine's VM agent and follow it; returns its
/// `command_result` line. Ctrl-C cancels the job (a second one stops
/// following and leaves it to the hub).
async fn follow_hub(hub: &Hub, req: Value) -> Result<Value> {
    let id = req["id"].as_str().unwrap_or_default().to_string();
    let mut child = agent_ctl(hub, &req)?;
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
                Some("command_queued") => println!("{}", queued_line(&v)),
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
    });
    tokio::pin!(result);
    let mut cancelled = false;
    let result = loop {
        tokio::select! {
            r = &mut result => break r?,
            _ = tokio::signal::ctrl_c() => {
                if cancelled {
                    let _ = child.kill();
                    bail!("stopped following job {id}; the hub decides when it ends");
                }
                cancelled = true;
                eprintln!("cancelling job {id} (Ctrl-C again to stop following)");
                match cancel_on(hub, &id).await {
                    Ok(r) if r["status"] == "completed" => {}
                    Ok(r) => eprintln!("cancel: {}", r["result"]["error"].as_str().unwrap_or("refused")),
                    Err(e) => eprintln!("cancel: {e:#}"),
                }
            }
        }
    };
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
    /// `--hub user@host`: that machine's avocado-vm, directly over ssh.
    pub hub_host: Option<String>,
    pub vm: Option<String>,
    pub connect_profile: Option<String>,
    pub provision_profile: Option<String>,
    /// Project directory (where `--pack` stages into).
    pub src_dir: PathBuf,
    pub config_path: String,
    /// `--interactive`: a human watches the console; no boot check.
    pub interactive: bool,
}

/// Fail before packing when the project says how to drive a board for some
/// provision profiles but not the chosen one: the hub would skip the board's
/// recovery steps and the flash would wait for a device that never comes.
fn check_rubicon_binding(config_path: &str, target: Option<&str>, profile: Option<&str>) -> Result<()> {
    let Some(profile) = profile.filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    // Composed: extensions (a board's Rubicon support) contribute bindings.
    let composed = crate::utils::config::Config::load_composed(config_path, target)?;
    unbound_profile_error(&composed.config, profile).map_or(Ok(()), |e| bail!(e))
}

fn unbound_profile_error(config: &crate::utils::config::Config, profile: &str) -> Option<String> {
    let profiles = config.provision_profiles.as_ref()?;
    let mut bound: Vec<&str> = profiles
        .iter()
        .filter(|(_, p)| p.rubicon.is_some())
        .map(|(n, _)| n.as_str())
        .collect();
    if bound.is_empty() || bound.contains(&profile) {
        return None;
    }
    bound.sort_unstable();
    Some(format!(
        "provision profile {profile:?} has no Rubicon steps, so the hub would not put the board \
         into recovery; use one that does ({}) or bind it with \
         `provision.{profile}.rubicon: {{ before: ..., after: ... }}`",
        bound.join(", ")
    ))
}

/// A board's address as a single directory name.
fn staging_name(address: &str) -> String {
    let name: String = address
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.chars().all(|c| c == '.') {
        format!("_{name}")
    } else {
        name
    }
}

impl RemoteProvision {
    pub async fn execute(&self, provision: ProvisionCommand) -> Result<()> {
        let wanted = match self.rubicon.trim() {
            "" => default_board(&self.config_path)?,
            w => w.to_string(),
        };
        let scope = Scope {
            via: self.via,
            host: self.hub_host.as_deref(),
            vm: self.vm.as_deref(),
            connect_profile: self.connect_profile.as_deref(),
        };
        check_rubicon_binding(
            &self.config_path,
            provision.target_name(),
            self.provision_profile.as_deref(),
        )?;
        let board = resolve(&wanted, &scope).await?;
        // One staging directory per board: a board runs one job at a time,
        // while provisions to different boards (from one project or several)
        // run side by side and must not clear each other's bundle.
        let out = match &board.hub {
            // Inside the VM's workspace share: the guest reads it in place.
            Hub::Local => {
                let paths = VmPaths::resolve()?;
                share::read_recorded_workspace(&paths)?
                    .map(Ok)
                    .unwrap_or_else(|| share::resolve_workspace(None))?
                    .join(".avocado/hub-bundles")
            }
            Hub::Connect { .. } | Hub::Host { .. } => self.src_dir.join(".avocado/rubicon-bundle"),
        }
        .join(staging_name(&board.address));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out)?;
        // The caller's --env values (per-run choices such as a provisioning
        // profile's ERASE_* options) are not packed into the bundle, which
        // is stored and replayed; they ride with the job instead.
        let job_env = provision.env_vars().cloned().unwrap_or_default();
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
                self.run_local(&bundle, &board.address, token, &job_env)
                    .await
            }
            Hub::Host { ssh } => {
                let token =
                    local_token(&board, "provision", self.connect_profile.as_deref()).await?;
                let r = self
                    .run_host(ssh, &bundle, &board.address, token, &job_env)
                    .await;
                let _ = std::fs::remove_dir_all(&out);
                r
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
                    interactive: self.interactive,
                }
                .execute()
                .await
            }
        }
    }

    /// Hand the job to this machine's VM agent and follow it to the end.
    async fn run_local(
        &self,
        bundle: &Path,
        address: &str,
        token: Option<String>,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let ws = share::read_recorded_workspace(&paths)?
            .map(Ok)
            .unwrap_or_else(|| share::resolve_workspace(None))?;
        let meta: BundleMeta = serde_json::from_slice(&std::fs::read(bundle.join("bundle.json"))?)?;
        let blobs: Vec<Value> = if std::env::var("AVOCADO_HUB_STORE").is_ok_and(|v| v == "1") {
            self.run_local_store(bundle).await?
        } else {
            meta.blobs()
                .iter()
                .map(|b| {
                    let host = bundle.join("blobs").join(&b.sha256);
                    share::translate_to_vm(&host, &ws).map(|vm| {
                        json!({"sha256": b.sha256, "size_bytes": b.size, "url": vm.to_string_lossy()})
                    })
                })
                .collect::<Result<_>>()?
        };
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
                "interactive": self.interactive,
                "bundle": {"name": name, "bundle_json": meta, "blobs": blobs},
                "rubicon_token": token,
                "env": env,
                "requested_by": requester(),
            },
        });
        let r = follow_hub(&Hub::Local, req).await?;
        report(&r, &format!("provision on Rubicon {address}"))?;
        print_success(
            &format!("Provisioned through Rubicon {address}"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

impl RemoteProvision {
    /// Hand the job to the avocado-vm on `ssh_host`. Blobs go by rsync into
    /// a content-addressed cache in that host's VM workspace share (only
    /// the ones it lacks travel), and the job goes straight to its agent.
    async fn run_host(
        &self,
        ssh_host: &str,
        bundle: &Path,
        address: &str,
        token: Option<String>,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let meta: BundleMeta = serde_json::from_slice(&std::fs::read(bundle.join("bundle.json"))?)?;
        if std::env::var("AVOCADO_HUB_STORE").is_ok_and(|v| v == "1") {
            return self.run_host_store(ssh_host, bundle, meta, address, token, env).await;
        }
        // The host's VM workspace share; the cache lives inside it so the
        // guest reads blobs in place.
        let ws = {
            let h = ssh_host.to_string();
            let out = tokio::task::spawn_blocking(move || {
                Command::new("ssh")
                    .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"])
                    .arg(&h)
                    .arg("cat ~/.avocado/vm/workspace")
                    .output()
            })
            .await??;
            let ws = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.status.success() || !ws.starts_with('/') {
                bail!("{ssh_host} has no avocado-vm workspace (~/.avocado/vm/workspace)");
            }
            ws
        };
        let cache = format!("{ws}/.avocado/hub-blobs");
        print_info(
            &format!(
                "Syncing {} blobs to {ssh_host} (only what it lacks)...",
                meta.blobs().len()
            ),
            OutputLevel::Normal,
        );
        let src = format!("{}/", bundle.join("blobs").display());
        let dst = format!("{ssh_host}:{cache}/");
        let st = tokio::task::spawn_blocking(move || {
            Command::new("rsync")
                .args(["-rL", "--ignore-existing", "--partial", "--rsync-path"])
                .arg(format!("mkdir -p {cache} && rsync"))
                .args(["-e", "ssh -o BatchMode=yes"])
                .arg(&src)
                .arg(&dst)
                .status()
        })
        .await?
        .context("rsync")?;
        if !st.success() {
            bail!("syncing the bundle to {ssh_host} failed ({st})");
        }
        let blobs: Vec<Value> = meta
            .blobs()
            .iter()
            .map(|b| {
                json!({"sha256": b.sha256, "size_bytes": b.size,
                       "url": format!("{}/.avocado/hub-blobs/{}", share::VM_MOUNT_POINT, b.sha256)})
            })
            .collect();
        let name = bundle
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        print_info(
            &format!("Provisioning through the avocado-vm on {ssh_host}, Rubicon {address}"),
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
                "env": env,
                "requested_by": requester(),
                "interactive": self.interactive,
            },
        });
        let r = follow_hub(&Hub::Host { ssh: ssh_host.to_string() }, req).await?;
        report(&r, &format!("provision on Rubicon {address}"))?;
        print_success(
            &format!("Provisioned through Rubicon {address} on {ssh_host}"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

impl RemoteProvision {
    /// `--hub` by pull (layer store PoC): the bundle becomes a layer in the local store, is
    /// pushed to a registry (only objects it lacks), and the hub host's store pulls only the
    /// objects *it* lacks. That store lives inside the host's VM workspace share, so the job's
    /// blob URLs point at the store's objects: the hub reads them in place, nothing is copied
    /// again. Needs AVOCADO_LAYER_STORE (local root), AVOCADO_LAYER_REGISTRY
    /// (http://host:port/repo, reachable from both ends) and AVOCADO_HUB_STORE_ROOT (the
    /// hub host's store root, inside its workspace).
    async fn run_host_store(
        &self,
        ssh_host: &str,
        bundle: &Path,
        meta: BundleMeta,
        address: &str,
        token: Option<String>,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        use crate::utils::layer_store as ls;
        let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
        let (root, registry, remote_root) =
            (var("AVOCADO_LAYER_STORE")?, var("AVOCADO_LAYER_REGISTRY")?, var("AVOCADO_HUB_STORE_ROOT")?);
        let t = std::time::Instant::now();
        let (key, rec) = ls::import_bundle("docker", &root, bundle).await?;
        let t_import = t.elapsed().as_secs_f64();
        let push = ls::run_helper("docker", &root, &[], &["push", &registry, &key]).await?;
        let t_push = t.elapsed().as_secs_f64() - t_import;
        let ws = {
            let o = Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", ssh_host, "cat ~/.avocado/vm/workspace"])
                .output()?;
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let rel = remote_root
            .strip_prefix(&ws)
            .with_context(|| format!("{remote_root} is not inside {ssh_host}'s VM workspace {ws}"))?
            .to_string();
        let pull_cmd = format!(
            "docker run --rm -i --privileged --network=host -v {remote_root}:{remote_root}:rshared {} --root {remote_root} pull {registry} {key}",
            ls::HELPER_IMAGE
        );
        let o = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", ssh_host, &pull_cmd])
            .output()?;
        if !o.status.success() {
            bail!("pull on {ssh_host} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        let pull: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
        let t_pull = t.elapsed().as_secs_f64() - t_import - t_push;
        let line = json!({"bundle_layer": key, "files": rec["files"], "import_s": t_import,
            "push": push["xfer"], "push_s": t_push, "pull": pull["xfer"], "pull_s": t_pull});
        print_info(&format!("Layer store sync: {line}"), OutputLevel::Normal);
        if let Ok(f) = std::env::var("AVOCADO_HUB_STORE_REPORT") {
            let _ = std::fs::write(f, line.to_string());
        }
        // Objects at or below the inline size live in the EROFS image, not objects/: a bundle
        // blob that small cannot be served by path.
        if let Some(b) = meta.blobs().iter().find(|b| b.size <= 64) {
            bail!("blob {} is too small to serve from the store ({} B)", b.sha256, b.size);
        }
        let blobs: Vec<Value> = meta
            .blobs()
            .iter()
            .map(|b| json!({"sha256": b.sha256, "size_bytes": b.size, "url": ls::blob_url_in_share(&rel, &b.sha256)}))
            .collect();
        let name = bundle.file_name().unwrap_or_default().to_string_lossy().into_owned();
        print_info(
            &format!("Provisioning through the avocado-vm on {ssh_host}, Rubicon {address} (bundle by pull)"),
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
                "env": env,
                "requested_by": requester(),
                "interactive": self.interactive,
            },
        });
        let r = follow_hub(&Hub::Host { ssh: ssh_host.to_string() }, req).await?;
        report(&r, &format!("provision on Rubicon {address}"))?;
        print_success(&format!("Provisioned through Rubicon {address} on {ssh_host}"), OutputLevel::Normal);
        Ok(())
    }

    /// `--via local` on a hub host whose own store sits in its VM workspace (build on the hub):
    /// import the bundle into that store (a reflink on btrfs: no data copy) and point the job at
    /// the store's objects. Zero bytes move between machines.
    async fn run_local_store(&self, bundle: &Path) -> Result<Vec<Value>> {
        use crate::utils::layer_store as ls;
        let root = std::env::var("AVOCADO_LAYER_STORE").context("AVOCADO_LAYER_STORE is not set")?;
        let paths = VmPaths::resolve()?;
        let ws = share::read_recorded_workspace(&paths)?
            .map(Ok)
            .unwrap_or_else(|| share::resolve_workspace(None))?;
        let rel = Path::new(&root)
            .strip_prefix(&ws)
            .with_context(|| format!("{root} is not inside the VM workspace {}", ws.display()))?
            .to_string_lossy()
            .to_string();
        let t = std::time::Instant::now();
        let (key, rec) = ls::import_bundle("docker", &root, bundle).await?;
        let line = json!({"bundle_layer": key, "files": rec["files"], "new_bytes": rec["new_bytes"],
                          "import_s": t.elapsed().as_secs_f64(), "moved_bytes": 0});
        print_info(&format!("Layer store (local hub): {line}"), OutputLevel::Normal);
        if let Ok(f) = std::env::var("AVOCADO_HUB_STORE_REPORT") {
            let _ = std::fs::write(f, line.to_string());
        }
        let meta: BundleMeta = serde_json::from_slice(&std::fs::read(bundle.join("bundle.json"))?)?;
        if let Some(b) = meta.blobs().iter().find(|b| b.size <= 64) {
            bail!("blob {} is too small to serve from the store ({} B)", b.sha256, b.size);
        }
        Ok(meta
            .blobs()
            .iter()
            .map(|b| json!({"sha256": b.sha256, "size_bytes": b.size, "url": ls::blob_url_in_share(&rel, &b.sha256)}))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_lines_name_the_job_who_and_how_long() {
        let e = json!({"id": "3f2a1c0d-aaaa", "what": "provision",
                       "requested_by": "josh@riptide", "since": 1000});
        assert_eq!(
            queue_entry(&e, 1000 + 12 * 60 + 5),
            "provision 3f2a1c0d by josh@riptide, 12m"
        );
        assert_eq!(
            queue_entry(&e, 1000 + 125 * 60),
            "provision 3f2a1c0d by josh@riptide, 2h05m"
        );
        // Short ids, no requester, a clock behind the hub's: still a line.
        assert_eq!(
            queue_entry(&json!({"id": "k1", "what": "off", "since": 9}), 5),
            "off k1"
        );
        let b = board("10.0.0.1", "r", None, Hub::Local);
        assert!(queue_of(&b).is_empty());
        let mut b = b;
        b.info = json!({"queue": [e.clone()]});
        assert_eq!(console_writer(&b, 3000), None);
        assert_eq!(queue_of(&b).len(), 1);
        b.info = json!({"state": {"queue": [e]}});
        assert_eq!(queue_of(&b).len(), 1);
        // A console writer is shown apart from the queue, not as a job.
        b.info = json!({"queue": [], "console_writer":
                        {"id": "console-3", "what": "console", "requested_by": "justin@mac", "since": 0}});
        assert_eq!(
            console_writer(&b, 3000).as_deref(),
            Some("console (writer) by justin@mac")
        );
        b.info = json!({"state": {"console_writer": {"what": "console", "since": 600}}});
        assert_eq!(
            console_writer(&b, 3600).as_deref(),
            Some("console (writer), 50m")
        );
    }

    #[test]
    fn each_board_stages_its_bundle_in_its_own_directory() {
        // Two concurrent provisions to different boards once shared one
        // directory, and the second cleared the first's bundle mid-job.
        assert_ne!(staging_name("10.12.0.97"), staging_name("10.12.0.18"));
        assert_eq!(staging_name("10.12.0.97"), "10.12.0.97");
        assert_eq!(staging_name("fe80::1%eth0"), "fe80__1_eth0");
        assert_eq!(staging_name("../x"), ".._x");
        assert_eq!(staging_name(".."), "_..");
        assert_eq!(staging_name(""), "_");
    }

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

    #[test]
    fn an_unbound_provision_profile_is_refused_before_packing() {
        let c: crate::utils::config::Config = serde_yaml::from_str(
            "provision:\n  tegraflash:\n    rubicon: { before: r, after: b }\n  tegraflash-sd: {}\n",
        )
        .unwrap();
        assert!(unbound_profile_error(&c, "tegraflash").is_none());
        let e = unbound_profile_error(&c, "tegraflash-sd").unwrap();
        assert!(e.contains("tegraflash-sd") && e.contains("(tegraflash)"), "{e}");
        // A project with no Rubicon bindings at all is left to the hub.
        let none: crate::utils::config::Config =
            serde_yaml::from_str("provision:\n  usb: {}\n").unwrap();
        assert!(unbound_profile_error(&none, "usb").is_none());
    }
}
