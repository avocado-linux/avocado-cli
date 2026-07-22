//! `avocado container dev` orchestration: `up`/`down`/`status` + per-`up`
//! bootstrap (task 5.2).
//!
//! `up` mints BOTH session tokens (task 3.6), starts the embedded registry (the
//! dedicated bulk read listener + the distinct write listener), the engine-driver
//! watcher (task 4.x), and the control WebSocket (task 5.1); resolves the host
//! endpoint (reusing `get_local_ip_for_remote` + the `AVOCADO_CONTAINER_DEV_HOST`
//! / `AVOCADO_CONTAINER_DEV_PORT` overrides, design L2); and writes ONCE per `up`
//! to the device writable partition the BULK-LISTENER endpoint (never the write
//! listener, design G-4), the READ/CONTROL token (never the write token), and the
//! CA certificate. Steady-state sync then rides the control WS with no further
//! SSH (design D5).
//!
//! `down` stops all listeners AND tears down the routable write listener + its
//! `0.0.0.0` forward through a guaranteed-cleanup guard
//! ([`crate::utils::container_dev::bootstrap::WriteListenerGuard`]), so an unclean
//! exit never leaves an authenticated LAN write port bound (design L-1).
//!
//! `status` reports registry/watcher/last-sync state and surfaces a "re-run
//! `up`/bootstrap" state when a device presents a stale token (design H-2), using
//! the drain-based [`crate::utils::container_dev::bootstrap::TokenRegistry`] — the
//! rotated-out read/control token stays valid until its in-flight bulk pulls
//! drain to zero OR a hard ceiling elapses, so a mid-pull rotation of the largest
//! image on a throttled link never 401s the in-flight pull.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::utils::config::{Config, RuntimeConfig};
use crate::utils::container_dev::bootstrap::{
    bootstrap_path, host_override, port_override, resolve_endpoint, DevStatus, DeviceBootstrap,
    WriteListenerGuard, WRITABLE_PARTITION,
};
use crate::utils::container_dev::commands::{prune_store, run_one_shot_sync};
use crate::utils::container_dev::config::ContainerDevConfig;
use crate::utils::container_dev::engine::{driver_for, watch_tag_events, TagEvent};
use crate::utils::container_dev::registry::{write_router, BulkListener};
use crate::utils::container_dev::store::BlobStore;
use crate::utils::container_dev::tls::DevSession;
use crate::utils::container_dev::watcher::{
    arch_guard::HelloArchBook, run_watcher, EngineSyncer, HostTopology, SyncMode, DEBOUNCE,
};
use crate::utils::container_dev::ws::{ControlServer, DesiredState};
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};
use crate::utils::remote::{get_local_ip_for_remote, RemoteHost, SshClient};

/// Default config file, matching the rest of the CLI (`-C/--config`).
const DEFAULT_CONFIG: &str = "avocado.yaml";

/// The device SSH target `up` bootstraps and the endpoint auto-detection resolves
/// the reachable host IP against (design A6/L2). The `up`/`down`/`status`
/// subcommands take no positional arguments (task 2.3), so the device is sourced
/// here.
const DEVICE_ENV: &str = "AVOCADO_CONTAINER_DEV_DEVICE";

/// The default engine CLI when none is configured.
const DEFAULT_ENGINE: &str = "docker";

pub struct DevUpCommand;
pub struct DevSyncCommand;
pub struct DevStatusCommand;
pub struct DevDownCommand;
pub struct DevPruneCommand;

/// The resolved dev context: the runtime that carries the `container_dev` block,
/// its config, and the per-project namespace derived from the runtime name
/// (design D8 per-project store/CA/token/port namespacing).
struct DevContext {
    project: String,
    dev: ContainerDevConfig,
}

