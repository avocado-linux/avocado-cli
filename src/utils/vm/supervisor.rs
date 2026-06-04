//! Host-side hibernation supervisor for the helper VM.
//!
//! Architecturally a small proxy server with QMP-driven lifecycle.
//! QEMU is launched with its SSH hostfwd bound to a loopback-only
//! "internal" port; the supervisor listens on the user-facing port
//! (the one in `~/.avocado/vm/ssh-port`) and pipes accepted
//! connections through to the internal port. Doing it this way means
//! *we* see every incoming connection, which gives us:
//!
//! 1. **Idle detection** — when no proxied connection has been active
//!    for `idle_after_secs`, we send QMP `stop` to halt all vCPU
//!    threads. Host CPU drops to ~0%; guest RAM stays resident.
//! 2. **Wake-on-connect** — on the next incoming TCP, we send QMP
//!    `cont` *before* opening the inner connection. The guest resumes
//!    in-place and the SSH handshake completes ~100ms later than it
//!    would on a live VM.
//!
//! The supervisor also owns the user-facing **docker socket**
//! (`~/.avocado/vm/docker.sock`). On any incoming docker client
//! connection it ensures (a) the VM is awake and (b) a single
//! supervisor-managed `ssh -L` tunnel is running between an internal
//! sock (`docker.sock.internal`) and `/run/docker.sock` in the guest,
//! then pipes the client through. The tunnel comes up on wake and is
//! torn down on pause so QEMU can sleep cleanly.
//!
//! Lifecycle: spawned by `lifecycle::start` after QEMU is reachable,
//! killed by `lifecycle::stop` before QEMU. The subcommand entry point
//! lives in `commands::vm::supervise` — this module is the loop it
//! runs.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::qmp::QmpClient;
use super::state;

/// Log a supervisor event with a UTC timestamp prefix. Timestamps make
/// pause/wake cycles in `~/.avocado/vm/supervisor.log` legible without
/// having to correlate against shell history.
macro_rules! slog {
    ($($arg:tt)*) => {{
        eprintln!("[{}] supervisor: {}", chrono::Utc::now().to_rfc3339(), format_args!($($arg)*))
    }};
}

/// Arguments passed from the `avocado vm supervise` subcommand into the
/// supervisor loop. Plain owned data so the caller can construct it from
/// clap-parsed flags without leaking lifetimes.
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// External TCP port the supervisor listens on. Today this is the
    /// SSH port that everything else (`vm shell`, Avocado.app)
    /// connects to.
    pub user_port: u16,
    /// Loopback port QEMU's `hostfwd` binds to. Only the supervisor
    /// connects here.
    pub internal_port: u16,
    /// QMP control socket.
    pub qmp_socket: PathBuf,
    /// How long with no active connections before we halt the vCPUs.
    pub idle_after_secs: u64,
    /// Path to write our pid so the lifecycle layer can kill us later.
    pub pid_file: PathBuf,
    /// Host path for the user-facing docker socket. Supervisor owns it.
    pub docker_socket: PathBuf,
    /// Host path the supervisor's SSH `-L` tunnel binds to; only the
    /// docker proxy connects here.
    pub docker_socket_internal: PathBuf,
    /// "Infrastructure" TCP lane — second user-facing port that
    /// wakes the VM on connect but does NOT count toward idle.
    /// Used by long-lived telemetry channels (Avocado.app's agent SSH
    /// tunnel) so they don't pin the VM awake.
    pub infra_port: u16,
    /// "Infrastructure" docker socket. Same SSH `-L` tunnel as
    /// `docker_socket`, but accepted connections here don't count
    /// toward idle — meant for `GET /events` style streaming
    /// subscriptions.
    pub docker_socket_stream: PathBuf,
    /// SSH private key for tunneling to the guest.
    pub ssh_key: PathBuf,
    /// known_hosts file the SSH tunnel uses.
    pub known_hosts: PathBuf,
}

