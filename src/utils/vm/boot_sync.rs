//! Boot handshake: declare the guest "ready" when **either** qga responds
//! to `guest-sync` **or** sshd accepts a TCP connection. Whichever wins.
//!
//! Both signals are equivalent for our purposes — they each mean systemd
//! has reached the multi-user portion of boot. Racing them avoids hard
//! failure when one of the two paths is misconfigured (e.g. qemu-guest-agent
//! not installed in the VM image).

use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration, Instant};

use super::qga::QgaClient;

// 120s covers cold-boot pessimism on first run (kernel ramdisk extract +
// systemd unit ordering + first-time var.btrfs mount). Subsequent boots
// finish in well under 30s.
const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Wait for the guest to become ready. Callers immediately use SSH after
/// this returns (workspace mount, docker forward), so we ALWAYS require
/// SSH to be past its identification banner before declaring ready. qga
/// is raced in alongside as an early "systemd reached multi-user" signal
/// — if qga responds before sshd is reachable, we know boot is making
/// progress and we'll keep polling SSH; if SSH responds first, qga gets
/// dropped. Errors only if both are still failing when `timeout` elapses.
pub async fn wait_for_guest_ready(
    qga_socket: &Path,
    ssh_port: u16,
    timeout: Option<Duration>,
) -> Result<&'static str> {
    let timeout = timeout.unwrap_or(DEFAULT_BOOT_TIMEOUT);

    let qga = wait_qga(qga_socket.to_path_buf(), timeout);
    let ssh = wait_ssh(ssh_port, timeout);

    // Race for the first sign of life. qga winning is fine — it just means
    // we'll lean on the SSH wait that follows. SSH winning is enough by
    // itself (it's the same thing the caller's about to use).
    let winner = tokio::select! {
        r = qga => r.map(|_| "qga"),
        r = ssh => r.map(|_| "ssh"),
    }?;

    // If qga won, sshd may still be in its accept-but-reset window. Do a
    // final SSH-banner check before returning so the next operation
    // doesn't immediately hit "kex_exchange_identification: Connection
    // reset by peer".
    if winner == "qga" {
        wait_ssh(ssh_port, timeout)
            .await
            .context("qga came up but sshd never advanced past kex-init")?;
    }
    Ok(winner)
}

async fn wait_qga(socket: std::path::PathBuf, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !socket.exists() {
        if Instant::now() >= deadline {
            bail!(
                "qga socket {} never appeared (waited {}s)",
                socket.display(),
                timeout.as_secs()
            );
        }
        sleep(POLL_INTERVAL).await;
    }
    loop {
        if Instant::now() >= deadline {
            bail!(
                "guest-sync never responded on {} (waited {}s); is qemu-guest-agent installed and enabled in the VM?",
                socket.display(),
                timeout.as_secs(),
            );
        }
        if let Ok(mut client) = QgaClient::connect(&socket).await {
            if client.ping().await.is_ok() {
                return Ok(());
            }
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn wait_ssh(port: u16, deadline_window: Duration) -> Result<()> {
    let deadline = Instant::now() + deadline_window;
    loop {
        if Instant::now() >= deadline {
            bail!(
                "sshd on 127.0.0.1:{port} never sent its identification banner (waited {}s)",
                deadline_window.as_secs()
            );
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
            // TCP-accept is not enough — sshd accepts connections while
            // still initializing and then closes them mid-kex with
            // "kex_exchange_identification: Connection reset by peer".
            // Wait for sshd's identification banner ("SSH-2.0-OpenSSH_…")
            // before declaring ready. Only the first 4 bytes are needed:
            // matching "SSH-" guarantees the daemon is past the
            // not-yet-listening race.
            let mut buf = [0u8; 4];
            if let Ok(Ok(_n)) = timeout(Duration::from_secs(1), s.read_exact(&mut buf)).await {
                if &buf == b"SSH-" {
                    return Ok(());
                }
            }
            // banner didn't arrive (or wrong bytes); fall through and
            // retry. dropping `s` closes the socket cleanly.
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Same as [`wait_for_guest_ready`] but tries to surface a final error from
/// the last attempt, useful for `avocado vm status` style introspection.
#[allow(dead_code)]
pub async fn last_boot_error(socket: &Path) -> Result<()> {
    let mut client = QgaClient::connect(socket).await.context("qga connect")?;
    client.ping().await.context("qga ping")
}