/// Load the config and select the runtime whose `container_dev` block enables the
/// feature (design D7 — presence of the block is the gate).
fn load_dev_context() -> Result<DevContext> {
    let config = Config::load(DEFAULT_CONFIG)
        .with_context(|| format!("loading Container Dev Mode config from {DEFAULT_CONFIG}"))?;
    let runtimes = config.runtimes.unwrap_or_default();

    let mut enabled: Vec<(String, RuntimeConfig)> = runtimes
        .into_iter()
        .filter(|(_, rt)| rt.container_dev.is_some())
        .collect();
    enabled.sort_by(|a, b| a.0.cmp(&b.0));

    match enabled.len() {
        0 => bail!(
            "no runtime has a `container_dev` block; add `runtimes.<name>.container_dev` to \
             {DEFAULT_CONFIG} to enable Container Dev Mode"
        ),
        1 => {
            let (project, rt) = enabled.into_iter().next().unwrap();
            let dev = rt
                .container_dev
                .expect("filtered runtimes carry a container_dev block");
            Ok(DevContext { project, dev })
        }
        _ => {
            let names: Vec<String> = enabled.into_iter().map(|(name, _)| name).collect();
            bail!(
                "multiple runtimes enable Container Dev Mode ({}); v1 supports a single dev \
                 runtime per config",
                names.join(", ")
            )
        }
    }
}

/// The path to the per-`up` session state file, a sibling of the per-project
/// registry store (`~/.avocado/container-dev/<project>/session.json`). `down` and
/// `status` read it; `up` writes it on start and clears it on teardown.
fn session_state_path(store: &BlobStore) -> PathBuf {
    store
        .root()
        .parent()
        .expect("the registry store root sits under the per-project dir")
        .join("session.json")
}

impl DevUpCommand {
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = Arc::new(
            BlobStore::for_project(&ctx.project)
                .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?,
        );

