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
//! `down` stops all listeners AND tears down the write listener through a
//! guaranteed-cleanup guard
//! ([`crate::utils::container_dev::bootstrap::WriteListenerGuard`]), so an unclean
//! exit never leaves an authenticated write port bound (design L-1). The write
//! listener binds `127.0.0.1` only; the VM push path reaches it through QEMU's
//! `10.0.2.2` host alias rather than a routable bind, so there is no LAN-facing
//! write port to leak.
//!
//! `status` reports the registry/watcher/last-sync state recorded at `up` time,
//! and reports "not running" when no live `up` owns the session (proved by the
//! session lock, not by the recorded pid).
//!
//! NOT YET LIVE, despite being implemented and tested in `bootstrap.rs`: the
//! per-device `status.devices` list and the drain-based
//! [`crate::utils::container_dev::bootstrap::TokenRegistry`] rotation behind
//! `needs_rebootstrap()` (design H-2). `up` writes `session.json` once and never
//! updates it, so `devices` stays empty and `needs_rebootstrap()` is
//! structurally false; token rotation cannot cross an `up` either, because
//! `TokenRegistry::rotate` needs `&mut self` and a re-`up` is a NEW process that
//! starts from a fresh registry. Making both live needs `up` to keep publishing
//! session state while it runs, which is a change in its own right rather than a
//! missing call here. Until then `status` reports a live-or-not answer and the
//! per-device detail is absent, not stale.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::utils::config::{Config, RuntimeConfig};
use crate::utils::container_dev::bootstrap::{
    bootstrap_path, host_override, port_override, resolve_endpoint, write_port_override,
    ws_port_override, DevStatus, DeviceBootstrap, VmWriteSetup, WriteListenerGuard,
    DEFAULT_WRITE_PORT, DEFAULT_WS_PORT, WRITABLE_PARTITION,
};
use crate::utils::container_dev::commands::{prune_store, run_one_shot_sync};
use crate::utils::container_dev::config::ContainerDevConfig;
use crate::utils::container_dev::engine::{
    driver_for, resolve_image_id, watch_tag_events, TagEvent,
};
use crate::utils::container_dev::registry::{serve_write_router_tls, write_router, BulkListener};
use crate::utils::container_dev::store::BlobStore;
use crate::utils::container_dev::tls::DevSession;
use crate::utils::container_dev::watcher::{
    arch_guard::{ArchGuardSyncer, EngineArchProbe, HelloArchBook, ImageArchBook},
    run_watcher, EngineSyncer, HostTopology, SyncMode, Syncer, WatchSet, DEBOUNCE,
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

/// The avocado-vm engine guest SSH target the per-project CA is delivered into on
/// the VM write path (design D2/H4, task 7.1). Only consulted when the host
/// topology selects the avocado-vm push path; the native-Linux loopback push
/// never uses it.
const VM_ENV: &str = "AVOCADO_CONTAINER_DEV_VM";

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

/// The per-project dir holding the session state and lock files.
fn session_dir(store: &BlobStore) -> &Path {
    store
        .root()
        .parent()
        .expect("the registry store root sits under the per-project dir")
}

/// The path to the per-`up` session state file, a sibling of the per-project
/// registry store (`~/.avocado/container-dev/<project>/session.json`). `down` and
/// `status` read it; `up` writes it on start and clears it on teardown.
fn session_state_path(store: &BlobStore) -> PathBuf {
    session_dir(store).join("session.json")
}

/// The path to the per-`up` lock file, a sibling of the state file.
///
/// Deliberately NOT the state file itself. `flock` is held on an inode, and the
/// teardown paths unlink `session.json` - so locking that inode would mean the
/// next `up` locks a freshly created one and mutual exclusion would not survive
/// a single `down`. This file is created once and never removed, so the inode
/// every `up` contends on is stable for the life of the project dir.
fn session_lock_path(store: &BlobStore) -> PathBuf {
    session_dir(store).join("session.lock")
}

impl DevUpCommand {
    pub async fn execute(self) -> Result<()> {
        let ctx = load_dev_context()?;
        let store = Arc::new(
            BlobStore::for_project(&ctx.project)
                .with_context(|| format!("opening the dev store for project `{}`", ctx.project))?,
        );

        // Claim the project's session BEFORE anything observable happens. Every
        // step below is a side effect a second `up` must not interleave with:
        // minting fresh tokens, binding three listeners, spawning the watcher,
        // and SSHing a new bootstrap over the device's existing one. Taken at the
        // end instead, the lock would only report a collision that had already
        // occurred - the loser would have already repointed the device at itself
        // and overwritten the winner's state file before finding out it lost.
        let state_path = session_state_path(&store);
        let lock_path = session_lock_path(&store);
        let _session_lock = SessionLock::acquire(&lock_path)?;

        // Publish OUR pid the instant the lock is ours, before any of the slow
        // work below. Moving the lock to the top of `up` decoupled it from the
        // state file, and that reopened the hazard the lock exists to close: a
        // predecessor SIGKILLed before its teardown leaves its pid in
        // `session.json`, so between acquiring the lock here and writing the
        // record after the binds and the SSH, `load_live_session` would report
        // the session live while handing out a DEAD pid. `sync` would then send
        // SIGUSR1 - default disposition terminate - to whatever recycled that
        // number. Overwriting the record now restores the invariant that a live
        // lock implies the recorded pid is the holder's; the fuller status is
        // written again once the listeners are up.
        write_session_state(
            &state_path,
            &SessionState {
                pid: std::process::id(),
                status: DevStatus {
                    registry_running: false,
                    watcher_running: false,
                    last_sync: None,
                    devices: Vec::new(),
                },
            },
        )?;

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

        // Detect the host topology once (design D1): it selects PUSH vs INGEST AND,
        // on the avocado-vm push path, drives the write listener onto a KNOWN port
        // with a routable 10.0.2.2 registry + guest CA delivery (task 7.1).
        let topo = HostTopology::detect();

        // On the VM push path the per-project CA must be delivered into the
        // avocado-vm engine guest's trust store; require its SSH target up front so
        // `up` fails fast rather than after binding listeners (design H4).
        let vm_target = if topo.vm_routing {
            let spec = std::env::var(VM_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the host topology selected the avocado-vm push path; set \
                         {VM_ENV}=<user@host> to the avocado-vm engine guest so `up` can deliver \
                         the per-project CA into its docker trust store"
                    )
                })?;
            Some(RemoteHost::parse(&spec)?)
        } else {
            None
        };

        // The DISTINCT write listener: loopback-BOUND (design D9/H-1) so a device
        // (handed only the bulk endpoint) can never reach a write route; its
        // address is NEVER disclosed to a device. On the avocado-vm push path the
        // port must be KNOWN (not ephemeral) so the guest's certs.d dir and the
        // pushed tag are both keyed on 10.0.2.2:<port> (H-3) — a QEMU-SLIRP guest
        // reaches this loopback listener through the 10.0.2.2 host alias. Native
        // Linux keeps an ephemeral loopback port.
        let write_port = write_port_override().unwrap_or(DEFAULT_WRITE_PORT);
        let write_bind: SocketAddr = if topo.vm_routing {
            format!("127.0.0.1:{write_port}")
                .parse()
                .expect("a known write port yields a valid loopback bind")
        } else {
            "127.0.0.1:0".parse().expect("loopback write bind is valid")
        };
        let write_listener = TcpListener::bind(write_bind)
            .await
            .context("binding the loopback write listener")?;
        let write_addr = write_listener.local_addr()?;

        // On the VM push path compose the guest write-path plan (task 7.1): the
        // routable 10.0.2.2:<port> registry the guest daemon connects to, plus the
        // CA to deliver into its trust store. Native Linux pushes to the loopback
        // listener directly, so no guest plan is needed.
        let vm_setup = topo
            .vm_routing
            .then(|| VmWriteSetup::docker(&session, write_addr.port()));
        // The registry the syncer tags + authenticates against: the routable VM
        // registry on the VM path, else the loopback write listener itself.
        let syncer_registry = match &vm_setup {
            Some(setup) => setup.registry.clone(),
            None => write_addr.to_string(),
        };
        // On the VM push path the guest reaches this listener via 10.0.2.2 — not a
        // docker-trusted 127.0.0.0/8 loopback — so it must terminate the same
        // per-project leaf TLS the bulk and control listeners do; the guest's
        // delivered certs.d CA pins it (design A2/H4). The native loopback path
        // keeps plain HTTP under docker's built-in 127.0.0.0/8 insecure exemption.
        let write_task: JoinHandle<()> = if topo.vm_routing {
            serve_write_router_tls(
                write_listener,
                session.tls.server_config(),
                Arc::clone(&store),
                write_token.clone(),
            )
        } else {
            let write_router = write_router(Arc::clone(&store), write_token.clone());
            tokio::spawn(async move {
                let _ = axum::serve(write_listener, write_router).await;
            })
        };

        // Guaranteed-cleanup guard for the loopback write listener (design L-1):
        // aborting the serve task tears the listener down on ANY exit path, clean
        // or unclean, so no authenticated write port lingers.
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
        // The arch book is shared: the control server writes each device's
        // `hello.arch` into it, and the cross-arch guard below reads the snapshot
        // before every sync.
        let arch_book = HelloArchBook::new();
        // The image-arch book runs the other direction: the guard writes what it
        // probed, the control server reads it in `notify` so the arch is stored
        // beside the digest and a later `reconcile` can refuse a wrong-arch
        // delivery the guard could not, having had no connected device to
        // compare against at push time.
        let image_arches = ImageArchBook::new();
        let control = ControlServer::new(
            read_token.clone(),
            DesiredState::default(),
            arch_book.clone(),
            image_arches.clone(),
            // The notify path resolves a tag to the registry manifest digest here.
            Some(store.clone()),
        );
        // Bind the control WS on a RESOLVED, discoverable port (design D9), NOT an
        // ephemeral `0.0.0.0:0` the device could never learn: the device agent is
        // handed `ws_endpoint` at bootstrap and must be able to dial it. The port
        // is the configured/overridden WS port (AVOCADO_CONTAINER_DEV_WS_PORT),
        // distinct from the bulk listener's port; the host component is the same
        // device-reachable host the bulk endpoint resolves to.
        let ws_port = ws_port_override().unwrap_or(DEFAULT_WS_PORT);
        let ws_bind: SocketAddr = format!("0.0.0.0:{ws_port}")
            .parse()
            .expect("a ws port yields a valid bind address");
        let ws_listener = TcpListener::bind(ws_bind)
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
        let mode = topo.sync_mode();
        let project_dir = store
            .root()
            .parent()
            .expect("store root has a per-project parent")
            .to_path_buf();
        let engine_syncer = Arc::new(EngineSyncer::new(
            driver_for(engine).expect("engine driver resolves"),
            syncer_registry,
            write_token.clone(),
            project_dir,
        ));
        let (events_rx, mut events_child) = watch_tag_events(driver)
            .await
            .context("starting the engine event watcher")?;
        let notifier = Arc::clone(&control);
        // Wrap the real syncer in the cross-arch guard (task 4.3) BEFORE anything
        // can push through it. `control` already records every device's
        // `hello.arch` into `arch_book`; without this decorator nothing ever reads
        // that book, so an amd64 host targeting an aarch64 device would build,
        // push and notify a wrong-arch image the device cannot run — the exact
        // silent delivery the guard exists to refuse. A refusal returns `Err`, so
        // the notify is skipped too.
        let syncer: Arc<dyn Syncer> = Arc::new(ArchGuardSyncer::new(
            engine_syncer,
            // A fresh driver handle: `driver` itself was moved into the event
            // watcher above. The probe keeps only the engine binary name.
            Arc::new(EngineArchProbe::new(
                driver_for(engine).expect("engine driver resolves").as_ref(),
            )),
            Arc::new(arch_book),
            image_arches,
        ));
        // The watcher and the manual `sync` trigger share the SAME push+notify
        // primitives (design D5): clone the syncer + control for the trigger
        // before the watcher takes ownership of its copies.
        let trigger_syncer = Arc::clone(&syncer);
        let trigger_notifier = Arc::clone(&control);
        // The declared watch list scopes the watcher too, not just the manual
        // `sync` trigger below: the engine reports every tag on the daemon,
        // including the registry retag each push performs, so an unscoped watcher
        // syncs in response to its own side effect (see `WatchSet`).
        let watched_images: Vec<String> =
            ctx.dev.images.iter().map(|i| i.image_ref.clone()).collect();
        let watch_set = WatchSet::new(watched_images.clone());
        let watcher_task: JoinHandle<()> = tokio::spawn(async move {
            run_watcher(events_rx, mode, syncer, notifier, DEBOUNCE, watch_set).await;
        });

        // The `container dev sync` trigger (task 5.3): a separate `sync`
        // invocation signals this process (SIGUSR1), and each signal drives ONE
        // re-push + notify of every configured watched image through the SAME
        // pipeline the watcher uses — exactly once per signal, never a second
        // watch loop. Reusing the running session's syncer + control WS is what
        // lets the notify reach a connected device with no extra SSH.
        let sync_trigger_task: JoinHandle<()> = tokio::spawn(async move {
            run_sync_trigger(
                mode,
                trigger_syncer,
                trigger_notifier,
                watched_images,
                engine,
            )
            .await;
        });

        // Deliver the bootstrap ONCE per `up` (design D5): the bulk endpoint (the
        // device-reachable address of the bulk listener), the read/control token,
        // and the CA cert — never the write token, never the write-listener
        // address (design G-4). Steady-state sync never re-opens SSH.
        let device_host = bulk_host(&bulk_endpoint, &auto_host);
        let device_bulk_endpoint = format!("{}:{}", device_host, bulk_addr.port());
        // The control-WS endpoint the device dials: the same device-reachable host
        // as the bulk endpoint, on the resolved WS port (design D9/G-4). NEVER the
        // write-listener address, which is never disclosed to a device.
        let device_ws_endpoint = format!("{}:{}", device_host, ws_addr.port());
        let payload =
            DeviceBootstrap::from_session(&session, device_bulk_endpoint, device_ws_endpoint);
        deliver_bootstrap(&device, &payload).await?;

        // On the VM push path, deliver the per-project CA into the avocado-vm
        // engine guest's docker trust store so its daemon trusts the host write
        // listener's leaf per connection (design H4). Delivered at `up` over SSH,
        // NEVER baked into the VM overlay (design D8). `vm_setup` and `vm_target`
        // are both `Some` iff the topology selected the VM push path.
        if let (Some(setup), Some(vm)) = (&vm_setup, &vm_target) {
            deliver_vm_ca(vm, setup).await?;
        }

        // Record the running session (with this process's pid) so `status`/`down`
        // in a separate invocation can find and signal it.
        let state = SessionState {
            pid: std::process::id(),
            status: DevStatus {
                registry_running: true,
                watcher_running: true,
                last_sync: None,
                // Empty for as long as `up` writes this record once and never
                // revisits it; see the per-device caveat in the module docs.
                devices: Vec::new(),
            },
        };
        // The lock claimed at the top of `up` is still held; this only publishes
        // the pid and status for a separate `status`/`down` to read.
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
        // write guard tears down the write listener via Drop (design L-1); the
        // other listeners' tasks are aborted and the state file is cleared.
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

        let Some(state) = load_live_session(&state_path, &session_lock_path(&store))? else {
            bail!(
                "container dev: no active `up` session to sync; run `avocado container dev up` \
                 first, then `sync` re-pushes the current watched image"
            );
        };

        // Trigger exactly one re-push + notify in the running `up` process. The
        // liveness check above is what makes this safe: signalling a stale pid
        // would deliver SIGUSR1 to whatever process recycled that number, and
        // SIGUSR1 terminates by default.
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

        // A session file whose owner is gone would otherwise be reported verbatim,
        // i.e. registry_running=true for listeners that died with the process.
        let Some(state) = load_live_session(&state_path, &session_lock_path(&store))? else {
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

        let Some(state) = load_live_session(&state_path, &session_lock_path(&store))? else {
            print_info(
                "container dev: nothing to tear down (no active `up` session).",
                OutputLevel::Normal,
            );
            return Ok(());
        };

        // Signal the foreground `up` process to shut down. It handles SIGTERM the
        // same as Ctrl-C, tearing down ALL listeners — including the write
        // listener via the guaranteed-cleanup guard (design L-1) — so no
        // authenticated write port survives `down`.
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

/// The remote shell command that writes the bootstrap file at mode 0600.
///
/// Split out of [`deliver_bootstrap`] so the one property that matters here is
/// assertable rather than reviewed by eye: this file carries the Bearer
/// read/control token, and the token must never exist world-readable.
///
/// The umask is in force when the redirect creates the file, so the mode is
/// right from the first byte. Writing the file and then correcting it with
/// `chmod 0600` - which is what this did before - leaves the token on disk at
/// the remote shell's umask (0644 under a default 0022) for the width of two
/// commands, readable by any local user on the device. A subshell keeps the
/// umask change from leaking into anything else `run_command` might later chain.
fn bootstrap_delivery_command(remote_dir: &str, remote_path: &str, encoded: &str) -> String {
    format!(
        "mkdir -p {remote_dir} && (umask 077 && printf %s '{encoded}' | base64 -d > {remote_path})"
    )
}

/// Deliver the bootstrap payload to the device writable partition ONCE (design
/// D5). Renders the JSON, base64-encodes it, and decodes it into
/// `WRITABLE_PARTITION/container-dev/bootstrap.json` over SSH so the payload
/// survives shell quoting untouched, at mode 0600 from creation.
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
    let command = bootstrap_delivery_command(&remote_dir, &remote_path, &encoded);
    ssh.run_command(&command)
        .await
        .context("writing the bootstrap file to the device writable partition")?;
    Ok(())
}

/// Deliver the per-project CA into the avocado-vm engine guest's docker trust
/// store over SSH (task 7.1, design H4).
///
/// Base64-decodes the CA PEM into the guest's `certs.d/<registry>/ca.crt` so its
/// docker daemon trusts the host write listener's leaf per connection (no daemon
/// reload — phase-0 task 1.8). The CA cert is public material (mode 0644); the CA
/// private key is never delivered (design D8), and only the CA *cert* travels in
/// [`VmWriteSetup`]. Delivered at `up`, NEVER baked into the VM overlay.
async fn deliver_vm_ca(vm: &RemoteHost, setup: &VmWriteSetup) -> Result<()> {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(setup.ca_cert_pem.as_bytes());
    let ca_path = &setup.ca_trust_path;
    let ca_dir = std::path::Path::new(ca_path)
        .parent()
        .expect("the CA trust path has a parent directory")
        .to_string_lossy();

    let ssh = SshClient::new(vm.clone());
    let command = format!(
        "mkdir -p {ca_dir} && printf %s '{encoded}' | base64 -d > {ca_path} && chmod 0644 {ca_path}"
    );
    ssh.run_command(&command)
        .await
        .context("delivering the per-project CA into the avocado-vm engine trust store")?;
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

/// Try to take `flag` on `file`, returning `true` when the lock was acquired and
/// `false` when a conflicting lock is already held.
#[cfg(unix)]
fn try_flock(file: &std::fs::File, flag: libc::c_int) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `flock` takes a raw fd plus a flag word and has no memory-safety
    // hazard; `file` owns a valid open fd for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), flag | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    // EWOULDBLOCK (== EAGAIN on Linux and macOS) is the "someone else holds it"
    // answer, which is a result here rather than a failure.
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(false),
        _ => Err(err).context("locking the session lock file"),
    }
}

