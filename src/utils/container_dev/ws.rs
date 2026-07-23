//! Control-only WebSocket channel (design D9; task 5.1).
//!
//! The host and device exchange ONLY control frames over this channel:
//!
//! - host -> device: [`HostFrame::Sync`] `{image, tag, digest}` — the image now
//!   available to pull. It carries a digest *reference*, never blob bytes: bulk
//!   blob/manifest transfers ride the dedicated bulk HTTPS listener (design D9,
//!   tasks 3.7/6.2), NOT this WS. The [`HostFrame`] enum has no blob variant by
//!   construction, so a blob transfer cannot be sent as a WS frame.
//! - device -> host: [`DeviceFrame::Hello`] `{device_id, arch, running_digest}`,
//!   [`DeviceFrame::Progress`], and [`DeviceFrame::Status`].
//!
//! Two behaviors are load-bearing (design D5/H2):
//!
//! 1. **Desired-state is RE-DERIVED at `up`, never assumed persistent.**
//!    [`DesiredState`] is built solely from the engine's current watched tags
//!    ([`DesiredState::derive_from_watched_tags`]); there is no disk/restore
//!    constructor. After a host restart the host rebuilds it from the engine's
//!    *current* tags, so a digest that changed while the host was down is
//!    reflected, not restored from a stale snapshot.
//! 2. **On (re)connect the host reconciles the device's `running_digest`.** A
//!    device that reconnects reporting a digest that no longer matches the
//!    desired state is driven back to current with a reconcile [`HostFrame::Sync`]
//!    ([`DesiredState::reconcile`]).
//!
//! The WS upgrade authenticates through the SAME read/control-token validator
//! seam the bulk listener uses ([`super::auth::read_request_authorized`], task
//! 3.4) — the WS is NOT a second, separately-implemented auth surface (design
//! G-5). A WebSocket upgrade is an HTTP `GET` carrying the same `Authorization`
//! header, so the upgrade callback hands that header straight to the shared
//! validator.
//!
//! This module realizes the watcher's [`super::watcher::Notifier`] seam (task
//! 4.2): [`ControlServer`] broadcasts a [`HostFrame::Sync`] to every connected
//! device when the watcher reports a new tag, and records each device's
//! `hello.arch` into the [`super::watcher::arch_guard::HelloArchBook`] the
//! cross-arch guard (task 4.3) reads.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{header, StatusCode};
use tokio_tungstenite::tungstenite::Message;

use super::auth::{read_request_authorized, ReadToken};
use super::engine::TagEvent;
use super::watcher::arch_guard::HelloArchBook;
use super::watcher::Notifier;

/// A host -> device control frame.
///
/// There is exactly ONE variant, [`HostFrame::Sync`], and it carries only image
/// coordinates plus a content-digest *reference* — never blob bytes. This is the
/// structural guarantee that a bulk transfer can never ride the control WS
/// (design D9): the type has no frame that could carry a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// The `{image, tag, digest}` now available for the device to pull over the
    /// dedicated bulk listener. `digest` is a `sha256:…` reference, not content.
    Sync {
        /// Repository component of the watched image (e.g. `my-app`).
        image: String,
        /// Tag component (e.g. `dev`).
        tag: String,
        /// Content digest (`sha256:…`) the device should be running.
        digest: String,
    },
}

/// A device -> host control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceFrame {
    /// Sent on connect and reconnect; carries the digest the device currently
    /// runs so the host can reconcile it against the desired state.
    Hello(Hello),
    /// Progress of an in-flight pull (informational).
    Progress(Progress),
    /// A device state report (informational).
    Status(Status),
}

/// The device's `hello`: who it is, its CPU arch, and the digest it runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Stable per-device identity.
    pub device_id: String,
    /// The device CPU architecture (`uname -m` form, e.g. `aarch64`), recorded
    /// into the cross-arch guard's [`HelloArchBook`].
    pub arch: String,
    /// The content digest the device is currently running. Empty on a device
    /// that has not yet pulled anything.
    pub running_digest: String,
}

/// Progress of an in-flight device pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// The image the progress refers to.
    pub image: String,
    /// Bytes pulled so far.
    pub bytes_pulled: u64,
}