struct State {
    paused: AtomicBool,
    active_conns: AtomicUsize,
    last_activity_ms: AtomicI64,
    qmp_socket: PathBuf,
    idle_threshold_ms: i64,
    args: RunArgs,
    /// SSH `-L` tunnel child pid, if running. Mutex serializes
    /// spawn/kill so a pause/wake race doesn't leak a child.
    tunnel: Mutex<Option<u32>>,
    /// Serializes QMP stop/cont so racing wake-and-pause attempts
    /// can't leave the supervisor's `paused` flag out of sync with
    /// QEMU's actual state.
    qmp_lock: Mutex<()>,
}

impl State {
    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// QMP `cont` only — bring vCPUs back to running. Idempotent and
    /// fast (single QMP round-trip). Does NOT touch the SSH tunnel:
    /// TCP-proxy callers don't need it, and bundling it would make
    /// every SSH probe wait 8s on tunnel spawn during boot.
    ///
    /// On macOS, fires `vm.notify.woke` to Avocado.app after the
    /// successful `cont` so the desktop can re-attach USB devices that
    /// the host-side USB/IP helper may have dropped while the VM was
    /// paused (the helper times out reading from a frozen guest, drops
    /// the IOUSBHost claim, and leaves vhci_hcd holding a dead FD).
    /// The notification is best-effort; if the desktop isn't running,
    /// it's a silent no-op.
    async fn wake(self: &Arc<Self>) -> Result<()> {
        let _guard = self.qmp_lock.lock().await;
        if self.paused.load(Ordering::Relaxed) {
            qmp_send(&self.qmp_socket, "cont", None)
                .await
                .context("QMP cont")?;
            self.paused.store(false, Ordering::Relaxed);
            slog!("resumed VM on incoming connection");
            notify_woke().await;
        }
        Ok(())
    }