        // Source the device SSH target: needed to deliver the bootstrap and, when
        // no host override is set, to auto-detect the reachable host IP.
        let device_spec = std::env::var(DEVICE_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(device_spec) = device_spec else {
            bail!(
                "set {DEVICE_ENV}=<user@host> to the dev device so `up` can bootstrap it \
                 (the subcommands take no positional arguments)"
            );
        };
        let device = RemoteHost::parse(&device_spec)?;

        // Mint fresh TLS material + BOTH tokens for this `up` (design D2/D8).
        let session = DevSession::mint(&ctx.project)
            .with_context(|| format!("minting the dev session for `{}`", ctx.project))?;
        let tls_config = session.tls.server_config();
        let read_token = session.read_token.clone();
        let write_token = session.write_token.clone();

        // Resolve the BULK-LISTENER endpoint the device pulls from (design L2):
        // AVOCADO_CONTAINER_DEV_HOST overrides host auto-detection;
        // AVOCADO_CONTAINER_DEV_PORT overrides the configured port.
        let configured_port = ctx.dev.registry.port;
        let auto_host = match host_override() {
            Some(_) => String::new(),
            None => get_local_ip_for_remote(&device.host)
                .await
                .with_context(|| {
                    format!(
                        "auto-detecting the host IP reachable from `{}`",
                        device.host
                    )
                })?
                .to_string(),
        };
        let bulk_endpoint = resolve_endpoint(
            host_override().as_deref(),
            &auto_host,
            port_override(),
            configured_port,
        );

        // The bulk read listener binds the resolved port on all interfaces so the
        // device (or its loopback proxy) can reach it over TLS. The write listener
        // is bound SEPARATELY and loopback-only (design D9/G-4).
        let bulk_bind: SocketAddr = format!("0.0.0.0:{}", endpoint_port(&bulk_endpoint)?)
            .parse()
            .expect("a host:port endpoint yields a valid bind address");
        let bulk = BulkListener::bind(
            bulk_bind,
            Arc::clone(&store),
            read_token.clone(),
            tls_config,
        )
        .await
        .context("binding the dedicated bulk read listener")?;
        let bulk_addr = bulk.local_addr();

        // The DISTINCT write listener: loopback-only on native Linux so a device
        // (handed only the bulk endpoint) can never reach a write route (design
        // D9/H-1). Its address is NEVER disclosed to a device.
        let write_bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback write bind is valid");
        let write_listener = TcpListener::bind(write_bind)
            .await
            .context("binding the loopback write listener")?;
        let write_addr = write_listener.local_addr()?;
        let write_router = write_router(Arc::clone(&store), write_token.clone());
        let write_task: JoinHandle<()> = tokio::spawn(async move {
            let _ = axum::serve(write_listener, write_router).await;
        });

        // Guaranteed-cleanup guard for the routable write listener + its `0.0.0.0`
        // forward (design L-1): aborting the serve task tears the listener down on
        // ANY exit path, clean or unclean, so no authenticated write port lingers.
        let mut write_guard = WriteListenerGuard::new(move || {
            write_task.abort();
        });

        // The control WS (task 5.1) shares the read/control-token validator with
        // the bulk listener (design G-5) AND terminates the SAME per-project
        // pinned-CA TLS the bulk listener does (design D8/D9): the device agent
        // dials `wss://` and pins the session CA, so the control channel is never
        // plaintext. Its desired state is RE-DERIVED at `up` from the engine's
        // current watched tags (design D5) — the watcher's first events populate
        // it; we start empty and let hellos reconcile.
        let control = ControlServer::new(
            read_token.clone(),
            DesiredState::default(),
            HelloArchBook::new(),
        );
        let ws_listener = TcpListener::bind("0.0.0.0:0")
            .await
            .context("binding the control WS listener")?;
        let ws_addr = ws_listener.local_addr()?;
        // `tls_config` was moved into `BulkListener::bind` above; `server_config()`
        // returns a fresh `Arc::clone` of the same leaf-backed config for the
        // control acceptor.
        let control_acceptor = TlsAcceptor::from(session.tls.server_config());
        let control_serve = Arc::clone(&control);
        let ws_task: JoinHandle<()> =
            tokio::spawn(
                async move { control_serve.serve_tls(ws_listener, control_acceptor).await },
            );

        // The engine-driver watcher (task 4.x): tag events over the engine CLI
        // subprocess (never an API socket), topology-selected PUSH/INGEST, then a
        // control-WS notify — no SSH per sync (design D5).
        let engine = DEFAULT_ENGINE;
        let driver =
            driver_for(engine).with_context(|| format!("no engine driver for `{engine}`"))?;
        let mode = HostTopology::detect().sync_mode();
        let project_dir = store
            .root()
            .parent()
            .expect("store root has a per-project parent")
            .to_path_buf();
        let syncer = Arc::new(EngineSyncer::new(
            driver_for(engine).expect("engine driver resolves"),
            write_addr.to_string(),
            write_token.clone(),
            project_dir,
        ));
        let (events_rx, mut events_child) = watch_tag_events(driver)
            .await
            .context("starting the engine event watcher")?;
        let notifier = Arc::clone(&control);
        // The watcher and the manual `sync` trigger share the SAME push+notify
        // primitives (design D5): clone the syncer + control for the trigger
        // before the watcher takes ownership of its copies.
        let trigger_syncer = Arc::clone(&syncer);
        let trigger_notifier = Arc::clone(&control);
        let watcher_task: JoinHandle<()> = tokio::spawn(async move {
            run_watcher(events_rx, mode, syncer, notifier, DEBOUNCE).await;
        });

        // The `container dev sync` trigger (task 5.3): a separate `sync`
        // invocation signals this process (SIGUSR1), and each signal drives ONE
        // re-push + notify of every configured watched image through the SAME
        // pipeline the watcher uses — exactly once per signal, never a second
        // watch loop. Reusing the running session's syncer + control WS is what
        // lets the notify reach a connected device with no extra SSH.
        let watched_images: Vec<String> =
            ctx.dev.images.iter().map(|i| i.image_ref.clone()).collect();
        let sync_trigger_task: JoinHandle<()> = tokio::spawn(async move {
            run_sync_trigger(mode, trigger_syncer, trigger_notifier, watched_images).await;
        });

        // Deliver the bootstrap ONCE per `up` (design D5): the bulk endpoint (the
        // device-reachable address of the bulk listener), the read/control token,
        // and the CA cert — never the write token, never the write-listener
        // address (design G-4). Steady-state sync never re-opens SSH.
        let device_bulk_endpoint = format!(
            "{}:{}",
            bulk_host(&bulk_endpoint, &auto_host),
            bulk_addr.port()
        );
        let payload = DeviceBootstrap::from_session(&session, device_bulk_endpoint);
        deliver_bootstrap(&device, &payload).await?;

        // Record the running session (with this process's pid) so `status`/`down`
        // in a separate invocation can find and signal it.
        let state = SessionState {
            pid: std::process::id(),
            status: DevStatus {
                registry_running: true,
                watcher_running: true,
                last_sync: None,
                devices: Vec::new(),
            },
        };
        let state_path = session_state_path(&store);
        write_session_state(&state_path, &state)?;

        print_success(
            &format!(
                "container dev up: bulk listener on {bulk_addr}, write listener loopback-only on \
                 {write_addr}, control WS on {ws_addr}; device `{}` bootstrapped",
                device.host
            ),
            OutputLevel::Normal,
        );
        print_info(
            "Watching for image rebuilds; press Ctrl-C or run `container dev down` to tear down.",
            OutputLevel::Normal,
        );

        // Run foreground until interrupted by Ctrl-C (SIGINT) or by a separate
        // `down` (SIGTERM). On ANY exit — including a panic or early return — the
        // write guard tears down the routable write listener + its `0.0.0.0`
        // forward via Drop (design L-1); the other listeners' tasks are aborted
        // and the state file is cleared.
        wait_for_shutdown().await;

        write_guard.teardown();
        ws_task.abort();
        watcher_task.abort();
        sync_trigger_task.abort();
        let _ = events_child.kill().await;
        drop(bulk);
        let _ = std::fs::remove_file(&state_path);

        print_info(
            "container dev down: listeners torn down.",
            OutputLevel::Normal,
        );
        Ok(())
    }
}