/// A device state report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The reporting device.
    pub device_id: String,
    /// A short state token (e.g. `running`, `restarting`).
    pub state: String,
    /// Optional human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Split an image reference (`[registry/]repo[:tag]`) into `(repo, tag)`.
///
/// Strips a leading registry qualifier (podman writes `localhost/my-app:dev`)
/// and defaults a missing tag to `latest`, matching engine semantics.
fn split_image_tag(image: &str) -> (String, String) {
    let without_registry = match image.split_once('/') {
        Some((first, rest))
            if first == "localhost" || first.contains('.') || first.contains(':') =>
        {
            rest
        }
        _ => image,
    };
    match without_registry.rsplit_once(':') {
        Some((repo, tag)) => (repo.to_string(), tag.to_string()),
        None => (without_registry.to_string(), "latest".to_string()),
    }
}

/// The host's desired container state: `(image, tag) -> digest`.
///
/// RE-DERIVED at every `up` from the engine's current watched tags (design D5);
/// there is deliberately NO `Deserialize`/disk-restore path, so the desired
/// state cannot be silently loaded from a stale snapshot across a host restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesiredState {
    by_tag: BTreeMap<(String, String), String>,
}

impl DesiredState {
    /// Re-derive the desired state from the engine's CURRENT watched tags at
    /// `up` (design D5).
    ///
    /// This is the ONLY way to populate a [`DesiredState`]: the desired mapping
    /// is a function of what the engine reports *now*, never a persisted value.
    /// Each item is `(image, tag, digest)`.
    pub fn derive_from_watched_tags<I>(watched: I) -> Self
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        let by_tag = watched
            .into_iter()
            .map(|(image, tag, digest)| ((image, tag), digest))
            .collect();
        Self { by_tag }
    }

    /// Record a fresh `(image, tag) -> digest` after a new sync so a later
    /// reconcile compares against the just-pushed digest.
    pub fn record_sync(&mut self, image: &str, tag: &str, digest: &str) {
        self.by_tag
            .insert((image.to_string(), tag.to_string()), digest.to_string());
    }

    /// The desired digest for `(image, tag)`, if watched.
    pub fn digest_for(&self, image: &str, tag: &str) -> Option<&str> {
        self.by_tag
            .get(&(image.to_string(), tag.to_string()))
            .map(String::as_str)
    }

    /// The desired entries as `(image, tag, digest)` triples.
    pub fn entries(&self) -> Vec<(String, String, String)> {
        self.by_tag
            .iter()
            .map(|((image, tag), digest)| (image.clone(), tag.clone(), digest.clone()))
            .collect()
    }

    /// Reconcile a device's reported `running_digest` against the desired state
    /// (design H2).
    ///
    /// Returns a [`HostFrame::Sync`] for every desired entry whose digest does
    /// NOT match what the device runs — driving a device that reconnected with a
    /// stale digest back to current. A device already on the desired digest
    /// yields no sync.
    pub fn reconcile(&self, hello: &Hello) -> Vec<HostFrame> {
        self.by_tag
            .iter()
            .filter(|(_, digest)| digest.as_str() != hello.running_digest)
            .map(|((image, tag), digest)| HostFrame::Sync {
                image: image.clone(),
                tag: tag.clone(),
                digest: digest.clone(),
            })
            .collect()
    }
}

/// The control-WS server: authenticates each upgrade through the shared
/// read/control-token seam, reconciles a device's `hello`, and broadcasts
/// host -> device `sync` frames (realizing the watcher's [`Notifier`] seam).
///
/// Held behind an [`Arc`] so the accept loop, per-connection tasks, and the
/// watcher's notify path all share one instance.
pub struct ControlServer {
    /// The per-session Bearer read/control token every WS upgrade is validated
    /// against — the SAME token the bulk listener uses (design G-5).
    read_token: ReadToken,
    /// The desired state, re-derived at `up`; updated on each notify.
    desired: Mutex<DesiredState>,
    /// The cross-arch guard's device-arch book, populated from `hello.arch`.
    arch_book: HelloArchBook,
    /// Host -> device fan-out of `sync` frames; each connection subscribes.
    tx: broadcast::Sender<HostFrame>,
}