    /// Halt the VM and tear down the tunnel so QEMU isn't holding any
    /// kernel-side state that the guest can't service while paused.
    async fn pause(self: &Arc<Self>) -> Result<()> {
        let _guard = self.qmp_lock.lock().await;
        if self.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Tear down tunnel first; its SSH keepalives would otherwise
        // timeout while QEMU is stopped and the child would die in
        // a way we can't tell apart from a real failure.
        self.kill_tunnel().await;
        qmp_send(&self.qmp_socket, "stop", None)
            .await
            .context("QMP stop")?;
        self.paused.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Spawn the SSH `-L` tunnel if it's not already running. Polls
    /// briefly for the local socket to appear so callers can proceed
    /// to `connect()` immediately on return.
    async fn ensure_tunnel(self: &Arc<Self>) -> Result<()> {
        let mut lock = self.tunnel.lock().await;
        if let Some(pid) = *lock {
            if state::pid_alive(pid) && self.args.docker_socket_internal.exists() {
                return Ok(());
            }
            // stale handle; clean up before respawning
            send_signal(pid, SIGTERM);
            let _ = std::fs::remove_file(&self.args.docker_socket_internal);
        }
        let pid = spawn_ssh_tunnel(&self.args)?;
        // Wait for the local sock to materialize — ssh -L creates it
        // only after authentication completes.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if self.args.docker_socket_internal.exists() {
                *lock = Some(pid);
                slog!("docker tunnel up (pid {pid})");
                return Ok(());
            }
            if !state::pid_alive(pid) {
                return Err(anyhow::anyhow!(
                    "ssh tunnel exited before docker socket appeared"
                ));
            }
            if std::time::Instant::now() >= deadline {
                send_signal(pid, SIGTERM);
                return Err(anyhow::anyhow!(
                    "timed out waiting for docker tunnel to come up"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn kill_tunnel(self: &Arc<Self>) {
        let mut lock = self.tunnel.lock().await;
        if let Some(pid) = lock.take() {
            send_signal(pid, SIGTERM);
            // Don't block long; ssh dies quickly on SIGTERM.
            for _ in 0..20 {
                if !state::pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if state::pid_alive(pid) {
                send_signal(pid, SIGKILL);
            }
        }
        let _ = std::fs::remove_file(&self.args.docker_socket_internal);
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const SIGTERM: libc::c_int = 15;
const SIGKILL: libc::c_int = 9;

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

/// Run the supervisor loop until killed.
pub async fn run(args: RunArgs) -> Result<()> {
    std::fs::write(&args.pid_file, std::process::id().to_string())
        .with_context(|| format!("writing {}", args.pid_file.display()))?;

    let state = Arc::new(State {
        paused: AtomicBool::new(false),
        active_conns: AtomicUsize::new(0),
        last_activity_ms: AtomicI64::new(now_ms()),
        qmp_socket: args.qmp_socket.clone(),
        idle_threshold_ms: (args.idle_after_secs.saturating_mul(1000)) as i64,
        tunnel: Mutex::new(None),
        qmp_lock: Mutex::new(()),
        args: args.clone(),
    });

    // Tunnel comes up lazily on first docker conn (handle_docker calls
    // ensure_tunnel). Spawning eagerly here would race against guest
    // sshd boot: the SSH handshake fails for ~30s after QEMU starts,
    // and during that time the supervisor's TCP listener wouldn't bind
    // (this function blocks on tunnel polling), making the whole boot
    // cascade fail.

    let tcp_listener = TcpListener::bind(("127.0.0.1", args.user_port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", args.user_port))?;
    eprintln!(
        "supervisor: TCP listening on 127.0.0.1:{} → 127.0.0.1:{} (idle {} s)",
        args.user_port, args.internal_port, args.idle_after_secs
    );

    // Stale Unix socket would refuse bind; ours is owned by us across restarts.
    let _ = std::fs::remove_file(&args.docker_socket);
    let unix_listener = UnixListener::bind(&args.docker_socket)
        .with_context(|| format!("binding {}", args.docker_socket.display()))?;
    eprintln!(
        "supervisor: Unix listening on {} → SSH→/run/docker.sock",
        args.docker_socket.display()
    );

    // Infrastructure TCP lane — wakes the VM on connect, proxies to the
    // same internal hostfwd, but does NOT count toward idle. Long-lived
    // telemetry channels (desktop's agent SSH tunnel, future event-stream
    // consumers) connect here so they don't pin the VM awake.
    let infra_tcp_listener = TcpListener::bind(("127.0.0.1", args.infra_port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", args.infra_port))?;
    eprintln!(
        "supervisor: infra TCP listening on 127.0.0.1:{} → 127.0.0.1:{} (idle-exempt)",
        args.infra_port, args.internal_port
    );

    // Infrastructure docker socket — same SSH tunnel, doesn't count toward
    // idle. Meant for `GET /events` streaming subscriptions.
    let _ = std::fs::remove_file(&args.docker_socket_stream);
    let infra_unix_listener = UnixListener::bind(&args.docker_socket_stream)
        .with_context(|| format!("binding {}", args.docker_socket_stream.display()))?;
    eprintln!(
        "supervisor: infra Unix listening on {} → SSH→/run/docker.sock (idle-exempt)",
        args.docker_socket_stream.display()
    );

    if args.idle_after_secs > 0 {
        let state_t = state.clone();
        tokio::spawn(async move {
            idle_watcher(state_t).await;
        });
    }

    // Signal handler: on SIGTERM/SIGINT, restore the VM to a usable
    // state (resumed + tunnel down) so the next start doesn't trip
    // over a paused VM with no supervisor to wake it.
    let state_sig = state.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_term().await {
            slog!("signal handler error: {e:#}");
            return;
        }
        let _ = state_sig.wake().await; // ensure VM is resumed before we exit
        state_sig.kill_tunnel().await;
        std::process::exit(0);
    });

    // Main accept loop: select between TCP/Unix user-facing listeners
    // (counted) and the two infra listeners (idle-exempt). Spawned
    // tasks own their connection through close.
    loop {
        tokio::select! {
            res = tcp_listener.accept() => {
                let (sock, peer) = match res {
                    Ok(v) => v,
                    Err(e) => { slog!("TCP accept error: {e:#}"); continue; }
                };
                let s = state.clone();
                let internal_port = args.internal_port;
                tokio::spawn(async move {
                    if let Err(e) = handle_tcp(sock, internal_port, s, /* count */ true).await {
                        slog!("TCP conn {peer} error: {e:#}");
                    }
                });
            }
            res = infra_tcp_listener.accept() => {
                let (sock, peer) = match res {
                    Ok(v) => v,
                    Err(e) => { slog!("infra TCP accept error: {e:#}"); continue; }
                };
                let s = state.clone();
                let internal_port = args.internal_port;
                tokio::spawn(async move {
                    if let Err(e) = handle_tcp(sock, internal_port, s, /* count */ false).await {
                        slog!("infra TCP conn {peer} error: {e:#}");
                    }
                });
            }
            res = unix_listener.accept() => {
                let (sock, _peer) = match res {
                    Ok(v) => v,
                    Err(e) => { slog!("Unix accept error: {e:#}"); continue; }
                };
                let s = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_docker(sock, s, /* count */ true).await {
                        slog!("docker conn error: {e:#}");
                    }
                });
            }
            res = infra_unix_listener.accept() => {
                let (sock, _peer) = match res {
                    Ok(v) => v,
                    Err(e) => { slog!("infra Unix accept error: {e:#}"); continue; }
                };
                let s = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_docker(sock, s, /* count */ false).await {
                        slog!("infra docker conn error: {e:#}");
                    }
                });
            }
        }
    }
}

/// Proxy a TCP connection from a user-facing or infra listener to QEMU's
/// internal hostfwd.
///
/// `count`-true: user-facing traffic (a real SSH session, etc.) —
/// bumps `active_conns` + activity time, and calls `wake()` to bring
/// the VM out of hibernation. Drives the VM lifecycle.
///
/// `count`-false: infrastructure (long-lived telemetry like the
/// desktop's agent SSH tunnel) — does NOT touch activity counters and
/// does NOT wake the VM. Just opportunistically uses the VM if it's
/// already running. Otherwise the inner connect to `internal_port`
/// succeeds (QEMU slirp accepts) but bytes queue without delivery; the
/// caller times out and retries with backoff. Keeps hibernation
/// intact: only real user activity can wake the VM.
async fn handle_tcp(
    mut incoming: TcpStream,
    internal_port: u16,
    state: Arc<State>,
    count: bool,
) -> Result<()> {
    if count {
        state.active_conns.fetch_add(1, Ordering::Relaxed);
        state.touch();
        if let Err(e) = state.wake().await {
            slog!("wake failed: {e}");
        }
    }

    let mut inner = TcpStream::connect(("127.0.0.1", internal_port))
        .await
        .with_context(|| format!("connecting to internal port {internal_port}"))?;
    let res = tokio::io::copy_bidirectional(&mut incoming, &mut inner).await;
    let _ = incoming.shutdown().await;
    let _ = inner.shutdown().await;

    if count {
        state.active_conns.fetch_sub(1, Ordering::Relaxed);
        state.touch();
    }
    classify_close(res)
}

/// Proxy a docker client to the supervisor-managed SSH tunnel.
///
/// `count`-true: user-facing docker call (avocado build, docker ps from
/// the user shell, etc.) — wakes VM + brings tunnel up if needed,
/// counts toward idle. The VM stays awake until the call finishes.
///
/// `count`-false: infrastructure (containers watcher's `/events` stream,
/// snapshot refreshes) — does NOT wake VM and does NOT ensure the
/// tunnel. Just connects to the existing tunnel socket; if it's down
/// (paused VM, boot still in progress), returns a fast error. Caller
/// backs off and retries. This is what lets hibernation actually stick
/// when the desktop is open: the watcher's reconnect attempts can't
/// pin the VM awake by themselves, only user activity can.
async fn handle_docker(mut client: UnixStream, state: Arc<State>, count: bool) -> Result<()> {
    if count {
        state.active_conns.fetch_add(1, Ordering::Relaxed);
        state.touch();
        state.wake().await.context("waking VM for docker conn")?;
        state
            .ensure_tunnel()
            .await
            .context("bringing docker tunnel up")?;
    } else if !state.args.docker_socket_internal.exists() {
        // Infra: tunnel not currently up. Fail fast — caller (likely
        // ContainersWatcher) backs off and retries; a future
        // user-driven docker call will bring the tunnel up and the
        // next retry will succeed.
        return Err(anyhow::anyhow!(
            "docker tunnel not up (VM paused or still booting)"
        ));
    }

    let mut backend = UnixStream::connect(&state.args.docker_socket_internal)
        .await
        .with_context(|| {
            format!(
                "connecting to docker tunnel sock {}",
                state.args.docker_socket_internal.display()
            )
        })?;
    let res = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
    let _ = client.shutdown().await;
    let _ = backend.shutdown().await;

    if count {
        state.active_conns.fetch_sub(1, Ordering::Relaxed);
        state.touch();
    }
    classify_close(res)
}

/// Filter expected close patterns. SSH probe (boot_sync), `vm shell`
/// exit, docker client disconnect, any client that closes without
/// TCP-FIN — all show up as ECONNRESET / BrokenPipe / UnexpectedEof
/// here. Real I/O faults still propagate.
fn classify_close(res: std::io::Result<(u64, u64)>) -> Result<()> {
    match res {
        Ok(_) => Ok(()),
        Err(e) => match e.kind() {
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected => Ok(()),
            _ => Err(e).context("bidirectional copy failed"),
        },
    }
}

async fn idle_watcher(state: Arc<State>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if state.paused.load(Ordering::Relaxed) {
            continue;
        }
        if state.active_conns.load(Ordering::Relaxed) > 0 {
            continue;
        }
        let since = now_ms() - state.last_activity_ms.load(Ordering::Relaxed);
        if since >= state.idle_threshold_ms {
            match state.pause().await {
                Ok(_) => slog!("paused VM after {since} ms idle"),
                Err(e) => {
                    slog!("pause failed: {e}");
                    state.touch(); // back off
                }
            }
        }
    }
}

/// Spawn an `ssh -N -L <local-sock>:/run/docker.sock` to the guest.
/// Same flag set as the original `forward.rs`; managed by the
/// supervisor instead of `lifecycle::start`.
fn spawn_ssh_tunnel(args: &RunArgs) -> Result<u32> {
    let _ = std::fs::remove_file(&args.docker_socket_internal);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args([
        "-N",
        "-T",
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
        &format!("UserKnownHostsFile={}", args.known_hosts.display()),
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "BatchMode=yes",
        "-o",
        "LogLevel=ERROR",
        "-i",
        args.ssh_key.to_str().context("ssh key path utf-8")?,
        "-p",
        &args.internal_port.to_string(),
        "-L",
        &format!("{}:/run/docker.sock", args.docker_socket_internal.display()),
        "root@127.0.0.1",
    ]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawning ssh -L tunnel")?;
    Ok(child.id())
}

/// Thin one-shot QMP command runner. Open + close per call because
/// stop/cont happen at most a few times per minute and the QmpClient
/// holds its own connection state.
async fn qmp_send(socket: &Path, cmd: &str, args: Option<serde_json::Value>) -> Result<()> {
    let mut client = QmpClient::connect(socket).await?;
    let _ = client.execute(cmd, args).await?;
    Ok(())
}

/// Fire `vm.notify.woke` to Avocado.app via the shared CLI/desktop
/// notify channel. Dispatched via `spawn_blocking` because the underlying
/// `client::notify` uses sync std-net I/O with a 100ms timeout; running
/// it directly on the runtime thread would briefly block the supervisor's
/// accept loop on a slow/missing desktop.
#[cfg(target_os = "macos")]
async fn notify_woke() {
    tokio::task::spawn_blocking(|| {
        super::client::notify("vm.notify.woke", serde_json::json!({}));
    })
    .await
    .ok();
}

#[cfg(not(target_os = "macos"))]
async fn notify_woke() {}

#[cfg(unix)]
async fn wait_for_term() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut intr = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    tokio::select! {
        _ = term.recv() => {}
        _ = intr.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_term() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("install ctrl-c handler")
}