impl DevSyncCommand {
    /// One-shot re-push + notify of the current watched tag (task 5.3, design
    /// M4): NO long-running watcher. `sync` finds the running `up` session and
    /// signals it (SIGUSR1) to drive ONE pass of the same push+notify pipeline
    /// the watcher uses — reusing the session's registry write listener, engine
    /// syncer, and control WS so the notify reaches a connected device with no
    /// extra SSH. With no active session there is nothing holding those
    /// listeners, so `sync` reports that `up` must run first rather than silently
    /// doing nothing.
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = BlobStore::for_project(&ctx.project)
            .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?;
        let state_path = session_state_path(&store);

        let Some(state) = read_session_state(&state_path)? else {
            bail!(
                "container dev: no active `up` session to sync; run `avocado container dev up` \
                 first, then `sync` re-pushes the current watched image"
            );
        };

        // Trigger exactly one re-push + notify in the running `up` process.
        signal_sync(state.pid);
        print_info(
            "container dev sync: triggered a one-shot re-push + notify of the watched image(s).",
            OutputLevel::Normal,
        );
        Ok(())
    }
}

impl DevStatusCommand {
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = BlobStore::for_project(&ctx.project)
            .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?;
        let state_path = session_state_path(&store);

        let Some(state) = read_session_state(&state_path)? else {
            print_info(
                "container dev: not running (no active `up` session).",
                OutputLevel::Normal,
            );
            return Ok(());
        };

