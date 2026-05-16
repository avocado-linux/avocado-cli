//! `avocado vm start` — boot the helper VM if not already running.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;

use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::lifecycle::{self, StartOptions};
use crate::utils::vm::state::VmPaths;

pub struct StartCommand {
    pub vm_source: Option<PathBuf>,
    pub memory_mib: u32,
    pub cpus: u32,
    pub ssh_port: Option<u16>,
    pub cmdline_extra: Option<String>,
    pub workspace: Option<PathBuf>,
    pub var_size: Option<String>,
    pub watch: bool,
    #[allow(dead_code)]
    pub foreground: bool,
}

impl StartCommand {
    pub async fn execute(self) -> Result<()> {
        let vm_source = match self.vm_source {
            Some(p) => p,
            None => std::env::var("AVOCADO_VM_DIR")
                .map(PathBuf::from)
                .context("--vm-source not given and AVOCADO_VM_DIR is unset; point at a directory containing `direct` profile output (manifest.json + kernel/initramfs/rootfs/var)")?,
        };

        print_info(
            &format!("Starting avocado-vm from {}…", vm_source.display()),
            OutputLevel::Normal,
        );

        // If --watch is set, tail the serial log in parallel with boot-sync
        // so the user sees the boot progress as it happens. `tail -F` is robust
        // to the file appearing only after QEMU has started.
        let mut tail_child = if self.watch {
            let paths = VmPaths::resolve()?;
            let log = paths.serial_log();
            print_info(
                &format!("Watching serial log at {}…", log.display()),
                OutputLevel::Normal,
            );
            Some(
                tokio::process::Command::new("tail")
                    .arg("-F")
                    .arg("-n")
                    .arg("+0")
                    .arg(&log)
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("failed to spawn `tail -F` for log watch")?,
            )
        } else {
            None
        };

        let opts = StartOptions {
            vm_source,
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            ssh_port: self.ssh_port,
            cmdline_extra: self.cmdline_extra,
            workspace: self.workspace,
            var_size: self.var_size,
        };
        let result = lifecycle::start(opts).await;

        // Tear down the tail subprocess regardless of outcome.
        if let Some(child) = tail_child.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        // On failure (and only when not already watching) dump the tail of the
        // serial log so the user has diagnostic context without re-running.
        if result.is_err() && !self.watch {
            if let Ok(paths) = VmPaths::resolve() {
                if let Ok(content) = std::fs::read_to_string(paths.serial_log()) {
                    let tail: String = content.lines().rev().take(40).collect::<Vec<_>>()
                        .into_iter().rev().collect::<Vec<_>>().join("\n");
                    eprintln!("\n--- last 40 lines of serial log ---\n{tail}\n--- end ---\n");
                }
            }
        }

        let status = result?;
        print_success(
            &format!(
                "avocado-vm running (pid {}) on ssh 127.0.0.1:{}",
                status.pid.unwrap_or(0),
                status.ssh_port.unwrap_or(0),
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
}