/// How long `up` waits for the session lock before declaring a competing `up`.
///
/// Long enough to outlast the liveness probe's shared hold (microseconds), short
/// enough that a genuine collision is reported promptly rather than hanging.
const LOCK_ACQUIRE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);
/// Gap between acquire attempts within [`LOCK_ACQUIRE_WAIT`].
const LOCK_ACQUIRE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Take the exclusive lock (`up`'s ownership claim).
#[cfg(unix)]
fn try_lock_exclusive(file: &std::fs::File) -> Result<bool> {
    try_flock(file, libc::LOCK_EX)
}

/// Take a shared lock (the read-only liveness probe).
#[cfg(unix)]
fn try_lock_shared(file: &std::fs::File) -> Result<bool> {
    try_flock(file, libc::LOCK_SH)
}

/// An advisory exclusive lock on the session file, held for the whole life of
/// the foreground `up` process.
///
/// `up` removes `session.json` only on the graceful teardown path, so a panic or
/// a SIGKILL leaves the file behind carrying a pid that is no longer `up`.
/// Signalling that pid is not harmless: pids get recycled, so `sync` would
/// deliver SIGUSR1 — whose default disposition is *terminate* — to whatever
/// unrelated process inherited the number, and `down` would SIGTERM it. A
/// liveness check on the pid alone cannot tell a recycled pid from the original.
///
/// The kernel releases this lock when the holder dies by ANY route, including
/// SIGKILL, so "can I take the lock?" answers the question the pid cannot: no
/// live `up` owns this file. It doubles as the guard against two concurrent
/// `up`s on one project.
#[cfg(unix)]
struct SessionLock {
    _file: std::fs::File,
}

#[cfg(not(unix))]
struct SessionLock;

#[cfg(unix)]
impl SessionLock {
    /// Lock the project's session for this `up`. Fails when another `up` holds it.
    ///
    /// Creates the lock file when absent: `up` takes this before it has written
    /// any state, so requiring the file to pre-exist would make the very first
    /// `up` in a project fail.
    fn acquire(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating the session dir {parent:?}"))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening the session lock at {path:?}"))?;
        // Retry briefly instead of failing on the first EWOULDBLOCK. `LOCK_EX`
        // conflicts with a held `LOCK_SH` just as it does with another `LOCK_EX`,
        // so the read-only liveness probe - which holds a shared lock for
        // microseconds - could make a legitimate `up` abort with "another `up` is
        // already running" when none was. Switching the probe to shared fixed
        // probe-vs-probe only; this is what fixes probe-vs-up.
        //
        // The wait separates the two cases on duration rather than guessing: a
        // probe's hold is over almost immediately, while a real competing `up`
        // holds the lock for its entire lifetime and will still be holding it
        // when the window expires.
        let deadline = Instant::now() + LOCK_ACQUIRE_WAIT;
        loop {
            if try_lock_exclusive(&file)? {
                return Ok(Self { _file: file });
            }
            if Instant::now() >= deadline {
                bail!(
                    "another `avocado container dev up` is already running for this project; \
                     run `avocado container dev down` first"
                );
            }
            std::thread::sleep(LOCK_ACQUIRE_POLL);
        }
    }
}