impl ControlServer {
    /// Build a server over `read_token`, the up-time `desired` state, and the
    /// cross-arch guard's `arch_book`.
    pub fn new(
        read_token: ReadToken,
        desired: DesiredState,
        arch_book: HelloArchBook,
    ) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(64);
        Arc::new(Self {
            read_token,
            desired: Mutex::new(desired),
            arch_book,
            tx,
        })
    }

    /// Serve control-WS connections on `listener`, terminating TLS with
    /// `acceptor` before any WebSocket byte is read (design D8/D9).
    ///
    /// This is the production entry point: the device agent connects over
    /// `wss://` and pins the per-project session CA, so the control WS enforces
    /// the same pinned-CA TLS guarantee the bulk listener does
    /// ([`super::registry::BulkListener`]). Each accepted TCP stream is
    /// handshaked with the per-project leaf (task 3.6) and, on success, upgraded
    /// (with auth) and served on its own task over the resulting
    /// [`tokio_rustls::server::TlsStream`]. A TLS handshake failure is a
    /// per-connection concern (a client that does not trust the session CA, or a
    /// probe): the connection is dropped and the accept loop keeps serving,
    /// mirroring [`super::registry`]'s bulk `TlsListener`.
    pub async fn serve_tls(self: Arc<Self>, listener: TcpListener, acceptor: TlsAcceptor) {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                // Drop a connection whose TLS handshake fails and keep serving;
                // do not surface it, do not busy-spin.
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = server.handle_connection(tls).await;
            });
        }
    }

    /// Accept control-WS connections on `listener` over PLAIN TCP.
    ///
    /// Test-only: production binds the control WS over pinned-CA TLS via
    /// [`serve_tls`](Self::serve_tls). This entry exists so the transport-agnostic
    /// control logic can be exercised over plain TCP exactly as the auth-module
    /// tests do, without a TLS handshake in the loop. It is gated `#[cfg(test)]`
    /// so no production path can ever bind the control WS in plaintext.
    #[cfg(test)]
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                return;
            };
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = server.handle_connection(stream).await;
            });
        }
    }

    /// Upgrade one stream (authenticating via the shared seam) then serve its
    /// control frames.
    ///
    /// Generic over the transport `S` so the SAME connection-handling core drives
    /// both the production TLS stream (`TlsStream<TcpStream>`) and the plain-TCP
    /// stream tests use — the read/control-token validator seam is shared, never
    /// duplicated per transport (design G-5).
    async fn handle_connection<S>(self: Arc<Self>, stream: S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let ws = self.accept_authenticated(stream).await?;
        self.run_session(ws).await
    }

    /// Perform the WebSocket upgrade, rejecting a client that lacks a valid
    /// Bearer read/control token.
    ///
    /// The upgrade callback delegates to [`read_request_authorized`] — the exact
    /// function the bulk listener's middleware uses — so the WS cannot diverge
    /// from the bulk auth surface (design G-5). A rejected upgrade returns `401`
    /// with a bare `Bearer` challenge, matching the read listener (design L-1).
    // The upgrade callback's `Result<Response, ErrorResponse>` shape is imposed
    // verbatim by tungstenite's `accept_hdr_async` contract, so the large-err
    // lint cannot be satisfied by boxing without breaking the trait bound.
    #[allow(clippy::result_large_err)]
    async fn accept_authenticated<S>(
        &self,
        stream: S,
    ) -> Result<tokio_tungstenite::WebSocketStream<S>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let token = self.read_token.clone();
        let callback =
            move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                if read_request_authorized(request.headers(), &token) {
                    Ok(response)
                } else {
                    let err = tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .header(header::WWW_AUTHENTICATE, "Bearer")
                        .body(Some("read/control token required".to_string()))
                        .expect("static 401 response builds");
                    Err(err)
                }
            };
        tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .context("control-WS upgrade")
    }

    /// Serve one authenticated connection: reconcile on `hello`, fan out
    /// broadcast `sync` frames, and drain informational device frames.
    async fn run_session<S>(
        self: Arc<Self>,
        mut ws: tokio_tungstenite::WebSocketStream<S>,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut broadcasts = self.tx.subscribe();
        loop {
            tokio::select! {
                incoming = ws.next() => match incoming {
                    Some(Ok(msg)) => {
                        if let Some(frames) = self.on_device_message(&msg) {
                            for frame in frames {
                                ws.send(encode(&frame)?).await?;
                            }
                        }
                    }
                    // Connection closed or errored: end the session.
                    Some(Err(_)) | None => return Ok(()),
                },
                host = broadcasts.recv() => match host {
                    Ok(frame) => ws.send(encode(&frame)?).await?,
                    // Lagged past the buffer: skip the missed frames, keep serving.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }

    /// Handle one device -> host frame, returning any host -> device frames to
    /// send in response (the reconcile syncs for a `hello`).
    fn on_device_message(&self, msg: &Message) -> Option<Vec<HostFrame>> {
        let text = msg.to_text().ok()?;
        let frame: DeviceFrame = serde_json::from_str(text).ok()?;
        match frame {
            DeviceFrame::Hello(hello) => {
                // Record the device arch for the cross-arch guard (task 4.3).
                self.arch_book.record_hello(&hello.device_id, &hello.arch);
                // Reconcile the reported running_digest against the desired state.
                Some(self.desired.lock().unwrap().reconcile(&hello))
            }
            // Progress/Status are informational; no host response.
            DeviceFrame::Progress(_) | DeviceFrame::Status(_) => None,
        }
    }
}

impl Notifier for ControlServer {
    /// Notify every connected device that `event`'s image is available: update
    /// the desired state with the new digest and broadcast a [`HostFrame::Sync`].
    ///
    /// Only a control `sync` frame is ever sent — the bulk pull rides the
    /// dedicated listener (design D9), never this WS.
    fn notify<'a>(
        &'a self,
        event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let (image, tag) = split_image_tag(&event.image);
            let digest = event.image_id.clone().unwrap_or_default();
            self.desired
                .lock()
                .unwrap()
                .record_sync(&image, &tag, &digest);
            let frame = HostFrame::Sync { image, tag, digest };
            // A send with no connected devices is not an error (nobody to notify
            // yet); a later `hello` reconciles them.
            let _ = self.tx.send(frame);
            Ok(())
        })
    }
}

