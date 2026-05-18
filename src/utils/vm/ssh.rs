//! SSH helpers for the `avocado-vm` target.
//!
//! Thin wrapper around `ssh` that always uses the CLI-managed key, known_hosts,
//! and ssh-config under `~/.avocado/vm/`. Two ergonomic entry points:
//! [`SshTarget::exec`] runs one command and returns its output;
//! [`SshTarget::interactive`] forwards stdin/stdout/stderr so `avocado vm shell`
//! drops the user straight into a TTY.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use super::state::VmPaths;

/// Routing info for ssh-ing into the running VM.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: PathBuf,
    /// Reserved for a future refactor that uses `ssh -F <config>`. Currently
    /// the equivalent options are passed inline.
    #[allow(dead_code)]
    pub ssh_config: PathBuf,
    pub known_hosts: PathBuf,
}

impl SshTarget {
    /// Default target for a locally-running avocado-vm bound on the loopback.
    pub fn local(paths: &VmPaths, port: u16) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port,
            user: "root".to_string(),
            key: paths.ssh_key(),
            ssh_config: paths.ssh_config(),
            known_hosts: paths.known_hosts(),
        }
    }

    /// Common SSH args (no command appended).
    pub fn base_args(&self) -> Vec<String> {
        vec![
            "-p".into(),
            self.port.to_string(),
            "-i".into(),
            self.key.to_string_lossy().into_owned(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "-o".into(),
            "PasswordAuthentication=no".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            format!("{}@{}", self.user, self.host),
        ]
    }

    /// Run a one-shot command. Returns (stdout, stderr) on success.
    pub async fn exec(&self, command: &str) -> Result<(String, String)> {
        let mut cmd = AsyncCommand::new("ssh");
        cmd.args(self.base_args());
        cmd.arg(command);
        cmd.stdin(Stdio::null());
        let out = cmd.output().await.context("failed to spawn ssh")?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            bail!(
                "ssh `{command}` exited with status {}\nstdout: {stdout}\nstderr: {stderr}",
                out.status,
            );
        }
        Ok((stdout, stderr))
    }

    /// Drop the user into an interactive shell. Returns when ssh exits.
    pub async fn interactive(
        &self,
        command: Option<&[String]>,
    ) -> Result<std::process::ExitStatus> {
        let mut cmd = AsyncCommand::new("ssh");
        cmd.args(self.base_args());
        if let Some(extra) = command {
            cmd.args(extra);
        }
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().context("failed to spawn ssh")?;
        let status = child.wait().await.context("ssh wait failed")?;
        Ok(status)
    }
}