#[cfg(not(unix))]
impl SessionLock {
    fn acquire(_path: &std::path::Path) -> Result<Self> {
        Ok(Self)
    }
}

/// Whether a live `up` process still owns `path`'s session lock.
///
/// Taking a SHARED lock proves no `up` holds the exclusive one, because the two
/// are mutually exclusive; the lock is dropped immediately since only the answer
/// was wanted. Shared rather than exclusive on purpose: this is a read-only
/// probe, so it must not block a concurrent probe, and it must not make `up`'s
/// own `acquire` fail with "another `up` is already running" merely because a
/// `status` held an exclusive lock for that instant. Opened read-only for the
/// same reason - a read-only mount, or a file this user cannot write, is not a
/// reason for `status` to fail. A missing lock file means no `up` ever ran here.
#[cfg(unix)]
fn session_is_live(path: &std::path::Path) -> Result<bool> {
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("opening the session lock at {path:?}")),
    };
    Ok(!try_lock_shared(&file)?)
}

/// Without `flock` there is no ownership proof — but `signal_shutdown` and
/// `signal_sync` are no-ops off unix, so nothing can be mis-signalled either.
#[cfg(not(unix))]
fn session_is_live(path: &std::path::Path) -> Result<bool> {
    Ok(path.exists())
}

