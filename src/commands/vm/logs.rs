//! `avocado vm logs` — print (or tail) the QEMU serial console log.

use anyhow::{Context, Result};

use crate::utils::vm::state::VmPaths;

pub struct LogsCommand {
    pub follow: bool,
}

impl LogsCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let log = paths.serial_log();
        if !log.exists() {
            anyhow::bail!(
                "no serial log at {} (is the VM running? `avocado vm status`)",
                log.display()
            );
        }
        if self.follow {
            // tail -f equivalent. Reuse the system `tail` since it handles
            // log rotation, EOF, etc. correctly.
            let status = tokio::process::Command::new("tail")
                .arg("-f")
                .arg(&log)
                .status()
                .await
                .context("failed to spawn `tail`")?;
            std::process::exit(status.code().unwrap_or(0));
        } else {
            let content =
                std::fs::read_to_string(&log).context("read serial log")?;
            print!("{content}");
            Ok(())
        }
    }
}
