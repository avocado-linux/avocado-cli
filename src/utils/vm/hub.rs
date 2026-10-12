//! Host side of the VM hub: the Rubicon desired state in
//! `~/.avocado/vm/config.yaml`, pushed to the in-VM agent's API socket over
//! SSH (`avocado-vm-agent ctl`), and the avocado-conn claim.
//!
//! The host config is the source of truth: it's pushed after every VM
//! start and every `vm rubicon add/remove`, so `vm reset` or an agent
//! restart never loses it.

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::config::VmConfig;
use super::ssh::SshTarget;
use super::state::VmPaths;

/// Where avocado-conn keeps its config and claim state in the guest. Must be
/// under /var: the guest's /etc is a /run-backed overlay.
pub const CONN_DIR: &str = "/var/lib/avocado-conn";
/// The agent's API socket, avocado-conn's `[local_handler]`.
pub const AGENT_API_SOCK: &str = "/run/avocado-vm-agent/api.sock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RubiconEntry {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

pub fn rubicons(cfg: &VmConfig) -> Result<Vec<RubiconEntry>> {
    match cfg.extra.get("rubicons") {
        None => Ok(Vec::new()),
        Some(v) => serde_yaml::from_value(v.clone()).context("invalid `rubicons` in config.yaml"),
    }
}

pub fn set_rubicons(cfg: &mut VmConfig, list: &[RubiconEntry]) -> Result<()> {
    if list.is_empty() {
        cfg.extra.remove("rubicons");
    } else {
        cfg.extra
            .insert("rubicons".into(), serde_yaml::to_value(list)?);
    }
    Ok(())
}

/// Facts about the host the VM runs on, shown in Connect.
pub fn host_facts(paths: &VmPaths) -> Value {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let host_os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    let vm_version = super::manifest::Manifest::load(&paths.manifest())
        .ok()
        .and_then(|m| m.version);
    json!({
        "hostname": hostname,
        "host_os": host_os,
        "avocado_version": env!("AVOCADO_CLI_VERSION"),
        "vm_version": vm_version,
    })
}

/// Send one request line to the agent and return its reply.
pub async fn agent_request(target: &SshTarget, req: &Value) -> Result<Value> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(req.to_string());
    let (out, _) = target
        .exec(&format!("echo {b64} | base64 -d | avocado-vm-agent ctl"))
        .await
        .context("the VM agent did not answer (is it running a hub-capable build?)")?;
    let reply: Value = serde_json::from_str(out.trim())
        .with_context(|| format!("unexpected agent reply: {out}"))?;
    if reply.get("ok") != Some(&json!(true)) {
        bail!("agent refused: {}", reply.get("error").unwrap_or(&reply));
    }
    Ok(reply)
}

/// Push host facts + the Rubicon list to the agent.
pub async fn push_configure(paths: &VmPaths, target: &SshTarget) -> Result<()> {
    let cfg = VmConfig::load(paths)?;
    let req = json!({
        "op": "configure",
        "host": host_facts(paths),
        "rubicons": rubicons(&cfg)?,
    });
    agent_request(target, &req).await.map(|_| ())
}

/// The API URL as the guest must use it: QEMU user-mode NAT reaches the
/// host's loopback at 10.0.2.2.
pub fn guest_api_url(api_url: &str) -> String {
    for local in ["://localhost", "://127.0.0.1"] {
        if let Some(i) = api_url.find(local) {
            return format!(
                "{}://10.0.2.2{}",
                &api_url[..i],
                &api_url[i + local.len()..]
            );
        }
    }
    api_url.to_string()
}

/// avocado-conn config for a VM: claim with the DMI UUID (stable per
/// install, see `state::instance_id`) and hand unknown commands to the
/// agent.
pub fn conn_config(api_url: &str, claim_token: &str) -> String {
    format!(
        "# Written by `avocado vm connect`.\n\
         api_url = \"{api}\"\n\
         claim_token = \"{claim_token}\"\n\
         device_id_source = \"dmi\"\n\
         data_dir = \"{CONN_DIR}\"\n\
         \n\
         [local_handler]\n\
         socket = \"{AGENT_API_SOCK}\"\n",
        api = guest_api_url(api_url),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_url_rewrites_host_loopback_only() {
        assert_eq!(
            guest_api_url("http://localhost:3001"),
            "http://10.0.2.2:3001"
        );
        assert_eq!(
            guest_api_url("http://127.0.0.1:3001/x"),
            "http://10.0.2.2:3001/x"
        );
        assert_eq!(
            guest_api_url("https://connect.peridio.com"),
            "https://connect.peridio.com"
        );
    }

    #[test]
    fn rubicons_round_trip_through_config_extra() {
        let mut cfg = VmConfig::default();
        assert!(rubicons(&cfg).unwrap().is_empty());
        let list = vec![
            RubiconEntry {
                address: "192.168.1.52".into(),
                label: Some("jetson".into()),
            },
            RubiconEntry {
                address: "192.168.1.69".into(),
                label: None,
            },
        ];
        set_rubicons(&mut cfg, &list).unwrap();
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: VmConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(rubicons(&back).unwrap(), list);
        set_rubicons(&mut cfg, &[]).unwrap();
        assert!(!cfg.extra.contains_key("rubicons"));
    }

    #[test]
    fn conn_config_claims_by_dmi_and_bridges_to_the_agent() {
        let c = conn_config("http://localhost:3001", "tok");
        let v: toml::Value = toml::from_str(&c).unwrap();
        assert_eq!(v["api_url"].as_str(), Some("http://10.0.2.2:3001"));
        assert_eq!(v["claim_token"].as_str(), Some("tok"));
        assert_eq!(v["device_id_source"].as_str(), Some("dmi"));
        assert_eq!(v["data_dir"].as_str(), Some(CONN_DIR));
        assert_eq!(v["local_handler"]["socket"].as_str(), Some(AGENT_API_SOCK));
    }
}
