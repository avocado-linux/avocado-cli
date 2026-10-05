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
    /// Claim a board for the Connect org (someone presses its SW2).
    Claim {
        board: Option<String>,
    },
    /// New firmware: a signed image file, or a released version.
    Update {
        board: Option<String>,
        file: Option<std::path::PathBuf>,
        version: Option<String>,
        all: bool,
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
                let token =
                    remote::local_token(&b, "rubicon_console", self.connect_profile.as_deref())
                        .await?;
                self.console(&b, !read_only, true, true, token)
            }
            RubiconAction::Update {
                board,
                file,
                version,
                all,
            } => {
                let fw = firmware(file.as_deref(), version.as_deref()).await?;
                eprintln!(
                    "firmware {} ({}, {} KiB)",
                    fw.version,
                    &fw.sha256[..12],
                    fw.bytes.len() >> 10
                );
                if !*all {
                    let b = self.board(board).await?;
                    return self.update(&b, &fw).await;
                }
                self.update_all(&fw).await
            }
            RubiconAction::Claim { board } => {
                let b = self.board(board).await?;
                self.claim(&b).await
            }
            RubiconAction::Logs { board, follow } => {
                let b = self.board(board).await?;
                let token = if *follow {
                    remote::local_token(&b, "rubicon_console", self.connect_profile.as_deref())
                        .await?
                } else {
                    None
                };
                self.console(&b, false, true, *follow, token)
            }
        }
    }

    /// Hand the board the org's key: through Connect it fills the key in;
    /// through this machine's VM the key comes from Connect here. Either
    /// way someone presses SW2 on the board within the next minute.
    async fn claim(&self, b: &Board) -> Result<()> {
        let params = match &b.hub {
            Hub::Local => {
                let (c, org) =
                    crate::commands::connect::hub::connect(self.connect_profile.as_deref(), None)?;
                let info = c
                    .json(
                        reqwest::Method::GET,
                        &format!("/api/orgs/{org}/rubicon-claim"),
                        None,
                    )
                    .await
                    .context("the org's claim key (org admins only)")?;
                json!({"address": b.address, "org": info["data"]["org_id"],
                       "public_key": info["data"]["public_key"]})
            }
            Hub::Connect { .. } => json!({"address": b.address}),
        };
        eprintln!(
            "Press SW2 on {} now (the LED blinks blue for 60 s).",
            b.describe()
        );
        let r = remote::run_command(b, "rubicon_claim", params, self.connect_profile.as_deref())
            .await?;
        remote::report(&r, &format!("claim of {}", b.describe()))?;
        print_success(
            &format!(
                "{} is claimed: actions now need tokens from Connect",
                b.describe()
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    async fn update(&self, b: &Board, fw: &Firmware) -> Result<()> {
        let params = match &b.hub {
            Hub::Local => {
                // In the VM's workspace share: the guest reads it in place.
                let paths = crate::utils::vm::state::VmPaths::resolve()?;
                let ws = crate::utils::vm::share::read_recorded_workspace(&paths)?
                    .map(Ok)
                    .unwrap_or_else(|| crate::utils::vm::share::resolve_workspace(None))?;
                let dir = ws.join(".avocado/hub-firmware");
                std::fs::create_dir_all(&dir)?;
                let host = dir.join(format!("{}.bin", fw.sha256));
                std::fs::write(&host, &fw.bytes)?;
                let vm = crate::utils::vm::share::translate_to_vm(&host, &ws)?;
                json!({"address": b.address, "firmware": {
                    "url": vm.to_string_lossy(), "sha256": fw.sha256, "version": fw.version}})
            }
            Hub::Connect { .. } => {
                let id = upload_firmware(self.connect_profile.as_deref(), fw).await?;
                json!({"address": b.address, "firmware_id": id})
            }
        };
        let r = remote::run_command(b, "rubicon_update", params, self.connect_profile.as_deref())
            .await?;
        let res = &r["result"];
        remote::report(&r, &format!("update of {}", b.describe()))?;
        print_success(
            &format!(
                "{} updated: {} -> {}",
                b.describe(),
                res["from"].as_str().unwrap_or("?"),
                res["to"].as_str().unwrap_or("?")
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Every reachable, online board; three at a time. One board failing
    /// does not stop the others; any failure fails the command.
    async fn update_all(&self, fw: &Firmware) -> Result<()> {
        use futures_util::StreamExt;
        let boards: Vec<Board> = self
            .all_boards()
            .await?
            .into_iter()
            .filter(|b| b.info["online"] == true)
            .collect();
        if boards.is_empty() {
            bail!("no online Rubicons to update");
        }
        let results: Vec<(String, Result<()>)> = futures_util::stream::iter(boards)
            .map(|b| async move { (b.describe(), self.update(&b, fw).await) })
            .buffer_unordered(3)
            .collect()
            .await;
        println!("\n{:<60} RESULT", "BOARD");
        let mut failed = 0;
        for (who, r) in &results {
            match r {
                Ok(()) => println!("{who:<60} updated"),
                Err(e) => {
                    failed += 1;
                    println!("{who:<60} FAILED: {e:#}");
                }
            }
        }
        if failed > 0 {
            bail!("{failed} of {} Rubicons not updated", results.len());
        }
        Ok(())
    }

    /// A board of this machine's VM (adding, removing and labelling edit
    /// that VM's list).
    async fn local(&self, wanted: &str) -> Result<Board> {
        remote::pick(wanted, remote::local_boards().await?)?.with_context(|| {
            format!("this machine's avocado-vm has no Rubicon {wanted:?} (`avocado rubicon list`)")
        })
    }

    async fn list(&self) -> Result<()> {
        let boards = self.all_boards().await?;
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

    /// Every board reachable from here, each once (this machine's VM is also
    /// one of the org's VMs in Connect).
    async fn all_boards(&self) -> Result<Vec<Board>> {
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
        Ok(boards)
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
    fn console(
        &self,
        b: &Board,
        write: bool,
        history: bool,
        follow: bool,
        token: Option<String>,
    ) -> Result<()> {
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
        if let Some(t) = token {
            cmd.push_str(&format!(" --token {}", sh_quote(&t)));
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

/// A signed Rubicon image.
pub struct Firmware {
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// As the board reports it in `hello` (`0.1.0+83`).
    pub version: String,
}

/// The version in an MCUboot image header (`major.minor.revision+build`).
pub fn mcuboot_version(image: &[u8]) -> Result<String> {
    let le32 = |o: usize| {
        image
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    if le32(0) != Some(0x96f3_b83d) {
        bail!("not an MCUboot image (no image header magic); use the signed .bin");
    }
    let rev = u16::from_le_bytes([image[22], image[23]]);
    Ok(format!(
        "{}.{}.{rev}+{}",
        image[20],
        image[21],
        le32(24).unwrap()
    ))
}

/// Where released images are fetched from:
/// `<base>/v<version>/rubicon-micro-<version>.signed.bin` and `SHA256SUMS`
/// beside it, `<version>` in dotted form (`0.1.0.83`).
const RELEASES: &str = "https://github.com/avocado-linux/rubicon-micro/releases/download";

async fn firmware(file: Option<&std::path::Path>, version: Option<&str>) -> Result<Firmware> {
    use sha2::{Digest, Sha256};
    let bytes = match (file, version) {
        (Some(f), None) => std::fs::read(f).with_context(|| format!("read {}", f.display()))?,
        (None, Some(v)) => {
            let v = v.trim_start_matches('v').replace('+', ".");
            let base =
                std::env::var("AVOCADO_RUBICON_RELEASES").unwrap_or_else(|_| RELEASES.into());
            let asset = format!("rubicon-micro-{v}.signed.bin");
            let get = |url: String| async move {
                let r = reqwest::get(&url).await?.error_for_status()?;
                anyhow::Ok(r.bytes().await?.to_vec())
            };
            let sums = String::from_utf8_lossy(&get(format!("{base}/v{v}/SHA256SUMS")).await?)
                .into_owned();
            let want = sums
                .lines()
                .find_map(|l| {
                    let mut c = l.split_whitespace();
                    let (h, n) = (c.next()?, c.next()?);
                    (n.trim_start_matches('*') == asset).then(|| h.to_lowercase())
                })
                .with_context(|| format!("{asset} is not in release v{v}'s SHA256SUMS"))?;
            let bytes = get(format!("{base}/v{v}/{asset}")).await?;
            let got = crate::utils::signing_keys::hex::encode(&Sha256::digest(&bytes));
            if got != want {
                bail!("{asset}: sha256 {got}, the release says {want}");
            }
            bytes
        }
        _ => bail!("give exactly one of --file or --version"),
    };
    Ok(Firmware {
        version: mcuboot_version(&bytes)?,
        sha256: crate::utils::signing_keys::hex::encode(&Sha256::digest(&bytes)),
        bytes,
    })
}

/// Register the image in Connect, sending its bytes only if the org lacks them.
async fn upload_firmware(profile: Option<&str>, fw: &Firmware) -> Result<String> {
    use reqwest::Method;
    let (c, org) = crate::commands::connect::hub::connect(profile, None)?;
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &fw.bytes)?;
    let (_, parts) = crate::commands::connect::upload::compute_sha256_with_parts(tmp.path())?;
    let created = c
        .json(
            Method::POST,
            &format!("/api/orgs/{org}/rubicon-firmwares"),
            Some(
                &json!({"firmware": {"version": fw.version, "sha256": fw.sha256,
                                      "size_bytes": fw.bytes.len(), "part_checksums": parts}}),
            ),
        )
        .await?;
    let id = created["data"]["id"]
        .as_str()
        .context("no firmware id")?
        .to_string();
    if let Some(upload) = created["data"]["upload"].as_object() {
        let parts = upload["parts"].as_array().cloned().unwrap_or_default();
        let done = crate::commands::connect::hub::upload_parts(
            &c,
            tmp.path(),
            parts,
            &format!("/api/orgs/{org}/rubicon-firmwares/{id}/upload-urls"),
        )
        .await?;
        c.json(
            Method::POST,
            &format!("/api/orgs/{org}/rubicon-firmwares/{id}/complete"),
            Some(&json!({"parts": done})),
        )
        .await?;
        eprintln!("firmware {} uploaded to Connect", fw.version);
    } else {
        eprintln!("firmware {} already in Connect", fw.version);
    }
    Ok(id)
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
    let mode = if i["claimed"] == true {
        "claimed"
    } else {
        "dev mode"
    };
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
            "online  {mode}  fw {}  {}",
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
        assert_eq!(state(&p2), "online  dev mode  fw 0.2  power on, ssr1 off");
        let off = Board {
            info: json!({"online": false, "error": "no route"}),
            ..local
        };
        assert_eq!(state(&off), "offline (no route)");
    }

    #[test]
    fn reads_the_mcuboot_header_version() {
        let mut img = vec![0u8; 64];
        img[..4].copy_from_slice(&0x96f3_b83du32.to_le_bytes());
        img[21] = 1;
        img[24..28].copy_from_slice(&83u32.to_le_bytes());
        assert_eq!(mcuboot_version(&img).unwrap(), "0.1.0+83");
        assert!(mcuboot_version(&[0u8; 64]).is_err());
    }

    #[test]
    fn quoting_survives_odd_addresses() {
        assert_eq!(sh_quote("10.0.0.1"), "'10.0.0.1'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }
}
