//! `avocado vm status` — show running state + key metadata.

use anyhow::Result;

use crate::utils::vm::lifecycle;

pub struct StatusCommand;

impl StatusCommand {
    pub async fn execute(self) -> Result<()> {
        let s = lifecycle::status().await?;
        if s.running {
            let state_tag = match s.paused {
                Some(true) => " (hibernated — wakes on next ssh/docker call)",
                _ => "",
            };
            println!(
                "avocado-vm running (pid {}, ssh 127.0.0.1:{}){state_tag}",
                s.pid.unwrap_or(0),
                s.ssh_port.unwrap_or(0),
            );
        } else {
            println!("avocado-vm is not running.");
        }
        if let Some(p) = s.manifest_platform {
            println!("  platform: {p}");
        }
        if let Some(a) = s.manifest_arch {
            println!("  architecture: {a}");
        }
        println!("  state dir: {}", s.paths.root.display());
        Ok(())
    }
}