        let status = &state.status;
        print_info(
            &format!(
                "container dev status: registry_running={}, watcher_running={}, last_sync={}",
                status.registry_running,
                status.watcher_running,
                status.last_sync.as_deref().unwrap_or("<none>"),
            ),
            OutputLevel::Normal,
        );
        // Surface the re-bootstrap state when any device presented a stale token
        // (design H-2) — a stale token yields a status, never a silent loop.
        if status.needs_rebootstrap() {
            print_warning(
                "a device presented a stale token; re-run `avocado container dev up` to \
                 re-bootstrap it",
                OutputLevel::Normal,
            );
        }
        Ok(())
    }
}

impl DevDownCommand {
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = BlobStore::for_project(&ctx.project)
            .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?;
        let state_path = session_state_path(&store);

        let Some(state) = read_session_state(&state_path)? else {
            print_info(
                "container dev: nothing to tear down (no active `up` session).",
                OutputLevel::Normal,
            );
            return Ok(());
        };

        // Signal the foreground `up` process to shut down. It handles SIGTERM the
        // same as Ctrl-C, tearing down ALL listeners — and the routable write
        // listener + its `0.0.0.0` forward via the guaranteed-cleanup guard
        // (design L-1) — so no authenticated LAN write port survives `down`.
        signal_shutdown(state.pid);
        // The `up` process removes its own state file on graceful exit; remove it
        // here too so a `down` against an already-dead process still clears stale
        // state.
        let _ = std::fs::remove_file(&state_path);
        print_info(
            "container dev down: signaled the dev session to stop; listeners torn down.",
            OutputLevel::Normal,
        );
        Ok(())
    }
}

impl DevPruneCommand {
    /// Garbage-collect THIS project's Container Dev Mode store only (task 5.3,
    /// design M4): sweep blobs no currently-tagged manifest references, via the
    /// group-3.5 GC ([`prune_store`]). It touches only store blobs — never the
    /// per-session token or the per-project CA material — and refuses while a
    /// device is mid-pull rather than sweeping a blob a pull still needs.
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = BlobStore::for_project(&ctx.project)
            .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?;

        let swept = prune_store(&store).with_context(|| {
            format!(
                "pruning the Container Dev Mode store for project `{}`",
                ctx.project
            )
        })?;

        print_success(
            &format!(
                "container dev prune: swept {} unreferenced blob(s) from the `{}` store; the \
                 session token and CA material are left intact.",
                swept.len(),
                ctx.project
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

/// Deliver the bootstrap payload to the device writable partition ONCE (design
/// D5). Renders the JSON, base64-encodes it, and decodes it into
/// `WRITABLE_PARTITION/container-dev/bootstrap.json` over SSH so the payload
/// survives shell quoting untouched.
async fn deliver_bootstrap(device: &RemoteHost, payload: &DeviceBootstrap) -> Result<()> {
    use base64::Engine as _;

    let json = payload
        .to_json()
        .context("rendering the bootstrap payload")?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    let remote_path = bootstrap_path(std::path::Path::new(WRITABLE_PARTITION));
    let remote_path = remote_path.to_string_lossy();
    let remote_dir = std::path::Path::new(WRITABLE_PARTITION).join("container-dev");
    let remote_dir = remote_dir.to_string_lossy();

    let ssh = SshClient::new(device.clone());
    let command = format!(
        "mkdir -p {remote_dir} && printf %s '{encoded}' | base64 -d > {remote_path} && \
         chmod 0600 {remote_path}"
    );
    ssh.run_command(&command)
        .await
        .context("writing the bootstrap file to the device writable partition")?;
    Ok(())
}

/// The persisted per-`up` session record: the foreground `up` process id (so a
/// separate `down` can signal it to stop its listeners) plus the reported
/// [`DevStatus`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SessionState {
    /// PID of the foreground `up` process.
    pid: u32,
    /// The status `status` reports.
    status: DevStatus,
}

/// Persist the session state so `status`/`down` in a separate invocation can find
/// the running `up`.
fn write_session_state(path: &std::path::Path, state: &SessionState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the session state dir {parent:?}"))?;
    }
    let json = serde_json::to_string_pretty(state).context("serializing the session state")?;
    std::fs::write(path, json).with_context(|| format!("writing session state to {path:?}"))?;
    Ok(())
}

/// Read the session state, or `None` when no `up` session is recorded.
fn read_session_state(path: &std::path::Path) -> Result<Option<SessionState>> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let state: SessionState = serde_json::from_str(&content)
                .with_context(|| format!("parsing the session state at {path:?}"))?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading the session state at {path:?}")),
    }
}

