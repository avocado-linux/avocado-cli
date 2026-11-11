//! `avocado rubicon …`: the Rubicon boards reachable from here (this
//! machine's avocado-vm, and the org's VMs through Connect), and what can
//! be done with one: run a run profile from the project, watch or type on
//! its DUT's serial console. Boards resolve as in `provision --rubicon`
//! (see `provision_remote`).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::process::Command;

use crate::commands::provision_remote::{self as remote, Board, Hub, Scope, Via};
use crate::commands::vm::rubicon::{RubiconAction as VmAction, RubiconCommand as VmRubicon};
use crate::utils::config::Config;
use crate::utils::output::{print_success, OutputLevel};
use crate::utils::vm::lifecycle;

pub enum RubiconAction {
    List,
    Show {
        board: Option<String>,
    },
    Add {
        address: String,
        label: Option<String>,
    },
    Remove {
        board: String,
    },
    Label {
        board: String,
        label: String,
    },
    Run {
        board: Option<String>,
        profile: String,
    },
    Console {
        board: Option<String>,
        read_only: bool,
    },
    Logs {
        board: Option<String>,
        follow: bool,
    },
}

pub struct RubiconCommand {
    pub action: RubiconAction,
    pub config_path: String,
    pub via: Via,
    pub vm: Option<String>,
    pub connect_profile: Option<String>,
}

