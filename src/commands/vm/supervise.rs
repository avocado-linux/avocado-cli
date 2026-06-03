//! `avocado vm supervise` — long-lived host-side hibernation supervisor.
//!
//! Spawned by `avocado vm start` after QEMU is reachable. Not intended
//! to be run by users directly (hidden in CLI help); the lifecycle
//! layer owns the argv. See [`crate::utils::vm::supervisor`] for the
//! actual loop.

use anyhow::Result;
use std::path::PathBuf;

use crate::utils::vm::supervisor::{run, RunArgs};

pub struct SuperviseCommand {
    pub user_port: u16,
    pub internal_port: u16,
    pub qmp_socket: PathBuf,
    pub idle_after_secs: u64,
    pub pid_file: PathBuf,
    pub docker_socket: PathBuf,
    pub docker_socket_internal: PathBuf,
    pub ssh_key: PathBuf,
    pub known_hosts: PathBuf,
}

impl SuperviseCommand {
    pub async fn execute(self) -> Result<()> {
        run(RunArgs {
            user_port: self.user_port,
            internal_port: self.internal_port,
            qmp_socket: self.qmp_socket,
            idle_after_secs: self.idle_after_secs,
            pid_file: self.pid_file,
            docker_socket: self.docker_socket,
            docker_socket_internal: self.docker_socket_internal,
            ssh_key: self.ssh_key,
            known_hosts: self.known_hosts,
        })
        .await
    }
}
