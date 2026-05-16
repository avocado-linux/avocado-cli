//! `avocado vm shell` — drop into an SSH session against the running VM.

use anyhow::Result;

use crate::utils::vm::lifecycle;

pub struct ShellCommand {
    /// Optional command + args to run instead of opening a TTY.
    pub command: Vec<String>,
}

impl ShellCommand {
    pub async fn execute(self) -> Result<()> {
        let target = lifecycle::ssh_target_for_running()?;
        let extra = if self.command.is_empty() {
            None
        } else {
            Some(self.command.as_slice())
        };
        let status = target.interactive(extra).await?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        Ok(())
    }
}