/// Serialize a [`HostFrame`] into a WebSocket text message.
fn encode(frame: &HostFrame) -> Result<Message> {
    let json = serde_json::to_string(frame).context("serializing a control frame")?;
    Ok(Message::Text(json.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    const READ_TOKEN: &str = "read-control-token";

    fn hello(running_digest: &str) -> Hello {
        Hello {
            device_id: "dev-1".to_string(),
            arch: "aarch64".to_string(),
            running_digest: running_digest.to_string(),
        }
    }

    // ---- frame protocol: control-only, no blob carrier (design D9) ----

    #[test]
    fn a_sync_frame_round_trips_and_carries_only_a_digest_reference() {
        let frame = HostFrame::Sync {
            image: "my-app".to_string(),
            tag: "dev".to_string(),
            digest: "sha256:abc".to_string(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        // The wire form is tagged and carries a digest *reference*, never bytes.
        assert!(json.contains("\"type\":\"sync\""), "tagged as sync: {json}");
        assert!(json.contains("sha256:abc"), "carries the digest: {json}");
        let back: HostFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn the_only_host_frame_is_sync_so_no_blob_can_ride_the_ws() {
        // Structural guarantee: HostFrame has exactly one variant, Sync, which
        // carries image coordinates + a digest reference. There is no variant a
        // blob/bulk byte stream could be placed into, so a bulk transfer cannot
        // be sent as a WS frame (design D9). This test pins that: if a blob-bytes
        // variant were ever added, the exhaustive match below stops compiling.
        let frame = HostFrame::Sync {
            image: "a".into(),
            tag: "b".into(),
            digest: "sha256:c".into(),
        };
        match frame {
            HostFrame::Sync { .. } => {}
        }
    }

    #[test]
    fn device_frames_round_trip() {
        let frames = vec![
            DeviceFrame::Hello(hello("sha256:run")),
            DeviceFrame::Progress(Progress {
                image: "my-app:dev".into(),
                bytes_pulled: 42,
            }),
            DeviceFrame::Status(Status {
                device_id: "dev-1".into(),
                state: "running".into(),
                detail: None,
            }),
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).unwrap();
            let back: DeviceFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(back, frame);
        }
    }

    // ---- desired-state: re-derived at up, never persisted (design D5) ----

    #[test]
    fn desired_state_is_derived_from_current_watched_tags() {
        let desired = DesiredState::derive_from_watched_tags([
            (
                "my-app".to_string(),
                "dev".to_string(),
                "sha256:aaa".to_string(),
            ),
            (
                "side".to_string(),
                "latest".to_string(),
                "sha256:bbb".to_string(),
            ),
        ]);
        assert_eq!(desired.digest_for("my-app", "dev"), Some("sha256:aaa"));
        assert_eq!(desired.digest_for("side", "latest"), Some("sha256:bbb"));
        assert_eq!(desired.digest_for("absent", "dev"), None);
    }

    #[test]
    fn a_second_up_rederives_desired_state_from_the_new_current_tags() {
        // First `up`: the engine's current watched tag is digest aaa.
        let first_up = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:aaa".to_string(),
        )]);
        assert_eq!(first_up.digest_for("my-app", "dev"), Some("sha256:aaa"));

        // The image is rebuilt while the host is down; the engine's current tag
        // is now digest bbb. A fresh `up` RE-DERIVES from the current tags — it
        // must reflect bbb, not restore the stale aaa from any persisted state.
        let second_up = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:bbb".to_string(),
        )]);
        assert_eq!(
            second_up.digest_for("my-app", "dev"),
            Some("sha256:bbb"),
            "desired state must be re-derived from current tags, not persisted"
        );
    }

    // ---- reconcile: a stale running_digest is driven back to current (H2) ----

    #[test]
    fn a_stale_running_digest_reconciles_to_a_sync() {
        let desired = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:new".to_string(),
        )]);
        // The device reports it is running an older digest.
        let frames = desired.reconcile(&hello("sha256:old"));
        assert_eq!(
            frames,
            vec![HostFrame::Sync {
                image: "my-app".to_string(),
                tag: "dev".to_string(),
                digest: "sha256:new".to_string(),
            }],
            "a stale running_digest must produce a reconcile sync to the desired digest"
        );
    }

    #[test]
    fn a_device_already_on_the_desired_digest_needs_no_sync() {
        let desired = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:current".to_string(),
        )]);
        assert!(
            desired.reconcile(&hello("sha256:current")).is_empty(),
            "a device already on the desired digest must not be reconciled"
        );
    }

    #[test]
    fn split_image_tag_strips_registry_and_defaults_tag() {
        assert_eq!(
            split_image_tag("my-app:dev"),
            ("my-app".into(), "dev".into())
        );
        assert_eq!(
            split_image_tag("localhost/my-app:dev"),
            ("my-app".into(), "dev".into())
        );
        assert_eq!(
            split_image_tag("my-app"),
            ("my-app".into(), "latest".into())
        );
    }

    // ---- WS upgrade authenticates via the SHARED read/control validator (G-5) ----

    /// Spawn a control server over plain TCP; return its `ws://` base URL and the
    /// server handle so a test can also drive its notify path.
    async fn spawn_server(desired: DesiredState) -> (String, Arc<ControlServer>) {
        let server = ControlServer::new(ReadToken::new(READ_TOKEN), desired, HelloArchBook::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = Arc::clone(&server);
        tokio::spawn(async move { serve.serve(listener).await });
        (format!("ws://{addr}/"), server)
    }

    /// A client upgrade request carrying the Bearer read/control token.
    fn authed_request(url: &str, token: &str) -> Request {
        let mut req = url.into_client_request().unwrap();
        req.headers_mut()
            .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        req
    }

    #[tokio::test]
    async fn an_upgrade_without_the_read_control_token_is_rejected() {
        let (url, _server) = spawn_server(DesiredState::default()).await;
        // No Authorization header at all.
        let err = tokio_tungstenite::connect_async(url.into_client_request().unwrap())
            .await
            .expect_err("an unauthenticated WS upgrade must be rejected");
        match err {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                assert_eq!(
                    resp.status(),
                    StatusCode::UNAUTHORIZED,
                    "a tokenless upgrade must be 401"
                );
            }
            other => panic!("expected an HTTP 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_write_token_shape_is_rejected_on_the_ws_upgrade() {
        // A Basic credential (the write token's transport form) must never
        // authorize the control WS — the shared validator only accepts Bearer.
        use base64::Engine as _;
        let (url, _server) = spawn_server(DesiredState::default()).await;
        let mut req = url.into_client_request().unwrap();
        let basic = base64::engine::general_purpose::STANDARD.encode("avocado:write-secret");
        req.headers_mut()
            .insert(AUTHORIZATION, format!("Basic {basic}").parse().unwrap());
        let err = tokio_tungstenite::connect_async(req)
            .await
            .expect_err("a Basic write credential must not authorize the control WS");
        assert!(
            matches!(err, tokio_tungstenite::tungstenite::Error::Http(resp) if resp.status() == StatusCode::UNAUTHORIZED),
            "the write-token shape must be refused on the WS upgrade with 401"
        );
    }

    #[tokio::test]
    async fn an_upgrade_with_the_read_control_token_is_accepted() {
        let (url, _server) = spawn_server(DesiredState::default()).await;
        let (ws, resp) = tokio_tungstenite::connect_async(authed_request(&url, READ_TOKEN))
            .await
            .expect("a valid read/control token must be accepted");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        drop(ws);
    }

    // ---- end-to-end: a hello with a stale digest triggers a reconcile sync ----

    #[tokio::test]
    async fn a_hello_with_a_stale_running_digest_triggers_a_reconcile_sync() {
        let desired = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:new".to_string(),
        )]);
        let (url, _server) = spawn_server(desired).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(authed_request(&url, READ_TOKEN))
            .await
            .unwrap();

        // The device announces it is running the OLD digest.
        let hello = DeviceFrame::Hello(hello("sha256:old"));
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // The host must reconcile it back to the desired digest with a sync.
        let msg = ws.next().await.expect("a reconcile sync").unwrap();
        let frame: HostFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(
            frame,
            HostFrame::Sync {
                image: "my-app".to_string(),
                tag: "dev".to_string(),
                digest: "sha256:new".to_string(),
            },
            "a reconnect with a stale running_digest must reconcile to the desired digest"
        );
    }

    #[tokio::test]
    async fn a_hello_already_on_the_desired_digest_gets_no_sync() {
        let desired = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:current".to_string(),
        )]);
        let (url, _server) = spawn_server(desired).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(authed_request(&url, READ_TOKEN))
            .await
            .unwrap();

        ws.send(Message::Text(
            serde_json::to_string(&DeviceFrame::Hello(hello("sha256:current")))
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();

        // No reconcile should arrive; a short timeout confirms silence rather
        // than a spurious sync.
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), ws.next()).await;
        assert!(
            quiet.is_err(),
            "a device already on the desired digest must not receive a sync"
        );
    }

    // ---- the notify seam broadcasts a control sync, never a blob (D9/4.2) ----

    #[tokio::test]
    async fn notify_broadcasts_a_control_sync_frame_to_a_connected_device() {
        let (url, server) = spawn_server(DesiredState::default()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(authed_request(&url, READ_TOKEN))
            .await
            .unwrap();

        // Connect and announce a matching hello (empty desired -> no reconcile),
        // so the device is subscribed before the notify fires.
        ws.send(Message::Text(
            serde_json::to_string(&DeviceFrame::Hello(hello("")))
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
        // Let the server register the subscription.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The watcher reports a new tag over the Notifier seam.
        let event = TagEvent {
            image: "my-app:dev".to_string(),
            image_id: Some("sha256:fresh".to_string()),
        };
        server.notify(&event).await.unwrap();

        let msg = ws.next().await.expect("a broadcast sync").unwrap();
        // It is a text control frame carrying the digest reference — never binary
        // blob content.
        assert!(msg.is_text(), "a control frame is text, not a binary blob");
        let frame: HostFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(
            frame,
            HostFrame::Sync {
                image: "my-app".to_string(),
                tag: "dev".to_string(),
                digest: "sha256:fresh".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn notify_records_the_new_digest_into_desired_state() {
        let (_url, server) = spawn_server(DesiredState::default()).await;
        let event = TagEvent {
            image: "my-app:dev".to_string(),
            image_id: Some("sha256:fresh".to_string()),
        };
        server.notify(&event).await.unwrap();
        // A subsequent reconcile compares against the just-pushed digest.
        let stale = server
            .desired
            .lock()
            .unwrap()
            .reconcile(&hello("sha256:old"));
        assert_eq!(stale.len(), 1, "notify must update the desired digest");
        let current = server
            .desired
            .lock()
            .unwrap()
            .reconcile(&hello("sha256:fresh"));
        assert!(
            current.is_empty(),
            "a device on the just-pushed digest needs no reconcile"
        );
    }

    // ---- production TLS: the control WS runs over the pinned-CA leaf (D8/D9) ----

    /// Spawn a control server over TLS with a fresh session's leaf-backed server
    /// config; return its `wss://` base URL, the minted session (whose CA cert a
    /// client pins and whose read/control token it presents), and the handle.
    async fn spawn_tls_server(
        desired: DesiredState,
    ) -> (
        String,
        crate::utils::container_dev::tls::DevSession,
        Arc<ControlServer>,
    ) {
        let session = crate::utils::container_dev::tls::DevSession::mint("dev-runtime")
            .expect("session mints");
        let server = ControlServer::new(session.read_token.clone(), desired, HelloArchBook::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(session.tls.server_config());
        let serve = Arc::clone(&server);
        tokio::spawn(async move { serve.serve_tls(listener, acceptor).await });
        (format!("wss://{addr}/"), session, server)
    }

    /// A `tokio_tungstenite` TLS connector that trusts ONLY `ca_cert_pem`, so it
    /// validates the leaf's `127.0.0.1` IP SAN and rejects any other chain —
    /// the same pinned-CA discipline the bulk listener's client uses.
    fn pinned_ca_connector(ca_cert_pem: &str) -> tokio_tungstenite::Connector {
        use base64::Engine as _;
        // Decode the single PEM cert body into DER without an extra dependency.
        let body: String = ca_cert_pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .expect("session CA PEM base64 decodes");
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .expect("the session CA cert is a valid trust anchor");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tokio_tungstenite::Connector::Rustls(Arc::new(config))
    }

    #[tokio::test]
    async fn a_pinned_ca_tls_upgrade_succeeds_and_reconciles_a_stale_hello() {
        let desired = DesiredState::derive_from_watched_tags([(
            "my-app".to_string(),
            "dev".to_string(),
            "sha256:new".to_string(),
        )]);
        let (url, session, _server) = spawn_tls_server(desired).await;

        // A client that pins ONLY the session CA and presents the Bearer
        // read/control token: the wss upgrade must succeed over TLS.
        let connector = pinned_ca_connector(session.tls.ca_cert_pem());
        let request = authed_request(&url, session.read_token.secret());
        let (mut ws, resp) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
                .await
                .expect("a pinned-CA wss upgrade with the read/control token must succeed");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

        // A hello reporting a stale running_digest reconciles to the desired one.
        ws.send(Message::Text(
            serde_json::to_string(&DeviceFrame::Hello(hello("sha256:old")))
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();

        let msg = ws.next().await.expect("a reconcile sync over TLS").unwrap();
        let frame: HostFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(
            frame,
            HostFrame::Sync {
                image: "my-app".to_string(),
                tag: "dev".to_string(),
                digest: "sha256:new".to_string(),
            },
            "a stale hello over the pinned-CA TLS control WS must reconcile to the desired digest"
        );
    }

    #[tokio::test]
    async fn a_client_that_does_not_trust_the_session_ca_fails_the_tls_handshake() {
        let (url, _session, _server) = spawn_tls_server(DesiredState::default()).await;

        // Pin a DIFFERENT session's CA: it did not sign the server leaf, so the
        // TLS handshake must fail before any WebSocket upgrade is attempted.
        let other = crate::utils::container_dev::tls::DevSession::mint("other-runtime")
            .expect("a second session mints");
        let connector = pinned_ca_connector(other.tls.ca_cert_pem());
        let result = tokio_tungstenite::connect_async_tls_with_config(
            url.into_client_request().unwrap(),
            None,
            false,
            Some(connector),
        )
        .await;
        assert!(
            result.is_err(),
            "a client that does not trust the session CA must fail the TLS handshake"
        );
    }
}