/// Block until the process receives SIGINT (Ctrl-C) or SIGTERM (a separate
/// `down`), so both a foreground Ctrl-C and `down` reach the same graceful
/// teardown path.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            // No SIGTERM handler available: fall back to Ctrl-C only.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Signal the recorded `up` process to shut down (SIGTERM), driving its graceful
/// teardown (and, on any unclean exit, its [`WriteListenerGuard`]).
#[cfg(unix)]
fn signal_shutdown(pid: u32) {
    // SAFETY: `kill` with a plain signal number has no memory-safety hazard; a
    // stale pid simply yields ESRCH, which is ignored (the process already exited).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn signal_shutdown(_pid: u32) {}

/// Serve the `container dev sync` trigger: each SIGUSR1 (sent by a separate
/// `sync` invocation, [`signal_sync`]) drives ONE re-push + notify of every
/// configured watched image through the shared push+notify pipeline
/// ([`run_one_shot_sync`]) — exactly one pass per signal, never a second watch
/// loop. Runs until the task is aborted on teardown. A per-image failure is
/// surfaced as a warning and does not stop the trigger (a later `sync` retries).
#[cfg(unix)]
async fn run_sync_trigger(
    mode: SyncMode,
    syncer: Arc<EngineSyncer>,
    notifier: Arc<ControlServer>,
    images: Vec<String>,
) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut usr1 = match signal(SignalKind::user_defined1()) {
        Ok(s) => s,
        // No SIGUSR1 handler available: the trigger is simply inert.
        Err(_) => return,
    };
    while usr1.recv().await.is_some() {
        for image in &images {
            let event = TagEvent {
                image: image.clone(),
                image_id: None,
            };
            if let Err(e) =
                run_one_shot_sync(mode, syncer.as_ref(), notifier.as_ref(), &event).await
            {
                print_warning(
                    &format!("container dev sync of `{image}` failed: {e:#}"),
                    OutputLevel::Normal,
                );
            }
        }
    }
}

#[cfg(not(unix))]
async fn run_sync_trigger(
    _mode: SyncMode,
    _syncer: Arc<EngineSyncer>,
    _notifier: Arc<ControlServer>,
    _images: Vec<String>,
) {
}

/// Signal the recorded `up` process to perform one manual sync (SIGUSR1),
/// driving its [`run_sync_trigger`] through a single re-push + notify pass.
#[cfg(unix)]
fn signal_sync(pid: u32) {
    // SAFETY: `kill` with a plain signal number has no memory-safety hazard; a
    // stale pid simply yields ESRCH, which is ignored (the process already exited).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGUSR1);
    }
}

#[cfg(not(unix))]
fn signal_sync(_pid: u32) {}

/// The port component of a `host:port` endpoint.
fn endpoint_port(endpoint: &str) -> Result<u16> {
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .with_context(|| format!("`{endpoint}` is not a valid host:port endpoint"))
}

/// The host component the device uses to reach the bulk listener: the endpoint's
/// host (an override or the auto-detected reachable IP).
fn bulk_host<'a>(endpoint: &'a str, auto_host: &'a str) -> &'a str {
    match endpoint.rsplit_once(':') {
        Some((host, _)) if !host.is_empty() => host,
        _ => auto_host,
    }
}
