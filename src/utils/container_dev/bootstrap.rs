//! Per-`up` device bootstrap, teardown guard, drain-based token rotation, and
//! `status` reporting for Container Dev Mode (task 5.2).
//!
//! This module carries the load-bearing, testable core of the `up`/`down`/
//! `status` lifecycle; the imperative glue that binds listeners and drives a
//! device over SSH lives in [`crate::commands::container::dev`]. Four guarantees
//! from the design + threat model are realized here as unit-testable primitives:
//!
//! - **Bootstrap non-disclosure (design G-4 / D2 / D8).** [`DeviceBootstrap`]
//!   carries EXACTLY the three things a device needs — the BULK-LISTENER endpoint,
//!   the Bearer read/control token, and the per-project CA certificate. It has no
//!   field for the host-only Basic write token or the write-listener address, so
//!   a serialization can never leak either. [`write_bootstrap`] always lands the
//!   file INSIDE the device writable partition (A7).
//! - **Guaranteed write-listener teardown (design L-1).** [`WriteListenerGuard`]
//!   runs its teardown from `Drop`, so an unclean exit (panic, early `?` return,
//!   dropped `up` future) still tears down the routable write listener and its
//!   `0.0.0.0` forward — no authenticated LAN write port survives the process.
//! - **Drain-based read/control rotation (design D5 / G-2 / H-2).**
//!   [`TokenRegistry`] keeps a rotated-out token valid until its in-flight bulk
//!   pulls drain to zero OR a hard ceiling elapses — NOT a fixed timer, which
//!   would 401 an in-flight pull of the largest supported image on a throttled
//!   link (there is no OCI/HTTP "terminal, do not retry" wire signal, so a
//!   mid-stream 401 is re-pulled forever).
//! - **Stale-token surfacing (design H-2).** A device presenting a token that is
//!   neither current nor a still-draining prior token is classified
//!   [`TokenStatus::NeedsReBootstrap`] and surfaced by [`DevStatus`], never looped
//!   on silently.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::auth::ReadToken;
use super::tls::DevSession;

/// The device writable-partition root the bootstrap file lands under (design D5,
/// assumption A7: the dev runtime mounts this rw before bootstrap runs).
pub const WRITABLE_PARTITION: &str = "/var/lib/avocado";

/// The bootstrap file path RELATIVE to the writable-partition root.
pub const BOOTSTRAP_RELATIVE_PATH: &str = "container-dev/bootstrap.json";

/// Environment override for the host endpoint the device reaches the host on
/// (mirrors `avocado deploy`'s `AVOCADO_DEPLOY_REPO_HOST`; design A6/L2). When
/// set it overrides host auto-detection.
pub const HOST_ENV: &str = "AVOCADO_CONTAINER_DEV_HOST";

/// Environment override for the bulk-listener port (design L2). When set it
/// overrides the configured `registry.port`.
pub const PORT_ENV: &str = "AVOCADO_CONTAINER_DEV_PORT";

/// The device-delivery bootstrap payload written once per `up` (design D5).
///
/// It carries EXACTLY three fields — and deliberately no field for the host-only
/// write token or the write-listener endpoint (design G-4/D2). A device is only
/// ever handed the bulk-listener endpoint, so it cannot reach the write listener
/// on any topology; and it never receives the Basic write secret, so a
/// compromised device cannot forge a push. The absence is structural: there is
/// no field to populate, so a serialization can never leak either value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBootstrap {
    /// The BULK read listener endpoint (`host:port`) the device pulls from — the
    /// ONLY endpoint a device is ever handed (design G-4). NEVER the
    /// write-listener address.
    pub bulk_endpoint: String,
    /// The Bearer read/control token the device authenticates pulls and the
    /// control WS with. NEVER the Basic host-only write token (design D2).
    pub read_token: String,
    /// The per-project CA certificate (PEM) the device pins the host TLS leaf
    /// against. NEVER the CA private key (design D8).
    pub ca_cert_pem: String,
}