impl RubiconCommand {
    fn scope(&self) -> Scope<'_> {
        Scope {
            via: self.via,
            vm: self.vm.as_deref(),
            connect_profile: self.connect_profile.as_deref(),
        }
    }

    /// The named board, else the project's `rubicon.default`.
    async fn board(&self, wanted: &Option<String>) -> Result<Board> {
        let wanted = match wanted {
            Some(w) => w.clone(),
            None => remote::default_board(&self.config_path)?,
        };
        remote::resolve(&wanted, &self.scope()).await
    }

    /// The project config, if there is one here (for run profiles, the
    /// console rate).
    fn project(&self) -> Option<Config> {
        Config::load_composed(&self.config_path, None)
            .ok()
            .map(|c| c.config)
    }

    pub async fn execute(self) -> Result<()> {
        match &self.action {
            RubiconAction::List => self.list().await,
            RubiconAction::Show { board } => {
                let b = self.board(board).await?;
                show(&b);
                Ok(())
            }
            // Boards are added to this machine's VM; Connect learns of them
            // from its inventory.
            RubiconAction::Add { address, label } => {
                VmRubicon {
                    action: VmAction::Add {
                        address: address.clone(),
                        label: label.clone(),
                    },
                }
                .execute()
                .await
            }
            RubiconAction::Remove { board } => {
                let b = self.local(board).await?;
                VmRubicon {
                    action: VmAction::Remove { address: b.address },
                }
                .execute()
                .await
            }
            RubiconAction::Label { board, label } => {
                let b = self.local(board).await?;
                VmRubicon {
                    action: VmAction::Add {
                        address: b.address,
                        label: Some(label.clone()),
                    },
                }
                .execute()
                .await
            }
            RubiconAction::Run { board, profile } => self.run(board, profile).await,
            RubiconAction::Console { board, read_only } => {
                let b = self.board(board).await?;
                self.console(&b, !read_only, true, true)
            }
            RubiconAction::Logs { board, follow } => {
                let b = self.board(board).await?;
                self.console(&b, false, true, *follow)
            }
        }
    }

    /// A board of this machine's VM (adding, removing and labelling edit
    /// that VM's list).
    async fn local(&self, wanted: &str) -> Result<Board> {
        remote::pick(wanted, remote::local_boards().await?)?.with_context(|| {
            format!("this machine's avocado-vm has no Rubicon {wanted:?} (`avocado rubicon list`)")
        })
    }

    async fn list(&self) -> Result<()> {
        let mut boards = Vec::new();
        if self.via != Via::Connect {
            boards.extend(remote::local_boards().await?);
        }
        if self.via != Via::Local {
            match remote::connect_boards(self.connect_profile.as_deref(), self.vm.as_deref()).await
            {
                Ok(c) => boards.extend(c),
                Err(e) if self.via == Via::Auto => {
                    eprintln!("(Connect not consulted: {e:#})");
                }
                Err(e) => return Err(e),
            }
        }
        // This machine's VM is also one of the org's VMs: show each board
        // once, as seen from here.
        if self.via != Via::Connect {
            if let Some(me) = remote::local_instance() {
                boards.retain(|b| b.vm_instance.as_deref() != Some(me.as_str()));
            }
        }
        let mut seen = HashSet::new();
        boards.retain(|b| match &b.name {
            Some(n) => seen.insert(n.clone()),
            None => true,
        });
        if boards.is_empty() {
            println!(
                "No Rubicons. Add one to this machine's VM with `avocado rubicon add <address>`."
            );
            return Ok(());
        }
        println!(
            "{:<18} {:<16} {:<16} {:<20} STATE",
            "NAME", "LABEL", "ADDRESS", "HUB"
        );
        for b in &boards {
            println!(
                "{:<18} {:<16} {:<16} {:<20} {}",
                b.name.as_deref().unwrap_or("-"),
                b.label.as_deref().unwrap_or("-"),
                b.address,
                match &b.hub {
                    Hub::Local => "this machine".to_string(),
                    Hub::Connect { vm_name, .. } => format!("VM {vm_name}"),
                },
                state(b)
            );
        }
        Ok(())
    }

    async fn run(&self, board: &Option<String>, profile: &str) -> Result<()> {
        let config = self.project().context(
            "`avocado rubicon run` reads run profiles from the project: no avocado.yaml here",
        )?;
        let steps = config.rubicon_steps(profile)?;
        let b = self.board(board).await?;
        let params = json!({"address": b.address, "name": profile, "steps": steps});
        let r =
            remote::run_command(&b, "rubicon_run", params, self.connect_profile.as_deref()).await?;
        remote::report(&r, &format!("{profile} on {}", b.describe()))?;
        print_success(
            &format!("{profile} ran on {}", b.describe()),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// The board's serial console through this machine's VM: interactive
    /// (`write`), or its history and then (`follow`) what it prints.
    fn console(&self, b: &Board, write: bool, history: bool, follow: bool) -> Result<()> {
        if !matches!(b.hub, Hub::Local) {
            bail!(
                "the console of {} is only reachable from that VM's host for now \
                 (a console through Connect is a follow-up)",
                b.describe()
            );
        }
        let baud = self.project().and_then(|c| c.rubicon?.console?.baud);
        let mut cmd = format!("avocado-vm-agent console {}", sh_quote(&b.address));
        for (on, flag) in [
            (write, " --write"),
            (history, " --history"),
            (!follow, " --no-follow"),
        ] {
            if on {
                cmd.push_str(flag);
            }
        }
        if let Some(baud) = baud {
            cmd.push_str(&format!(" --baud {baud}"));
        }
        let target = lifecycle::ssh_target_for_running()?;
        let mut ssh = Command::new("ssh");
        ssh.args(target.base_args());
        if write {
            use crate::utils::interactivity::{ContainerStdio, StdioCapabilities};
            if !ContainerStdio::decide(true, &StdioCapabilities::detect()).allocates_tty() {
                bail!("an interactive console needs a terminal; `avocado rubicon logs -f` watches without one");
            }
            eprintln!("[console of {}; Ctrl-] to leave]", b.describe());
            // A remote tty in raw mode: keys go to the DUT as typed.
            ssh.args(["-o", "RequestTTY=force"])
                .arg(format!("stty raw -echo; exec {cmd}"));
        } else {
            ssh.arg(cmd);
        }
        let st = ssh.status().context("ssh to the VM")?;
        if write {
            eprintln!();
        }
        if !st.success() {
            bail!("console session ended with {st}");
        }
        Ok(())
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One line of board state, from whichever hub reported it.
fn state(b: &Board) -> String {
    let i = &b.info;
    if i.is_null() {
        return "unknown (VM not running)".into();
    }
    if i["online"] != true {
        return format!("offline ({})", i["error"].as_str().unwrap_or("connecting"));
    }
    // Protocol 2 boards report their generic outputs.
    let io = if i["io"].is_object() {
        &i["io"]
    } else {
        &i["state"]["io"]
    };
    if let Some(io) = io.as_object().filter(|o| !o.is_empty()) {
        let outs: Vec<String> = io
            .iter()
            .map(|(k, v)| format!("{k} {}", if v == true { "on" } else { "off" }))
            .collect();
        return format!(
            "online  fw {}  {}",
            i["fw"].as_str().unwrap_or("?"),
            outs.join(", ")
        );
    }
    let dut = |k: &str| {
        let v = if i["dut"].is_object() {
            &i["dut"][k]
        } else {
            &i["state"]["dut"][k]
        };
        match v {
            Value::Bool(true) => "on".to_string(),
            Value::Bool(false) => "off".to_string(),
            Value::Null => "?".to_string(),
            v => v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string()),
        }
    };
    format!(
        "online  fw {}  power {}  recovery {}",
        i["fw"].as_str().unwrap_or("?"),
        dut("power"),
        dut("recovery")
    )
}

fn show(b: &Board) {
    println!("{}", b.describe());
    println!("  state:    {}", state(b));
    if let Some(m) = b.info["model"].as_str().filter(|m| !m.is_empty()) {
        println!("  model:    {m}");
    }
    let devices = if b.info["devices"].is_array() {
        &b.info["devices"]
    } else {
        &b.info["state"]["devices"]
    };
    for d in devices.as_array().into_iter().flatten() {
        println!(
            "  device:   {}  {}:{}",
            d["busid"].as_str().unwrap_or("?"),
            d["vid"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| d["vid"].to_string()),
            d["pid"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| d["pid"].to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_reads_both_hubs_shapes() {
        let local = Board {
            address: "a".into(),
            name: None,
            label: None,
            hub: Hub::Local,
            info: json!({"online": true, "fw": "0.1", "dut": {"power": true, "recovery": false}}),
            vm_instance: None,
        };
        assert_eq!(state(&local), "online  fw 0.1  power on  recovery off");
        let connect = Board {
            info: json!({"online": true, "fw": "0.1", "state": {"dut": {"power": "on"}}}),
            ..local.clone()
        };
        assert_eq!(state(&connect), "online  fw 0.1  power on  recovery ?");
        let p2 = Board {
            info: json!({"online": true, "fw": "0.2", "io": {"power": true, "ssr1": false}}),
            ..local.clone()
        };
        assert_eq!(state(&p2), "online  fw 0.2  power on, ssr1 off");
        let off = Board {
            info: json!({"online": false, "error": "no route"}),
            ..local
        };
        assert_eq!(state(&off), "offline (no route)");
    }

    #[test]
    fn quoting_survives_odd_addresses() {
        assert_eq!(sh_quote("10.0.0.1"), "'10.0.0.1'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }
}
