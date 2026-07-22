//! Container Dev Mode: embedded OCI Distribution registry and engine-driver
//! dev loop for iterating on containers running on Avocado devices.
//!
//! Scaffolding at this stage. TLS material, the remaining registry listeners,
//! the engine-driver watcher, and sync orchestration are added by later tasks
//! in the `container-dev-mode` change.

// The write-side Basic validator (3.3) and the read/control Bearer validator
// (3.4).
#[allow(dead_code)]
pub mod auth;
// Per-`up` device bootstrap, guaranteed write-listener teardown guard,
// drain-based read/control token rotation, and `status` reporting (task 5.2).
// The `up`/`down`/`status` glue in `commands::container::dev` binds these to the
// live listeners; some helpers are exercised only from that glue, hence
// dead_code here.
#[allow(dead_code)]
pub mod bootstrap;
// One-shot `sync` (re-push + notify, no watcher loop) and `prune` (per-project
// store GC only) command cores (task 5.3); wired to the live listeners/syncer/WS
// by the `commands::container::dev` glue.
pub mod commands;
pub mod config;
// The engine-driver trait + docker/podman drivers (4.1): tag events via the
// engine CLI subprocess (never the API socket). The watcher (4.2/4.3) that
// consumes the event stream and the push wiring that uses the credential hook
// are added later, hence dead_code here.
#[allow(dead_code)]
pub mod engine;
// The store (3.1), OCI read handlers (3.2), and write handlers + auth layer
// (3.3) land before the listeners that bind them: the read router is bound onto
// the dedicated bulk listener by 3.7, the write router onto the distinct write
// listener by 3.6/3.7.
#[allow(dead_code)]
pub mod registry;
#[allow(dead_code)]
pub mod store;
// Per-project CA + leaf, the rustls server config, and the per-session token
// mint (3.6). Bound onto the bulk/WS listeners by 3.7/5.2, hence dead_code here.
#[allow(dead_code)]
pub mod tls;
// Engine-driver watcher + sync orchestration (4.2): topology-selected PUSH/INGEST
// on a debounced tag event, then notify over the control-WS seam. Wired into the
// `up` orchestration (5.2) and the control WS (5.1) later, hence dead_code here.
#[allow(dead_code)]
pub mod watcher;
// Control-only WebSocket channel (5.1): host->device `sync`, device->host
// `hello`/`progress`/`status`; the WS upgrade authenticates through the shared
// read/control-token validator (3.4). Wired into the `up` orchestration (5.2)
// later, hence dead_code here.
#[allow(dead_code)]
pub mod ws;