/// The recorded session, but only when a live `up` still owns it.
///
/// `sync`, `status` and `down` all need the same three-step policy - probe the
/// lock, read the state, discard and clear a record whose owner is gone - and
/// each then signals or reports on the result. Keeping it in one place means the
/// "is this pid safe to signal?" rule has a single home rather than three copies
/// to keep in agreement.
///
/// Clears only the state file; the lock file is never removed (see
/// [`session_lock_path`]).
fn load_live_session(
    state_path: &std::path::Path,
    lock_path: &std::path::Path,
) -> Result<Option<SessionState>> {
    if !session_is_live(lock_path)? {
        // No owner: a leftover record describes a process that is gone, and its
        // pid may since have been recycled onto something unrelated.
        let _ = std::fs::remove_file(state_path);
        return Ok(None);
    }
    read_session_state(state_path)
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
    syncer: Arc<dyn Syncer>,
    notifier: Arc<ControlServer>,
    images: Vec<String>,
    engine: &'static str,
) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut usr1 = match signal(SignalKind::user_defined1()) {
        Ok(s) => s,
        // No SIGUSR1 handler available: the trigger is simply inert.
        Err(_) => return,
    };
    while usr1.recv().await.is_some() {
        for image in &images {
            // Ask the engine for the image id. A signal carries no event, so
            // unlike the watcher this path has nothing to read it from - and
            // passing `None` here is not harmless: the notifier turns it into an
            // empty desired digest, which then compares equal to the empty
            // `running_digest` a device reports before its first pull, so the
            // device is silently never told to pull.
            let image_id = match resolve_image_id(engine, image).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    print_warning(
                        &format!(
                            "container dev sync: `{engine}` does not know image `{image}`; \
                             build it first"
                        ),
                        OutputLevel::Normal,
                    );
                    continue;
                }
                Err(e) => {
                    print_warning(
                        &format!("container dev sync: resolving `{image}` failed: {e:#}"),
                        OutputLevel::Normal,
                    );
                    continue;
                }
            };
            let event = TagEvent {
                image: image.clone(),
                image_id: Some(image_id),
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
    _syncer: Arc<dyn Syncer>,
    _notifier: Arc<ControlServer>,
    _images: Vec<String>,
    _engine: &'static str,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The bootstrap file carries the Bearer read/control token, so it must never
    /// exist world-readable - not even briefly.
    ///
    /// The delivery used to write the file and then `chmod 0600` it, which leaves
    /// the token on disk at the remote shell's umask (0644 on a default 0022) for
    /// the width of two commands. The window cannot be observed from a test
    /// without racing the shell, so this asserts the shape that makes it
    /// impossible instead: the mode is established by a umask in force when the
    /// file is created, and there is no separate correcting step afterwards.
    #[test]
    fn bootstrap_delivery_never_creates_a_world_readable_token() {
        let command = bootstrap_delivery_command("/tmp/d", "/tmp/d/bootstrap.json", "YWJj");

        assert!(
            command.contains("umask 077"),
            "the mode must be set by a umask in force at creation: {command}"
        );
        // A chmod means the file existed at some other mode first, which is the
        // whole defect - so its absence is the assertion, not a style preference.
        assert!(
            !command.contains("chmod"),
            "a correcting chmod means the file was created at the wrong mode: {command}"
        );
        // The umask has to precede the redirect to govern it at all.
        let umask_at = command.find("umask 077").expect("umask present");
        let redirect_at = command.find('>').expect("redirect present");
        assert!(
            umask_at < redirect_at,
            "the umask must be in force before the write: {command}"
        );
    }

    /// The generated command is plain POSIX shell, so running it locally proves
    /// the mode it actually produces rather than only its shape.
    #[test]
    fn bootstrap_delivery_command_produces_a_0600_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("container-dev").join("bootstrap.json");
        let command = bootstrap_delivery_command(
            &dir.path().join("container-dev").to_string_lossy(),
            &target.to_string_lossy(),
            // base64 of `{"t":1}`
            "eyJ0IjoxfQ==",
        );

        // A permissive umask in the parent: if the command relied on inheriting a
        // strict one, this would catch it.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("umask 0022 && {command}"))
            .status()
            .expect("running the delivery command");
        assert!(status.success(), "delivery command failed: {command}");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the token file must be created 0600, got {mode:o}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"t\":1}");
    }

    /// A lock nobody holds must read as dead, so `down`/`sync` never signal the
    /// recorded pid. Without this, a `session.json` surviving an unclean exit is
    /// indistinguishable from a running `up` - and the pid it carries may since
    /// have been recycled onto an unrelated process, which SIGUSR1 would
    /// terminate.
    #[test]
    fn an_unheld_lock_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("session.lock");
        std::fs::write(&lock, "").unwrap();

        assert!(
            !session_is_live(&lock).unwrap(),
            "a lock file nobody holds must read as dead"
        );
    }

    /// The lock is what proves liveness, and it is held for as long as the
    /// process that took it lives.
    #[test]
    fn a_held_lock_is_live_and_excludes_a_second_up() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("session.lock");

        // No pre-created file: `up` takes the lock before writing any state, so
        // acquire has to create it.
        let held = SessionLock::acquire(&lock).expect("the first acquire succeeds");
        assert!(lock.exists(), "acquire must create the lock file");
        assert!(
            session_is_live(&lock).unwrap(),
            "a held session lock must read as live"
        );

        // A second `up` on the same project must be refused rather than racing
        // the first one's listeners.
        assert!(
            SessionLock::acquire(&lock).is_err(),
            "a second acquire must be refused while the first is held"
        );

        drop(held);
        assert!(
            !session_is_live(&lock).unwrap(),
            "releasing the lock must make the session read as dead again"
        );
    }

    /// A missing lock file is simply "no session", not an error.
    #[test]
    fn a_missing_lock_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!session_is_live(&dir.path().join("absent.lock")).unwrap());
    }

    /// The probe must not disturb the thing it observes.
    ///
    /// This needs TWO CONCURRENT holders to mean anything, which is what the
    /// earlier version of this test lacked: it ran two sequential probes plus an
    /// acquire on one thread, and `session_is_live` drops its `File` (releasing
    /// the flock) on every return - so every assertion passed with `LOCK_EX`
    /// restored, leaving the whole shared-lock mechanism unverified.
    ///
    /// Holds a real shared lock open across the acquire instead. `LOCK_EX`
    /// conflicts with a held `LOCK_SH`, so without the bounded retry in
    /// `acquire` this is exactly the case that made a legitimate `up` abort while
    /// an IDE task polled `status`.
    #[test]
    fn a_concurrent_probe_does_not_make_up_abort() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("session.lock");
        std::fs::write(&lock, "").unwrap();

        // Two concurrent shared holders coexist - the half that switching the
        // probe to LOCK_SH did fix.
        let probe = std::fs::OpenOptions::new().read(true).open(&lock).unwrap();
        assert!(try_lock_shared(&probe).unwrap());
        let probe2 = std::fs::OpenOptions::new().read(true).open(&lock).unwrap();
        assert!(
            try_lock_shared(&probe2).unwrap(),
            "two concurrent probes must not block each other"
        );
        drop(probe);
        drop(probe2);

        // Now the half it did NOT fix. A probe holds its shared lock briefly, as
        // `session_is_live` does - open, flock, drop - and `up` starts while it is
        // held. `LOCK_EX` conflicts with a held `LOCK_SH`, so without the bounded
        // retry `acquire` fails on the first EWOULDBLOCK and reports a competing
        // `up` that does not exist.
        let holding = std::fs::OpenOptions::new().read(true).open(&lock).unwrap();
        assert!(try_lock_shared(&holding).unwrap());
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            drop(holding);
        });

        let held = SessionLock::acquire(&lock);
        releaser.join().unwrap();
        assert!(
            held.is_ok(),
            "a transient probe must not make `up` report a competing `up`: {:?}",
            held.err()
        );
    }

    /// `session_is_live` must not report a live session merely because ANOTHER
    /// probe is reading at the same instant.
    ///
    /// This is the assertion that actually distinguishes `LOCK_SH` from
    /// `LOCK_EX`, and its absence is why the mechanism went unverified: with an
    /// exclusive probe, a concurrent shared holder makes the flock fail, and
    /// `session_is_live` maps that failure to "someone holds it" - a FALSE
    /// POSITIVE. `status` would report a session running with no `up` alive, and
    /// `down`/`sync` would then signal whatever pid the stale record carried.
    #[test]
    fn a_concurrent_probe_does_not_make_the_session_look_live() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("session.lock");
        std::fs::write(&lock, "").unwrap();

        // Another probe reading concurrently - nobody owns the session.
        let other = std::fs::OpenOptions::new().read(true).open(&lock).unwrap();
        assert!(try_lock_shared(&other).unwrap());

        assert!(
            !session_is_live(&lock).unwrap(),
            "a concurrent reader must not be mistaken for a live `up`"
        );

        drop(other);
        // And the true-positive direction still holds.
        let _held = SessionLock::acquire(&lock).expect("acquire succeeds");
        assert!(
            session_is_live(&lock).unwrap(),
            "a genuinely held lock must still read as live"
        );
    }

    /// The retry must not paper over a REAL collision: a live `up` holds the lock
    /// for its whole lifetime, so a second `up` must still be refused - promptly,
    /// not after a hang.
    #[test]
    fn a_live_up_still_excludes_a_second_up_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("session.lock");

        let _first = SessionLock::acquire(&lock).expect("the first acquire succeeds");

        let started = Instant::now();
        let second = SessionLock::acquire(&lock);
        let waited = started.elapsed();

        assert!(
            second.is_err(),
            "a second `up` must be refused while the first holds the lock"
        );
        assert!(
            waited >= LOCK_ACQUIRE_WAIT,
            "it must actually wait out the window before giving up, waited {waited:?}"
        );
        assert!(
            waited < LOCK_ACQUIRE_WAIT * 4,
            "it must give up promptly rather than hang, waited {waited:?}"
        );
    }

    /// Mutual exclusion has to survive a `down`. The teardown paths unlink
    /// `session.json`, so locking that inode would hand the next `up` a brand
    /// new one and silently drop the guarantee.
    #[test]
    fn clearing_the_state_file_does_not_release_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("session.json");
        let lock = dir.path().join("session.lock");
        std::fs::write(&state, "{}").unwrap();

        let held = SessionLock::acquire(&lock).expect("acquire succeeds");
        // What `down` does to a session it is tearing down.
        std::fs::remove_file(&state).unwrap();

        assert!(
            session_is_live(&lock).unwrap(),
            "unlinking the state file must not release the owner's lock"
        );
        assert!(
            SessionLock::acquire(&lock).is_err(),
            "a second `up` must still be excluded after the state file is cleared"
        );
        drop(held);
    }

    /// `load_live_session` is the single place the stale-record policy lives:
    /// an owner-less record is discarded AND cleared, so no caller signals its
    /// pid.
    #[test]
    fn load_live_session_discards_and_clears_an_ownerless_record() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("session.json");
        let lock = dir.path().join("session.lock");
        std::fs::write(&state, r#"{"pid":999999,"status":{"registry_running":true,"watcher_running":true,"last_sync":null,"devices":[]}}"#).unwrap();
        std::fs::write(&lock, "").unwrap();

        let loaded = load_live_session(&state, &lock).expect("load succeeds");
        assert!(
            loaded.is_none(),
            "a record whose owner is gone must not be returned"
        );
        assert!(
            !state.exists(),
            "the stale record must be cleared, not left for the next caller"
        );
        assert!(
            lock.exists(),
            "the lock inode must survive so exclusion holds for the next `up`"
        );
    }

    /// The live case: an owned record is returned intact.
    #[test]
    fn load_live_session_returns_an_owned_record() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("session.json");
        let lock = dir.path().join("session.lock");
        std::fs::write(&state, r#"{"pid":4242,"status":{"registry_running":true,"watcher_running":true,"last_sync":null,"devices":[]}}"#).unwrap();

        let _held = SessionLock::acquire(&lock).expect("acquire succeeds");
        let loaded = load_live_session(&state, &lock).expect("load succeeds");
        assert_eq!(
            loaded.map(|s| s.pid),
            Some(4242),
            "a record with a live owner must be returned as-is"
        );
    }
}
