//! Docker socket forwarding via `ssh -L`.
//!
//! Instead of using Docker's `ssh://` transport (which hardcodes its own
//! `ssh` argv and has no env hook for our managed key / known_hosts), we
//! spawn a long-lived `ssh -N -L <local-sock>:/run/docker.sock` into the
//! VM, daemonized like QEMU. From the host's perspective, `DOCKER_HOST=
//! unix://<local-sock>` then connects to dockerd inside the VM through a
//! plain Unix socket — no ssh-config or known_hosts shenanigans in the
//! docker client.
//!
//! Lifecycle:
//!   - [`start`]   spawn the forwarder, record its pid, return it.
//!   - [`stop`]    read the pidfile, SIGTERM → SIGKILL → cleanup.
//!   - [`is_alive`] best-effort liveness check.
//!
//! Implemented as a separate forked process so the SSH stays up across
//! avocado-cli invocations.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use super::state::{self, VmPaths};

const REMOTE_DOCKER_SOCK: &str = "/run/docker.sock";

// POSIX signal numbers, declared here so call sites compile on Windows
// where `libc::SIGTERM` / `libc::SIGKILL` are not defined. `send_signal`
// itself is `#[cfg(unix)]` and is a no-op elsewhere.
const SIGTERM: libc::c_int = 15;
const SIGKILL: libc::c_int = 9;

/// Spawn the SSH forward in the background. The returned pid is the ssh
/// process; killing it cleans up the forwarded socket.
pub async fn start(paths: &VmPaths, ssh_port: u16) -> Result<u32> {
    // Build the `-L` forward spec per platform.
    //   unix:    local AF_UNIX socket  → DOCKER_HOST=unix://<sock>
    //   windows: loopback TCP port     → DOCKER_HOST=tcp://127.0.0.1:<port>
    // Windows OpenSSH `-L` can't bind a local AF_UNIX socket and the docker
    // client has no `unix://` transport there, so we forward a TCP port to
    // the VM's /run/docker.sock instead. The byte stream is identical
    // (dockerd's HTTP API), so the docker client is none the wiser.
    #[cfg(unix)]
    let (forward_spec, tcp_port) = {
        // ssh -L refuses to overwrite an existing socket file even if dead.
        let local = paths.docker_socket();
        let _ = std::fs::remove_file(&local);
        (format!("{}:{}", local.display(), REMOTE_DOCKER_SOCK), 0u16)
    };
    #[cfg(not(unix))]
    let (forward_spec, tcp_port) = {
        let port = super::qemu::pick_free_port()?;
        state::write_docker_port(paths, port)?;
        (
            format!("127.0.0.1:{port}:{REMOTE_DOCKER_SOCK}"),
            port,
        )
    };

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args([
        "-N", // no remote command, just forward
        "-T", // no tty
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        &format!("UserKnownHostsFile={}", paths.known_hosts().display()),
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "BatchMode=yes",
        "-o",
        "LogLevel=ERROR",
        "-i",
        paths.ssh_key().to_str().context("ssh key path utf-8")?,
        "-p",
        &ssh_port.to_string(),
        "-L",
        &forward_spec,
        "root@127.0.0.1",
    ]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }

    let child = cmd.spawn().context("failed to spawn ssh -L forwarder")?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("spawned forwarder has no pid"))?;
    drop(child); // detach; the forwarder runs independently

    std::fs::write(paths.forwarder_pid(), pid.to_string())
        .with_context(|| format!("writing {}", paths.forwarder_pid().display()))?;

    // Poll briefly until the forward is live — confirms it's established
    // before we hand back. ExitOnForwardFailure=yes means ssh dies fast if
    // /run/docker.sock isn't there yet on the remote side.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if forward_ready(paths, tcp_port) {
            return Ok(pid);
        }
        if !state::pid_alive(pid) {
            let _ = std::fs::remove_file(paths.forwarder_pid());
            bail!(
                "ssh forwarder exited before the docker forward came up; \
                 is /run/docker.sock present in the VM? (`docker.service` running?)"
            );
        }
        if std::time::Instant::now() >= deadline {
            // Best-effort kill so we don't leak the process.
            send_signal(pid, SIGTERM);
            let _ = std::fs::remove_file(paths.forwarder_pid());
            bail!("timed out waiting for the docker forward to come up (ssh -L not established)");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Has the forward come up? Unix waits for the local socket file to be
/// created by ssh; Windows probes the loopback TCP port for acceptance.
#[cfg(unix)]
fn forward_ready(paths: &VmPaths, _tcp_port: u16) -> bool {
    paths.docker_socket().exists()
}

#[cfg(not(unix))]
fn forward_ready(_paths: &VmPaths, tcp_port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", tcp_port)).is_ok()
}

/// Stop the forwarder. Idempotent; missing pidfile / dead pid is a no-op.
pub async fn stop(paths: &VmPaths) -> Result<()> {
    if let Ok(pid) = read_pid(&paths.forwarder_pid()) {
        if state::pid_alive(pid) {
            send_signal(pid, SIGTERM);
            for _ in 0..20 {
                if !state::pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if state::pid_alive(pid) {
                send_signal(pid, SIGKILL);
            }
        }
    }
    let _ = std::fs::remove_file(paths.forwarder_pid());
    let _ = std::fs::remove_file(paths.docker_socket());
    Ok(())
}

/// Best-effort liveness check.
#[allow(dead_code)]
pub fn is_alive(paths: &VmPaths) -> bool {
    match read_pid(&paths.forwarder_pid()) {
        Ok(pid) => state::pid_alive(pid),
        Err(_) => false,
    }
}

fn read_pid(path: &Path) -> Result<u32> {
    let raw = std::fs::read_to_string(path).context("read forwarder pidfile")?;
    raw.trim().parse::<u32>().context("parse forwarder pid")
}

fn send_signal(pid: u32, sig: libc::c_int) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
    }
}