impl DeviceBootstrap {
    /// Assemble the payload from a minted session plus the resolved bulk
    /// endpoint.
    ///
    /// The read token and CA cert come from the session's device-delivery subset
    /// ([`DevSession::bootstrap_payload`]), which by construction excludes the
    /// write token and the CA private key. The bulk endpoint is supplied by the
    /// caller (task 5.2 resolves it); it must be the bulk listener's address,
    /// never the write listener's (design G-4).
    pub fn from_session(session: &DevSession, bulk_endpoint: impl Into<String>) -> Self {
        let payload = session.bootstrap_payload();
        Self {
            bulk_endpoint: bulk_endpoint.into(),
            read_token: payload.read_token,
            ca_cert_pem: payload.ca_cert_pem,
        }
    }

    /// Render the on-device JSON form.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The absolute on-device path the bootstrap file lands at, always under
/// `writable_root` (design D5 / A7).
pub fn bootstrap_path(writable_root: &Path) -> PathBuf {
    writable_root.join(BOOTSTRAP_RELATIVE_PATH)
}

/// Write the bootstrap file under the device writable-partition root, creating
/// the parent directory, and return the path written.
///
/// One-shot per `up`: task 5.2 calls this exactly once per `up`, never per sync
/// (steady-state sync rides the control WS with no SSH, design D5). The file
/// always lands inside `writable_root`.
pub fn write_bootstrap(writable_root: &Path, bootstrap: &DeviceBootstrap) -> io::Result<PathBuf> {
    let path = bootstrap_path(writable_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = bootstrap.to_json().map_err(io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Pure endpoint resolution (design L2): apply the host + port overrides over the
/// auto-detected host and configured port.
///
/// Kept free of env reads and networking so the precedence is unit-testable; the
/// caller supplies the override values (from [`host_override`] / [`port_override`])
/// and the auto-detected host (from `get_local_ip_for_remote`).
pub fn resolve_endpoint(
    host_override: Option<&str>,
    auto_host: &str,
    port_override: Option<u16>,
    configured_port: u16,
) -> String {
    let host = host_override.unwrap_or(auto_host);
    let port = port_override.unwrap_or(configured_port);
    format!("{host}:{port}")
}

/// The `AVOCADO_CONTAINER_DEV_HOST` override, if set and non-empty.
pub fn host_override() -> Option<String> {
    std::env::var(HOST_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The `AVOCADO_CONTAINER_DEV_PORT` override, if set and a valid port.
pub fn port_override() -> Option<u16> {
    std::env::var(PORT_ENV)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// A guaranteed-cleanup guard for the routable write listener + its `0.0.0.0`
/// hostfwd forward (design L-1).
///
/// `down` calls [`teardown`](Self::teardown) to stop the write listener and
/// remove its LAN forward on the clean path. But an UNCLEAN exit — a panic, an
/// early `?` return, or a dropped `up` future — would skip that call, leaving an
/// authenticated LAN write port bound after the process is gone. Running the
/// teardown from `Drop` closes that hole: whether `up` returns normally or
/// unwinds, the closure runs exactly once, so no authenticated write port
/// survives the process.
pub struct WriteListenerGuard {
    on_teardown: Option<Box<dyn FnOnce() + Send>>,
}

impl WriteListenerGuard {
    /// Wrap a teardown closure that stops the write listener and removes its
    /// `0.0.0.0` forward.
    pub fn new<F: FnOnce() + Send + 'static>(teardown: F) -> Self {
        Self {
            on_teardown: Some(Box::new(teardown)),
        }
    }

    /// Run the teardown now (idempotent). Safe to call on the clean `down` path;
    /// the `Drop` impl then does nothing because the closure was already taken.
    pub fn teardown(&mut self) {
        if let Some(f) = self.on_teardown.take() {
            f();
        }
    }

    /// Whether the teardown has already run.
    pub fn is_torn_down(&self) -> bool {
        self.on_teardown.is_none()
    }
}

impl Drop for WriteListenerGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// The hard ceiling above the worst-case single-blob pull on a throttled link.
///
/// The drain-based grace window (design D5/G-2) never keeps a rotated-out token
/// valid past this, even if its connection count never reaches zero. Sized well
/// above a large-image pull on a slow link so a legitimate in-flight pull is
/// never cut, but bounded so a wedged connection cannot pin the old token open.
pub const DEFAULT_DRAIN_CEILING: Duration = Duration::from_secs(15 * 60);

/// A prior read/control token kept valid while its in-flight bulk pulls drain
/// (design G-2 / H-2).
struct DrainingToken {
    token: ReadToken,
    /// Per-token count of open bulk connections authenticated with this token on
    /// the read listener. The registry keeps the token valid while this is > 0.
    open_connections: Arc<AtomicUsize>,
    /// When the rotation happened, for the hard-ceiling arm.
    since: Instant,
}

/// The device-presented token classification produced by [`TokenRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenStatus {
    /// The presented token is the current one, or a still-draining prior token.
    Accepted,
    /// The device presented a STALE token; the operator must re-run `up` to
    /// re-bootstrap the device (design H-2). Surfaced by `status`, never looped
    /// on silently.
    NeedsReBootstrap,
}

/// Tracks the current read/control token plus one prior token still draining
/// in-flight pulls, and classifies a device-presented token (design D5).
///
/// Rotation at re-`up` is DRAIN-BASED, not a fixed timer: the prior token stays
/// valid until its open bulk connections reach zero OR the hard ceiling elapses.
/// A fixed timer would 401 an in-flight pull of the largest supported image on a
/// throttled link — and because there is no OCI/HTTP "terminal, do not retry"
/// wire signal, that mid-stream 401 is re-pulled forever (design H-2). The drain
/// overlap makes the mid-pull 401 not occur.
pub struct TokenRegistry {
    current: ReadToken,
    draining: Option<DrainingToken>,
    ceiling: Duration,
}

impl TokenRegistry {
    /// A registry seeded with the initial `up` read/control token and the
    /// default drain ceiling.
    pub fn new(current: ReadToken) -> Self {
        Self::with_ceiling(current, DEFAULT_DRAIN_CEILING)
    }

    /// A registry with an explicit drain ceiling (used by tests to exercise the
    /// hard-ceiling arm deterministically).
    pub fn with_ceiling(current: ReadToken, ceiling: Duration) -> Self {
        Self {
            current,
            draining: None,
            ceiling,
        }
    }

    /// The current read/control token.
    pub fn current(&self) -> &ReadToken {
        &self.current
    }

    /// Rotate to `next` on re-`up`, moving the prior token into the draining slot
    /// with its live open-connection counter (`prior_open`).
    ///
    /// The prior token stays valid until `prior_open` reaches zero (all in-flight
    /// pulls drained) OR the ceiling elapses — never a fixed timer.
    pub fn rotate(&mut self, next: ReadToken, prior_open: Arc<AtomicUsize>) {
        let prev = std::mem::replace(&mut self.current, next);
        self.draining = Some(DrainingToken {
            token: prev,
            open_connections: prior_open,
            since: Instant::now(),
        });
    }

    /// Classify a presented token secret at instant `now`.
    ///
    /// A secret matching the current token is always accepted. A secret matching
    /// the draining prior token is accepted only while it has NOT yet drained
    /// (open connections > 0) AND is within the ceiling; once drained OR past the
    /// ceiling it is stale. Anything else is stale.
    pub fn classify_at(&self, secret: &str, now: Instant) -> TokenStatus {
        if self.current.secret() == secret {
            return TokenStatus::Accepted;
        }
        if let Some(d) = &self.draining {
            if d.token.secret() == secret {
                let drained = d.open_connections.load(Ordering::SeqCst) == 0;
                let expired = now.duration_since(d.since) >= self.ceiling;
                return if drained || expired {
                    TokenStatus::NeedsReBootstrap
                } else {
                    TokenStatus::Accepted
                };
            }
        }
        TokenStatus::NeedsReBootstrap
    }

    /// Classify a presented token secret at the current instant.
    pub fn classify(&self, secret: &str) -> TokenStatus {
        self.classify_at(secret, Instant::now())
    }
}

/// A single device's state in a [`DevStatus`] report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    /// The reporting device's stable id.
    pub device_id: String,
    /// Whether the token the device presented is accepted or stale.
    pub token: TokenStatus,
}

/// The `container dev status` report (design D5): registry/watcher/last-sync
/// state plus per-device token classification.
///
/// [`needs_rebootstrap`](Self::needs_rebootstrap) is the surfaced "re-run
/// `up`/bootstrap" signal: it is true when any connected device presented a
/// stale token, so the operator sees a status rather than a silent retry loop
/// (design H-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevStatus {
    /// Whether the embedded registry (bulk + write listeners) is running.
    pub registry_running: bool,
    /// Whether the engine-driver watcher is running.
    pub watcher_running: bool,
    /// The digest last synced to the device, or `None` if nothing synced yet.
    pub last_sync: Option<String>,
    /// Per-device token state.
    pub devices: Vec<DeviceStatus>,
}

impl DevStatus {
    /// Whether any device presented a stale token, so the operator should re-run
    /// `up` to re-bootstrap it (design H-2).
    pub fn needs_rebootstrap(&self) -> bool {
        self.devices
            .iter()
            .any(|d| d.token == TokenStatus::NeedsReBootstrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNTIME: &str = "dev-runtime";
    const BULK_ENDPOINT: &str = "192.168.1.10:5599";

    // ---- bootstrap payload: bulk endpoint + read token + CA, never the write
    //      token and never the write-listener address (design G-4/D2/D8) ----

    #[test]
    fn bootstrap_payload_carries_bulk_endpoint_read_token_and_ca_cert() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let bootstrap = DeviceBootstrap::from_session(&session, BULK_ENDPOINT);

        assert_eq!(bootstrap.bulk_endpoint, BULK_ENDPOINT);
        assert_eq!(bootstrap.read_token, session.read_token.secret());
        assert_eq!(bootstrap.ca_cert_pem, session.tls.ca_cert_pem());

        let json = bootstrap.to_json().expect("payload serializes");
        assert!(
            json.contains(BULK_ENDPOINT),
            "the payload must deliver the bulk-listener endpoint"
        );
        assert!(
            json.contains(session.read_token.secret()),
            "the payload must deliver the read/control token"
        );
        assert!(
            json.contains("BEGIN CERTIFICATE"),
            "the payload must deliver the CA certificate"
        );
    }

    #[test]
    fn bootstrap_payload_never_carries_the_write_token() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let bootstrap = DeviceBootstrap::from_session(&session, BULK_ENDPOINT);
        let json = bootstrap.to_json().expect("payload serializes");
        assert!(
            !json.contains(session.write_token.secret()),
            "the bootstrap payload must NEVER contain the host-only write token (design D2/G-4)"
        );
    }

    #[test]
    fn bootstrap_payload_never_carries_the_ca_private_key() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let json = DeviceBootstrap::from_session(&session, BULK_ENDPOINT)
            .to_json()
            .expect("payload serializes");
        assert!(
            !json.contains("PRIVATE KEY"),
            "the bootstrap payload must NEVER contain CA private key material (design D8)"
        );
    }

    #[test]
    fn bootstrap_payload_has_no_field_for_a_write_endpoint() {
        // Structural guarantee: the ONLY endpoint key is `bulk_endpoint`. A
        // write-listener address has no field to land in, so it cannot leak
        // (design G-4). Pin the exact key set.
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let value: serde_json::Value =
            serde_json::to_value(DeviceBootstrap::from_session(&session, BULK_ENDPOINT))
                .expect("payload serializes to a value");
        let keys: std::collections::BTreeSet<&str> = value
            .as_object()
            .expect("payload is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["bulk_endpoint", "ca_cert_pem", "read_token"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the payload must expose exactly the bulk endpoint, read token, and CA cert - \
             no write-listener endpoint field"
        );
    }

    // ---- write_bootstrap always lands inside the writable partition (A7) ----

    #[test]
    fn write_bootstrap_lands_under_the_writable_partition_root() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let bootstrap = DeviceBootstrap::from_session(&session, BULK_ENDPOINT);
        let root = tempfile::tempdir().expect("tempdir");

        let path = write_bootstrap(root.path(), &bootstrap).expect("bootstrap writes");

        assert!(
            path.starts_with(root.path()),
            "the bootstrap file must land INSIDE the writable-partition root: {path:?}"
        );
        assert_eq!(path, bootstrap_path(root.path()));
        assert!(path.exists(), "the bootstrap file must exist after writing");

        let written = std::fs::read_to_string(&path).expect("read back");
        let round: DeviceBootstrap =
            serde_json::from_str(&written).expect("written payload round-trips");
        assert_eq!(round, bootstrap);
    }

    #[test]
    fn bootstrap_path_is_relative_to_the_writable_partition() {
        let path = bootstrap_path(Path::new(WRITABLE_PARTITION));
        assert_eq!(
            path,
            Path::new(WRITABLE_PARTITION).join(BOOTSTRAP_RELATIVE_PATH),
            "the on-device path must sit under the writable partition"
        );
        assert!(path.starts_with(WRITABLE_PARTITION));
    }

    // ---- endpoint resolution precedence (design L2) ----

    #[test]
    fn resolve_endpoint_uses_auto_host_and_configured_port_by_default() {
        assert_eq!(
            resolve_endpoint(None, "10.0.0.5", None, 5599),
            "10.0.0.5:5599"
        );
    }

    #[test]
    fn resolve_endpoint_applies_host_and_port_overrides() {
        assert_eq!(
            resolve_endpoint(Some("host.override"), "10.0.0.5", Some(6001), 5599),
            "host.override:6001",
            "the host and port overrides must take precedence over auto-detection"
        );
    }

    // ---- guaranteed write-listener teardown (design L-1) ----

    #[test]
    fn write_listener_guard_tears_down_on_explicit_teardown() {
        let torn = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&torn);
        let mut guard = WriteListenerGuard::new(move || {
            flag.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!guard.is_torn_down());
        guard.teardown();
        assert!(guard.is_torn_down());
        assert_eq!(torn.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn write_listener_guard_tears_down_even_on_an_error_path() {
        // Simulate `up` failing partway through after the routable write listener
        // was bound. The guard is dropped on the early `?` return, and its
        // teardown MUST still run so no authenticated LAN write port survives.
        let torn = Arc::new(AtomicUsize::new(0));

        fn faulty_up(torn: Arc<AtomicUsize>) -> Result<(), &'static str> {
            let flag = Arc::clone(&torn);
            let _guard = WriteListenerGuard::new(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            });
            // Fail after the write listener is up: the `?`-style early return
            // drops the guard without an explicit teardown call.
            Err("bootstrap delivery failed")?;
            Ok(())
        }

        let result = faulty_up(Arc::clone(&torn));
        assert!(result.is_err(), "the simulated up must fail");
        assert_eq!(
            torn.load(Ordering::SeqCst),
            1,
            "the write listener must be torn down on the error path via Drop (design L-1)"
        );
    }

    #[test]
    fn write_listener_guard_runs_teardown_exactly_once() {
        let torn = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&torn);
        {
            let mut guard = WriteListenerGuard::new(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            });
            guard.teardown();
            // Dropping after an explicit teardown must not run it a second time.
        }
        assert_eq!(
            torn.load(Ordering::SeqCst),
            1,
            "teardown must run exactly once across an explicit call plus Drop"
        );
    }

    // ---- stale-token surfacing (design H-2) ----

    #[test]
    fn an_unknown_token_is_classified_needs_rebootstrap() {
        let registry = TokenRegistry::new(ReadToken::new("current-token"));
        assert_eq!(
            registry.classify("current-token"),
            TokenStatus::Accepted,
            "the current token must be accepted"
        );
        assert_eq!(
            registry.classify("some-old-token"),
            TokenStatus::NeedsReBootstrap,
            "a device presenting a stale token must surface a re-bootstrap status, not loop"
        );
    }

    // ---- drain-based read/control rotation (design D5/G-2/H-2) ----

    #[test]
    fn rotation_holds_the_old_token_until_in_flight_pulls_drain() {
        let mut registry = TokenRegistry::new(ReadToken::new("token-a"));
        // One in-flight bulk pull is authenticated with token-a on the read
        // listener.
        let open = Arc::new(AtomicUsize::new(1));

        registry.rotate(ReadToken::new("token-b"), Arc::clone(&open));

        // The new token is current; the old token is STILL valid because a pull
        // is in flight (draining, not yet zero).
        assert_eq!(registry.classify("token-b"), TokenStatus::Accepted);
        assert_eq!(
            registry.classify("token-a"),
            TokenStatus::Accepted,
            "the prior token must stay valid while an in-flight pull has not drained"
        );

        // The in-flight pull completes: the connection count drains to zero.
        open.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.classify("token-a"),
            TokenStatus::NeedsReBootstrap,
            "the prior token must retire once its in-flight pulls have drained to zero"
        );
    }

    #[test]
    fn rotation_is_drain_based_not_a_fixed_timer() {
        // A large ceiling stands in for "well past any fixed timer would fire".
        // With a pull still in flight, the old token must remain valid regardless
        // of elapsed time - proving the overlap is keyed on drain, not a timer
        // that would 401 the largest in-flight image on a slow link.
        let mut registry = TokenRegistry::new(ReadToken::new("token-a"));
        let open = Arc::new(AtomicUsize::new(1));
        registry.rotate(ReadToken::new("token-b"), Arc::clone(&open));

        let long_after = Instant::now() + Duration::from_secs(10 * 60);
        assert_eq!(
            registry.classify_at("token-a", long_after),
            TokenStatus::Accepted,
            "with a pull still in flight the old token must remain valid regardless of elapsed \
             time - a fixed timer would have 401'd the in-flight pull"
        );
    }

    #[test]
    fn a_hard_ceiling_retires_a_wedged_prior_token_even_if_connections_remain() {
        // A short ceiling: even though a connection never drains (count stays 1),
        // the ceiling forces the prior token to retire so a wedged connection
        // cannot pin the old credential open forever (design D5, the OR arm).
        let ceiling = Duration::from_secs(60);
        let mut registry = TokenRegistry::with_ceiling(ReadToken::new("token-a"), ceiling);
        let open = Arc::new(AtomicUsize::new(1));
        registry.rotate(ReadToken::new("token-b"), Arc::clone(&open));

        // Within the ceiling: still valid (drain overlap active).
        assert_eq!(registry.classify("token-a"), TokenStatus::Accepted);

        // Past the ceiling with the connection still open: forced retirement.
        let past_ceiling = Instant::now() + ceiling + Duration::from_secs(1);
        assert_eq!(
            registry.classify_at("token-a", past_ceiling),
            TokenStatus::NeedsReBootstrap,
            "the hard ceiling must retire a prior token even if its connections never drain"
        );
    }

    // ---- status surfacing (design D5/H-2) ----

    #[test]
    fn dev_status_surfaces_rebootstrap_when_any_device_is_stale() {
        let stale = DevStatus {
            registry_running: true,
            watcher_running: true,
            last_sync: Some("sha256:abc".to_string()),
            devices: vec![
                DeviceStatus {
                    device_id: "dev-1".to_string(),
                    token: TokenStatus::Accepted,
                },
                DeviceStatus {
                    device_id: "dev-2".to_string(),
                    token: TokenStatus::NeedsReBootstrap,
                },
            ],
        };
        assert!(
            stale.needs_rebootstrap(),
            "a status with any stale-token device must surface the re-bootstrap state"
        );

        let json = serde_json::to_string(&stale).expect("status serializes");
        assert!(json.contains("registry_running"), "status reports registry");
        assert!(json.contains("watcher_running"), "status reports watcher");
        assert!(json.contains("last_sync"), "status reports last-sync");
        assert!(
            json.contains("needs_re_bootstrap"),
            "the stale device's token state must serialize the re-bootstrap variant: {json}"
        );
    }

    #[test]
    fn dev_status_is_clean_when_all_devices_are_accepted() {
        let clean = DevStatus {
            registry_running: true,
            watcher_running: true,
            last_sync: None,
            devices: vec![DeviceStatus {
                device_id: "dev-1".to_string(),
                token: TokenStatus::Accepted,
            }],
        };
        assert!(
            !clean.needs_rebootstrap(),
            "a status with only accepted-token devices must not signal a re-bootstrap"
        );
    }
}
