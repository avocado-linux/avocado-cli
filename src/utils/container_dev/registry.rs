//! OCI Distribution read handlers for the Container Dev Mode registry.
//!
//! These handlers implement the read half of the OCI Distribution spec that a
//! device engine exercises on a pull:
//!
//! - `GET /v2/` — the API version check.
//! - `GET|HEAD /v2/<name>/manifests/<reference>` — a manifest by tag or by
//!   digest, including a multi-arch image index.
//! - `GET|HEAD /v2/<name>/blobs/<digest>` — a blob, honoring a `Range:`
//!   request with a `206 Partial Content` response.
//!
//! Content is read from the per-project [`BlobStore`] built in task 3.1; this
//! module never re-implements storage. The read routes are gated by the
//! per-session Bearer read/control token (task 3.4) via [`read_router`] and
//! bound onto the dedicated bulk read listener by task 3.7.
//!
//! Task 3.3 adds the write half — blob upload (`POST`/`PATCH`/`PUT
//! .../blobs/uploads/...`), manifest `PUT`, and blob `HEAD` dedup — assembled
//! into a SEPARATE [`write_router`] gated by the host-only Basic write token
//! ([`super::auth`]). Those write routes live on a DISTINCT write listener
//! (design D9/H-1), bound by tasks 3.6/3.7; a device is only ever handed the
//! bulk-listener endpoint, so it cannot reach a write route on any topology.
//! The TLS/listener sockets (tasks 3.6/3.7) remain out of scope here.
//!
//! HEAD requests are served by the same handler as GET: axum routes HEAD to the
//! GET handler and strips the response body while preserving the headers, so a
//! HEAD carries the resource's `Content-Length` and `Docker-Content-Digest`
//! with an empty body.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::StreamExt as _;
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use super::auth::{require_basic_write, require_bearer_read, ReadToken, WriteToken};
use super::store::{BlobStore, BlobUpload, StoreError};

/// Non-standard OCI response header carrying the content digest of the served
/// manifest or blob.
const DOCKER_CONTENT_DIGEST: &str = "docker-content-digest";

/// How long an upload session may sit untouched before it is evicted.
///
/// Bounds how long an abandoned push can hold a staging file open. An upload that
/// is never finalized - an interrupted push, a killed `docker` - leaves a session
/// in the map with a `NamedTempFile` behind it; eviction drops the session, which
/// unlinks the file.
///
/// What "untouched" means, precisely: `touched` is refreshed on every `PATCH` and
/// on the finalizing `PUT`. Both handlers take the session OUT of the map for the
/// duration of the transfer, so `evict_expired` cannot see a session that is
/// actively being streamed into at all - a slow transfer is not evictable, however
/// long it runs, and the only sessions the sweep can reach are ones no request is
/// touching.
///
/// This replaces an earlier comment describing the buffered implementation, whose
/// premises this path no longer has: there is no `Bytes` extractor buffering a
/// chunk before the handler runs, no in-flight session visible to the sweep, and
/// no request-size limit to derive a throughput bound from. The resource at risk
/// is disk, not memory - see [`crate::utils::container_dev::store::MAX_BLOB_BYTES`]
/// for the ceiling that bounds it.
const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(600);

/// Upper bound on a manifest body.
///
/// Blobs are streamed to disk and need no limit, but a manifest is parsed as a
/// whole document, so this one path still reads into memory - under a cap
/// chosen to be far above any real manifest and far below anything that
/// threatens the host.
const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;

/// Default media type used when a stored manifest omits its `mediaType` field.
const DEFAULT_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Shared state for the registry handlers: the backing content-addressed store.
#[derive(Clone)]
pub struct RegistryState {
    store: Arc<BlobStore>,
}

impl RegistryState {
    /// Build registry state over an existing store.
    pub fn new(store: Arc<BlobStore>) -> Self {
        Self { store }
    }
}

/// Build the ungated OCI read route assembly over `store`.
///
/// These are the read handlers only — `GET /v2/`, manifest reads, and blob
/// reads (GET + HEAD). It is a composition primitive: [`read_router`] wraps it
/// with the Bearer read/control gate. The read-semantics tests exercise this
/// assembly directly so they test handler behavior without auth noise.
fn read_routes(store: Arc<BlobStore>) -> Router {
    Router::new()
        .route("/v2/", get(base))
        // A single wildcard route captures `<name>/manifests/<reference>` and
        // `<name>/blobs/<digest>`; `<name>` may itself contain `/`, so it
        // cannot be a fixed path segment. The suffix is dispatched by hand.
        .route("/v2/{*rest}", get(read))
        .with_state(RegistryState::new(store))
}

/// Build the device-facing OCI read router over `store`, gated by the
/// per-session Bearer `read_token` (task 3.4).
///
/// Every read route sits behind [`require_bearer_read`] — the SAME validator
/// the control-WS upgrade (task 5.1) authorizes through (G-5) — so an
/// unauthenticated pull, or one presenting the Basic write token, is refused
/// with a bare `Bearer` challenge before any handler runs (M-2). This is the
/// only read entry point a device is handed; it is bound onto the dedicated
/// bulk read listener in task 3.7.
pub fn read_router(store: Arc<BlobStore>, read_token: ReadToken) -> Router {
    read_routes(store).layer(middleware::from_fn_with_state(
        read_token,
        require_bearer_read,
    ))
}

/// A TLS-terminating [`axum::serve::Listener`] over a bound [`TcpListener`].
///
/// Every accepted TCP connection is handshaked with the per-project leaf
/// (task 3.6) before the OCI read router sees a byte, so the dedicated bulk
/// listener speaks only TLS. The axum `Listener` contract forbids surfacing an
/// accept error, so a failed TCP accept or TLS handshake is dropped and the
/// loop continues; a persistent TCP accept error backs off briefly to avoid a
/// busy-spin.
struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(pair) => pair,
                Err(_) => {
                    // Transient accept errors (e.g. fd exhaustion) must not be
                    // surfaced; back off so we do not busy-spin on a persistent
                    // one, then retry.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            // A handshake failure is a per-connection concern (a client that
            // does not trust the CA, or a probe); drop it and keep serving.
            if let Ok(tls) = self.acceptor.accept(stream).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// A bound, running bulk read listener: the dedicated TLS socket that serves the
/// OCI read router (task 3.2) gated by the Bearer read/control token (task 3.4).
///
/// This is the bulk-read leg of the three-listener model (design D9/H-1). A
/// listener's identity IS a socket, so bulk pulls live on their OWN socket,
/// separate from the write listener (task 3.3, [`write_router`]) and the control
/// WebSocket (task 5.1). Bulk transfers therefore never share the control WS
/// byte stream: a blob GET is an ordinary HTTP request on this dedicated TLS
/// socket, so a large pull can never head-of-line-block a control frame. A
/// device is only ever handed this listener's endpoint (task 5.2), so it cannot
/// reach the write listener on any topology.
pub struct BulkListener {
    local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl BulkListener {
    /// Bind the dedicated bulk read listener at `addr` and start serving the
    /// token-gated OCI read router over TLS with the session leaf.
    ///
    /// Pass a `0` port to let the OS choose one; [`local_addr`](Self::local_addr)
    /// then reports the concrete socket. The server runs on a spawned task that
    /// is aborted when the returned handle is dropped.
    pub async fn bind(
        addr: SocketAddr,
        store: Arc<BlobStore>,
        read_token: ReadToken,
        tls_config: Arc<ServerConfig>,
    ) -> io::Result<Self> {
        let tcp = TcpListener::bind(addr).await?;
        let local_addr = tcp.local_addr()?;
        let listener = TlsListener {
            tcp,
            acceptor: TlsAcceptor::from(tls_config),
        };
        let router = read_router(store, read_token);
        let task = tokio::spawn(async move {
            // `axum::serve` only returns on shutdown; the dev session drops the
            // handle (aborting this task) when the registry is torn down.
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { local_addr, task })
    }

    /// The socket this bulk listener is bound to — its listener identity
    /// (design H-1), distinct from the write listener's and the control WS's.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for BulkListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One in-flight chunked upload: the on-disk staging handle plus when it last
/// grew.
struct UploadSession {
    upload: BlobUpload,
    touched: Instant,
}

impl UploadSession {
    fn new(upload: BlobUpload) -> Self {
        Self {
            upload,
            touched: Instant::now(),
        }
    }
}

/// In-flight chunked-upload sessions, keyed by upload UUID.
///
/// The OCI blob-upload protocol is stateful: `POST` opens a session, `PATCH`
/// appends chunks, and `PUT` finalizes with the expected digest. A session holds a
/// [`BlobUpload`] staging the bytes on disk and hashing them incrementally, so no
/// layer is ever held whole in memory.
///
/// Nothing in the protocol obliges a client to finish what it starts: a `POST`
/// followed by `PATCH`es and no `PUT` - an interrupted push, a killed `docker` -
/// abandons its staging file here. [`evict_expired`] reclaims those, and
/// `BlobStore::sweep_uploads` catches the ones whose process died before any
/// eviction could run.
#[derive(Default)]
struct UploadSessions {
    inner: Mutex<HashMap<String, UploadSession>>,
}

/// Drop sessions untouched for longer than [`UPLOAD_SESSION_TTL`] as of `now`.
///
/// Called when a new session opens, which is both the moment a fresh buffer is
/// about to be allocated and the only point an abandoned one can be noticed - no
/// client ever tells us it gave up.
///
/// `now` is a parameter rather than a call to `Instant::now()` inside so a test
/// can place a session at a chosen distance from the TTL boundary. Reading the
/// clock internally left the only available test a sweep milliseconds after the
/// POST, which passes whether or not the mechanism works at all.
fn evict_expired(sessions: &mut HashMap<String, UploadSession>, now: Instant) {
    sessions.retain(|_uuid, session| {
        now.saturating_duration_since(session.touched) < UPLOAD_SESSION_TTL
    });
}

/// Shared state for the write handlers: the backing store plus upload sessions.
#[derive(Clone)]
struct WriteState {
    store: Arc<BlobStore>,
    uploads: Arc<UploadSessions>,
}

/// Build the OCI WRITE router over `store`, gated by the host-only Basic
/// `write_token`.
///
/// The router serves blob upload (`POST`/`PATCH`/`PUT .../blobs/uploads/...`),
/// manifest `PUT`, blob `HEAD` dedup, and the `GET /v2/` ping — every route
/// behind [`require_basic_write`], so an anonymous request (including the ping)
/// receives a `401` with a Basic challenge. This router is bound onto the
/// DISTINCT write listener (design D9); it is never merged onto the bulk read
/// listener.
pub fn write_router(store: Arc<BlobStore>, write_token: WriteToken) -> Router {
    write_router_with_uploads(store, write_token, Arc::new(UploadSessions::default()))
}

/// [`write_router`], but over a caller-supplied session map.
///
/// Exists so a test can hold the same `Arc` the handlers mutate and place a
/// session at a chosen age. Without it the TTL is only reachable through a real
/// 10-minute wait, which is why the first attempt at a TTL test asserted nothing.
fn write_router_with_uploads(
    store: Arc<BlobStore>,
    write_token: WriteToken,
    uploads: Arc<UploadSessions>,
) -> Router {
    let state = WriteState { store, uploads };
    Router::new()
        .route("/v2/", get(base))
        .route(
            "/v2/{*rest}",
            post(post_route)
                .patch(patch_route)
                .put(put_route)
                .head(head_route),
        )
        // The auth layer wraps the whole router, so it runs before routing: an
        // unauthenticated request to any path (or an unrouted method) is
        // rejected with the Basic challenge before a handler is reached.
        .layer(middleware::from_fn_with_state(
            write_token,
            require_basic_write,
        ))
        // No DefaultBodyLimit layer at all. It would be inert: that limit is
        // consumed by the `Bytes`/`String` extractors, and every write handler
        // now takes `Body` and streams it, so the layer would gate nothing while
        // reading as though it did. Blob bodies never exist whole in memory, and
        // the one path that does buffer - manifest PUT - applies
        // MAX_MANIFEST_BYTES explicitly where the read happens.
        .with_state(state)
}

/// Serve the write router over TLS with the per-project session leaf, spawned on
/// its own task (aborted when the returned handle is dropped).
///
/// Used for the VM push path only: a QEMU-SLIRP guest reaches the loopback write
/// listener through the `10.0.2.2` host alias, which is NOT inside docker's
/// built-in `127.0.0.0/8` insecure exemption (design A2). The guest daemon is
/// therefore configured for HTTPS via a delivered `certs.d/<registry>/ca.crt`
/// (design H4), so the listener must terminate the same leaf TLS the bulk and
/// control listeners do. The native loopback path keeps plain HTTP under docker's
/// exemption and does not call this.
pub fn serve_write_router_tls(
    tcp: TcpListener,
    tls_config: Arc<ServerConfig>,
    store: Arc<BlobStore>,
    write_token: WriteToken,
) -> JoinHandle<()> {
    let listener = TlsListener {
        tcp,
        acceptor: TlsAcceptor::from(tls_config),
    };
    let router = write_router(store, write_token);
    tokio::spawn(async move {
        // `axum::serve` only returns on shutdown; the session aborts this task
        // via the returned handle when the write listener is torn down.
        let _ = axum::serve(listener, router).await;
    })
}

/// `POST /v2/<name>/blobs/uploads/[?digest=<digest>]` — open a chunked upload,
/// or complete a monolithic upload when a `digest` query is present.
async fn post_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    let Some(name) = rest
        .strip_suffix("/blobs/uploads/")
        .or_else(|| rest.strip_suffix("/blobs/uploads"))
    else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "unsupported write path",
        );
    };
    let name = name.to_string();

    if let Some(digest) = q.get("digest") {
        // Monolithic upload: the whole blob arrives with the POST. Still
        // streamed - "monolithic" describes the protocol, not how much of it we
        // are willing to hold at once.
        let mut upload = match state.store.begin_blob_upload() {
            Ok(upload) => upload,
            Err(e) => return store_error(&e),
        };
        if let Err(resp) = stream_into(body, &mut upload).await {
            return resp;
        }
        return finish_upload(&name, digest, upload);
    }

    let uuid = Uuid::new_v4().to_string();
    let upload = match state.store.begin_blob_upload() {
        Ok(upload) => upload,
        Err(e) => return store_error(&e),
    };
    let mut sessions = state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned");
    // Reclaim sessions from pushes that opened one and never finalized it,
    // before starting another alongside them. Their temp files go with them.
    evict_expired(&mut sessions, Instant::now());
    sessions.insert(uuid.clone(), UploadSession::new(upload));
    drop(sessions);
    upload_accepted(&name, &uuid, 0)
}

/// Drain `body` into `upload`, mapping a transport error to an OCI response.
async fn stream_into(body: Body, upload: &mut BlobUpload) -> Result<(), Response> {
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return Err(oci_error(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    "upload stream ended early",
                ))
            }
        };
        if let Err(e) = upload.append(&chunk) {
            return Err(store_error(&e));
        }
    }
    Ok(())
}

/// Verify and store a completed upload, returning the OCI response.
fn finish_upload(name: &str, digest: &str, upload: BlobUpload) -> Response {
    match upload.finish(digest) {
        Ok(true) => blob_created(name, digest),
        Ok(false) => oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "uploaded content does not match the supplied digest",
        ),
        Err(e) => store_error(&e),
    }
}

/// `PATCH /v2/<name>/blobs/uploads/<uuid>` — append a chunk to a session.
async fn patch_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    body: Body,
) -> Response {
    let Some((name, uuid)) = split_upload(&rest) else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "unsupported write path",
        );
    };
    let (name, uuid) = (name.to_string(), uuid.to_string());

    // Take the session OUT of the map for the duration of the transfer. The
    // mutex cannot be held across the await, and a session being streamed into
    // is not a session a concurrent sweep should be able to reclaim - removing
    // it makes both true at once.
    let Some(mut session) = state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned")
        .remove(&uuid)
    else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown",
        );
    };

    let start = session.upload.written();
    let streamed = stream_into(body, &mut session.upload).await;
    // Re-insert on BOTH paths. Taking the session out of the map is what keeps a
    // concurrent sweep from reclaiming it mid-transfer, but returning early on an
    // error would drop the `BlobUpload` here - unlinking the staging file and
    // every chunk already accepted. The buffered implementation got resumability
    // for free: the `Bytes` extractor rejected a truncated body before the
    // handler ran, so the session and its bytes survived and `Range` told the
    // client where to continue. Streaming has to restore that explicitly, or a
    // dropped connection on chunk 6 of 8 restarts the layer from byte 0.
    let end = session.upload.written();
    session.touched = Instant::now();
    state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned")
        .insert(uuid.clone(), session);
    if let Err(resp) = streamed {
        return resp;
    }
    upload_range_accepted(&name, &uuid, start, end)
}

/// `PUT` on the write listener: finalize a blob upload
/// (`.../blobs/uploads/<uuid>?digest=`) or store a manifest
/// (`.../manifests/<reference>`).
async fn put_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    if let Some((name, reference)) = rest.split_once("/manifests/") {
        // Manifests are small JSON documents and are parsed as a whole, so this
        // one path still buffers - under an explicit cap, not an unbounded read.
        let bytes = match axum::body::to_bytes(body, MAX_MANIFEST_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return oci_error(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_INVALID",
                    "manifest exceeds the maximum accepted size",
                )
            }
        };
        return put_manifest(&state, name, reference, &bytes);
    }
    if let Some((name, uuid)) = split_upload(&rest) {
        let (name, uuid) = (name.to_string(), uuid.to_string());
        let Some(digest) = q.get("digest").map(String::as_str) else {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "digest query parameter required to finalize an upload",
            );
        };
        let Some(mut session) = state
            .uploads
            .inner
            .lock()
            .expect("upload sessions mutex is not poisoned")
            .remove(&uuid)
        else {
            return oci_error(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload session unknown",
            );
        };
        // The PUT may carry a final chunk of its own. On a truncated one, put the
        // session back rather than discarding every previously accepted chunk -
        // the client can retry the finalize against the same Location.
        if let Err(resp) = stream_into(body, &mut session.upload).await {
            session.touched = Instant::now();
            state
                .uploads
                .inner
                .lock()
                .expect("upload sessions mutex is not poisoned")
                .insert(uuid.clone(), session);
            return resp;
        }
        return finish_upload(&name, digest, session.upload);
    }
    oci_error(
        StatusCode::NOT_FOUND,
        "UNSUPPORTED",
        "unsupported write path",
    )
}

/// `HEAD /v2/<name>/blobs/<digest>` — the push-side dedup probe: `200` when the
/// blob already exists so the engine skips re-uploading it, else `404`.
async fn head_route(State(state): State<WriteState>, Path(rest): Path<String>) -> Response {
    let Some((_name, digest)) = rest.split_once("/blobs/") else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "unsupported write path",
        );
    };
    // `blob_size` stats the entry instead of reading it: the probe reports only
    // a length, and an engine HEADs every layer before pushing, so reading each
    // existing layer into memory to discard it would put the whole image on the
    // heap just to answer "do you already have this?".
    match state.store.blob_size(digest) {
        Ok(Some(len)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, len.to_string())
            .header(DOCKER_CONTENT_DIGEST, digest)
            .body(Body::empty())
            .expect("blob-head response is always valid"),
        Ok(None) => blob_unknown(),
        Err(e) => store_error(&e),
    }
}

/// `PUT /v2/<name>/manifests/<reference>` — store a manifest and, when
/// `reference` is a tag (not a digest), point that tag at it.
fn put_manifest(state: &WriteState, name: &str, reference: &str, body: &[u8]) -> Response {
    let digest = compute_digest(body);
    if let Err(e) = state.store.write_blob(&digest, body) {
        return store_error(&e);
    }
    if !looks_like_digest(reference) {
        if let Err(e) = state.store.set_tag(reference, &digest) {
            return store_error(&e);
        }
    }
    manifest_created(name, reference, &digest)
}

/// Split `<name>/blobs/uploads/<uuid>` into `(name, uuid)`.
fn split_upload(rest: &str) -> Option<(&str, &str)> {
    let (name, uuid) = rest.split_once("/blobs/uploads/")?;
    if name.is_empty() || uuid.is_empty() || uuid.contains('/') {
        return None;
    }
    Some((name, uuid))
}

/// Compute the OCI digest (`sha256:<hex>`) of `bytes`.
fn compute_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// `202 Accepted` opening a chunked upload session.
fn upload_accepted(name: &str, uuid: &str, offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{uuid}"))
        .header("docker-upload-uuid", uuid)
        .header(header::RANGE, format!("0-{offset}"))
        .body(Body::empty())
        .expect("upload-accepted response is always valid")
}

/// `202 Accepted` acknowledging an appended chunk, reporting the new byte range.
fn upload_range_accepted(name: &str, uuid: &str, start: u64, end: u64) -> Response {
    // An empty session reports `0-0`; otherwise the last written byte index.
    let last = end.saturating_sub(1).max(start);
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{uuid}"))
        .header("docker-upload-uuid", uuid)
        .header(header::RANGE, format!("0-{last}"))
        .body(Body::empty())
        .expect("upload-range response is always valid")
}

/// `201 Created` for a completed blob upload.
fn blob_created(name: &str, digest: &str) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, format!("/v2/{name}/blobs/{digest}"))
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::empty())
        .expect("blob-created response is always valid")
}

/// `201 Created` for a stored manifest.
fn manifest_created(name: &str, reference: &str, digest: &str) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(
            header::LOCATION,
            format!("/v2/{name}/manifests/{reference}"),
        )
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::empty())
        .expect("manifest-created response is always valid")
}

/// Map a [`StoreError`] to an OCI error response.
fn store_error(err: &StoreError) -> Response {
    match err {
        StoreError::InvalidDigest(_) => {
            oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest")
        }
        StoreError::InvalidTag(_) => {
            oci_error(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid tag")
        }
        // A real OCI error the engine can act on, not a bare axum rejection:
        // 413 with BLOB_UPLOAD_INVALID tells the client the layer is too big
        // rather than leaving it to guess from a closed connection.
        StoreError::BlobTooLarge { .. } => oci_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BLOB_UPLOAD_INVALID",
            "blob exceeds the registry's size ceiling",
        ),
        StoreError::NoHome | StoreError::Io(_) | StoreError::PruneWhilePulling => oci_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "registry storage error",
        ),
    }
}

/// `GET /v2/` — advertise OCI Distribution v2 support.
async fn base() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("docker-distribution-api-version", "registry/2.0"),
            (header::CONTENT_TYPE.as_str(), "application/json"),
        ],
        "{}",
    )
}

/// Dispatch a `/v2/<rest>` read to the manifest or blob handler.
async fn read(
    State(state): State<RegistryState>,
    headers: HeaderMap,
    Path(rest): Path<String>,
) -> Response {
    if let Some((_name, reference)) = rest.split_once("/manifests/") {
        serve_manifest(&state, reference)
    } else if let Some((_name, digest)) = rest.split_once("/blobs/") {
        serve_blob(&state, &headers, digest).await
    } else {
        oci_error(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            "unsupported registry path",
        )
    }
}

/// Serve a manifest identified by `reference`, which is either a digest
/// (`<algorithm>:<hex>`) or a tag that resolves to a manifest digest.
fn serve_manifest(state: &RegistryState, reference: &str) -> Response {
    let digest = if looks_like_digest(reference) {
        reference.to_string()
    } else {
        match state.store.resolve_tag(reference) {
            Ok(Some(d)) => d,
            _ => return manifest_unknown(),
        }
    };

    let bytes = match state.store.read_blob(&digest) {
        Ok(Some(b)) => b,
        _ => return manifest_unknown(),
    };

    let media_type = manifest_media_type(&bytes);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, media_type)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::from(bytes))
        .expect("static manifest response is always valid")
}

/// Serve a blob by `digest`, honoring a single `Range:` request.
///
/// Streamed off disk rather than read whole. Uploads land on disk without ever
/// existing complete in memory, so the store can hold a layer larger than host
/// RAM - and a read that sized one allocation by the blob turned a single
/// oversized push into an OOM on every later pull, taking every listener and the
/// session's TLS material with it. Reading incrementally makes the served size
/// independent of available memory, and a ranged read serves its window from the
/// same handle instead of copying the slice back out of a full-blob buffer.
async fn serve_blob(state: &RegistryState, headers: &HeaderMap, digest: &str) -> Response {
    let (file, total) = match state.store.open_blob(digest) {
        Ok(Some(open)) => open,
        _ => return blob_unknown(),
    };
    let mut file = tokio::fs::File::from_std(file);

    if let Some(range) = headers.get(header::RANGE) {
        return match parse_range(range, total) {
            Some((start, end)) => {
                if tokio::io::AsyncSeekExt::seek(&mut file, io::SeekFrom::Start(start))
                    .await
                    .is_err()
                {
                    return blob_unknown();
                }
                // `end` is inclusive, matching Content-Range.
                let window = tokio::io::AsyncReadExt::take(file, end - start + 1);
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    )
                    .header(DOCKER_CONTENT_DIGEST, digest)
                    .body(Body::from_stream(ReaderStream::new(window)))
                    .expect("range response is always valid")
            }
            None => Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                .body(Body::empty())
                .expect("unsatisfiable-range response is always valid"),
        };
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::from_stream(ReaderStream::new(file)))
        .expect("full-blob response is always valid")
}

/// Read the `mediaType` field from a stored manifest, falling back to the
/// default OCI image-manifest type when it is absent or the body is not JSON.
///
/// A multi-arch image index carries its own index `mediaType`
/// (`application/vnd.oci.image.index.v1+json` or the Docker manifest-list type),
/// so echoing it back is what lets the engine recognize an index versus a
/// single-platform manifest.
fn manifest_media_type(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            v.get("mediaType")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_MANIFEST_MEDIA_TYPE.to_string())
}

/// Whether `reference` is shaped like an OCI digest (`<algorithm>:<hex>`),
/// distinguishing a by-digest reference from a tag.
fn looks_like_digest(reference: &str) -> bool {
    match reference.split_once(':') {
        Some((algorithm, hex)) => {
            !algorithm.is_empty()
                && !hex.is_empty()
                && algorithm.chars().all(|c| c.is_ascii_alphanumeric())
                && hex.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Parse a single-range `Range: bytes=...` header against a resource of
/// `total` bytes, returning an inclusive `(start, end)` clamped to bounds, or
/// `None` when the range is malformed, multi-range, or unsatisfiable.
fn parse_range(value: &HeaderValue, total: u64) -> Option<(u64, u64)> {
    let spec = value.to_str().ok()?.strip_prefix("bytes=")?;
    // Multi-range is not supported; treat it as unsatisfiable.
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;

    if start_s.is_empty() {
        // Suffix range: the last `n` bytes.
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 || total == 0 {
            return None;
        }
        let len = suffix.min(total);
        return Some((total - len, total - 1));
    }

    let start: u64 = start_s.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn manifest_unknown() -> Response {
    oci_error(
        StatusCode::NOT_FOUND,
        "MANIFEST_UNKNOWN",
        "manifest unknown",
    )
}

fn blob_unknown() -> Response {
    oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob unknown")
}

/// Build an OCI error response (`{"errors":[{"code","message"}]}`).
fn oci_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "errors": [{ "code": code, "message": message }] }).to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("oci error response is always valid")
}

#[cfg(test)]
mod read {
    use super::*;
    use crate::utils::container_dev::store::BlobStore;
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    /// Compute the OCI digest (`sha256:<hex>`) of `bytes`.
    fn digest_of(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    /// Start the ungated read route assembly over a fresh per-project store and
    /// return the base URL plus a handle keeping the store's temp dir alive.
    ///
    /// These tests exercise read semantics (ranges, media types, dedup); the
    /// Bearer read/control gate on the public [`read_router`] is covered by the
    /// `container_dev::auth` tests, so the assembly is served ungated here.
    async fn spawn() -> (String, Arc<BlobStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let app = read_routes(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), store, dir)
    }

    /// A minimal single-platform image manifest.
    fn image_manifest() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "size": 7,
            },
            "layers": [],
        }))
        .unwrap()
    }

    /// A multi-arch image index referencing per-platform manifests.
    fn image_index() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    "size": 100,
                    "platform": { "architecture": "amd64", "os": "linux" },
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                    "size": 100,
                    "platform": { "architecture": "arm64", "os": "linux" },
                },
            ],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn v2_base_returns_200_with_api_version() {
        let (base, _store, _dir) = spawn().await;
        let resp = reqwest::get(format!("{base}/v2/")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("docker-distribution-api-version")
                .and_then(|h| h.to_str().ok()),
            Some("registry/2.0"),
        );
    }

    #[tokio::test]
    async fn manifest_by_tag_returns_stored_manifest() {
        let (base, store, _dir) = spawn().await;
        let manifest = image_manifest();
        let digest = digest_of(&manifest);
        store.write_blob(&digest, &manifest).unwrap();
        store.set_tag("dev", &digest).unwrap();

        let resp = reqwest::get(format!("{base}/v2/my-app/manifests/dev"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("application/vnd.oci.image.manifest.v1+json"),
        );
        assert_eq!(
            resp.headers()
                .get("docker-content-digest")
                .and_then(|h| h.to_str().ok()),
            Some(digest.as_str()),
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), manifest.as_slice());
    }

    #[tokio::test]
    async fn manifest_by_digest_returns_stored_manifest() {
        let (base, store, _dir) = spawn().await;
        let manifest = image_manifest();
        let digest = digest_of(&manifest);
        store.write_blob(&digest, &manifest).unwrap();

        // No tag set: fetching by digest must still resolve.
        let resp = reqwest::get(format!("{base}/v2/my-app/manifests/{digest}"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("docker-content-digest")
                .and_then(|h| h.to_str().ok()),
            Some(digest.as_str()),
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), manifest.as_slice());
    }

    #[tokio::test]
    async fn multi_arch_index_is_served_with_index_media_type() {
        let (base, store, _dir) = spawn().await;
        let index = image_index();
        let digest = digest_of(&index);
        store.write_blob(&digest, &index).unwrap();
        store.set_tag("multi", &digest).unwrap();

        let resp = reqwest::get(format!("{base}/v2/my-app/manifests/multi"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // The index media type — not a single-platform manifest type — is what
        // lets the engine recognize a multi-arch index and pick a platform.
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("application/vnd.oci.image.index.v1+json"),
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), index.as_slice());
    }

    #[tokio::test]
    async fn unknown_manifest_returns_404() {
        let (base, _store, _dir) = spawn().await;
        let resp = reqwest::get(format!("{base}/v2/my-app/manifests/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn full_blob_get_returns_whole_body() {
        let (base, store, _dir) = spawn().await;
        let blob: Vec<u8> = (0u8..=255).collect();
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = reqwest::get(format!("{base}/v2/my-app/blobs/{digest}"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("docker-content-digest")
                .and_then(|h| h.to_str().ok()),
            Some(digest.as_str()),
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), blob.as_slice());
    }

    #[tokio::test]
    async fn ranged_blob_get_returns_206_with_only_the_requested_bytes() {
        let (base, store, _dir) = spawn().await;
        let blob: Vec<u8> = (0u8..=255).collect();
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = reqwest::Client::new()
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .header(reqwest::header::RANGE, "bytes=10-19")
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            206,
            "a Range request must return 206"
        );
        assert_eq!(
            resp.headers()
                .get("content-range")
                .and_then(|h| h.to_str().ok()),
            Some("bytes 10-19/256"),
        );
        let body = resp.bytes().await.unwrap();
        // Exactly the requested slice, not the whole blob.
        assert_eq!(body.len(), 10);
        assert_eq!(body.as_ref(), &blob[10..=19]);
    }

    #[tokio::test]
    async fn suffix_range_returns_last_bytes() {
        let (base, store, _dir) = spawn().await;
        let blob: Vec<u8> = (0u8..=99).collect();
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = reqwest::Client::new()
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .header(reqwest::header::RANGE, "bytes=-5")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 206);
        assert_eq!(
            resp.headers()
                .get("content-range")
                .and_then(|h| h.to_str().ok()),
            Some("bytes 95-99/100"),
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), &blob[95..=99]);
    }

    #[tokio::test]
    async fn unsatisfiable_range_returns_416() {
        let (base, store, _dir) = spawn().await;
        let blob: Vec<u8> = vec![1, 2, 3, 4];
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = reqwest::Client::new()
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .header(reqwest::header::RANGE, "bytes=100-200")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 416);
    }

    /// Collect a response body, returning how many data frames it arrived in.
    ///
    /// Frame count is the discriminator these two tests need: a `Body` built from
    /// one `Vec<u8>` carries exactly one data frame however large it is, while a
    /// body streamed off disk carries one per read. Driven through `oneshot`
    /// rather than a real request on purpose - over TCP the chunk boundaries a
    /// client observes come from coalescing, not from how the handler built the
    /// body, so counting socket reads would prove nothing about buffering.
    async fn collect_frames(body: Body) -> (usize, Vec<u8>) {
        use futures_util::StreamExt as _;

        let mut frames = 0usize;
        let mut bytes: Vec<u8> = Vec::new();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("body stream must not error");
            frames += 1;
            bytes.extend_from_slice(&chunk);
        }
        (frames, bytes)
    }

    /// A blob is read off disk incrementally, never sized into one allocation.
    ///
    /// The store accepts a layer larger than host RAM (the upload streams
    /// straight to disk), so a read path that buffers the whole object turns one
    /// oversized push into an OOM on every subsequent pull. Fails with `1 frame`
    /// if `serve_blob` returns to reading the blob whole.
    #[tokio::test]
    async fn a_large_blob_is_streamed_in_many_frames_not_one_allocation() {
        use tower::ServiceExt as _;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        // Comfortably more than one read, small enough to keep the test fast.
        let blob: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = read_routes(store)
            .oneshot(
                axum::http::Request::get(format!("/v2/my-app/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        let (frames, collected) = collect_frames(resp.into_body()).await;
        assert!(
            frames > 1,
            "a {}-byte blob must arrive in more than one frame; got {frames}, \
             so the whole blob was buffered into a single allocation",
            blob.len()
        );
        assert_eq!(collected, blob, "streaming must deliver the blob unchanged");
    }

    /// A ranged read streams the requested window instead of copying it.
    ///
    /// Slicing a buffered blob allocated the window a second time on top of the
    /// whole object, so this covers the doubling specifically rather than only
    /// the full-body path.
    #[tokio::test]
    async fn a_large_range_is_streamed_rather_than_copied() {
        use tower::ServiceExt as _;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let blob: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let digest = digest_of(&blob);
        store.write_blob(&digest, &blob).unwrap();

        let resp = read_routes(store)
            .oneshot(
                axum::http::Request::get(format!("/v2/my-app/blobs/{digest}"))
                    .header(header::RANGE, "bytes=1024-401023")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 206);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_RANGE)
                .and_then(|h| h.to_str().ok()),
            Some("bytes 1024-401023/524288"),
        );

        let (frames, collected) = collect_frames(resp.into_body()).await;
        assert!(
            frames > 1,
            "a 400000-byte range must arrive in more than one frame; got {frames}"
        );
        assert_eq!(
            collected,
            &blob[1024..=401023],
            "the range must be byte-exact"
        );
    }

    #[tokio::test]
    async fn head_manifest_returns_headers_without_body() {
        let (base, store, _dir) = spawn().await;
        let manifest = image_manifest();
        let digest = digest_of(&manifest);
        store.write_blob(&digest, &manifest).unwrap();
        store.set_tag("dev", &digest).unwrap();

        let resp = reqwest::Client::new()
            .head(format!("{base}/v2/my-app/manifests/dev"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("docker-content-digest")
                .and_then(|h| h.to_str().ok()),
            Some(digest.as_str()),
        );
        assert!(
            resp.bytes().await.unwrap().is_empty(),
            "HEAD carries no body"
        );
    }

    #[tokio::test]
    async fn unknown_blob_returns_404() {
        let (base, _store, _dir) = spawn().await;
        let missing = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let resp = reqwest::get(format!("{base}/v2/my-app/blobs/{missing}"))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }
}

#[cfg(test)]
mod write_auth {
    use super::*;
    use crate::utils::container_dev::auth::{WriteToken, WRITE_USERNAME};
    use crate::utils::container_dev::store::BlobStore;
    use tempfile::TempDir;

    const WRITE_TOKEN: &str = "write-token-secret";

    /// Start the WRITE router (gated by [`WRITE_TOKEN`]) over a fresh
    /// per-project store; return the base URL plus a handle keeping the store's
    /// temp dir alive.
    async fn spawn() -> (String, Arc<BlobStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let app = write_router(store.clone(), WriteToken::new(WRITE_TOKEN));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), store, dir)
    }

    /// A minimal single-platform image manifest.
    fn manifest() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "size": 7,
            },
            "layers": [],
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn valid_basic_write_token_stores_a_manifest() {
        let (base, store, _dir) = spawn().await;
        let body = manifest();
        let digest = compute_digest(&body);

        let resp = reqwest::Client::new()
            .put(format!("{base}/v2/my-app/manifests/dev"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(body.clone())
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            201,
            "a valid Basic write credential must be accepted on a write route"
        );
        // Observable side effect: the manifest is stored and the tag points at it.
        assert!(store.has_blob(&digest).unwrap());
        assert_eq!(
            store.resolve_tag("dev").unwrap().as_deref(),
            Some(digest.as_str())
        );
    }

    #[tokio::test]
    async fn a_blob_larger_than_the_default_body_limit_is_accepted() {
        // A real image layer exceeds axum's 2 MiB DefaultBodyLimit. The write
        // listener buffers the body as `Bytes`, so without lifting the cap every
        // `docker push` of a non-trivial image 413s ("Failed to buffer the request
        // body: length limit exceeded") and the push fails mid-stream. The store
        // persists blobs to disk, so a large upload must be accepted.
        let (base, store, _dir) = spawn().await;
        let blob = vec![0x5au8; 3 * 1024 * 1024]; // 3 MiB > the 2 MiB default
        let digest = compute_digest(&blob);

        let resp = reqwest::Client::new()
            .post(format!("{base}/v2/my-app/blobs/uploads/?digest={digest}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(blob.clone())
            .send()
            .await
            .unwrap();

        assert_ne!(
            resp.status().as_u16(),
            413,
            "a >2 MiB blob must not be rejected with 413 by the default body limit"
        );
        assert_eq!(
            resp.status().as_u16(),
            201,
            "a monolithic blob upload with a valid write credential must be created"
        );
        assert!(
            store.has_blob(&digest).unwrap(),
            "the oversized blob must be persisted to the on-disk store"
        );
    }

    #[tokio::test]
    async fn bearer_read_control_token_is_rejected_on_a_write_route() {
        let (base, store, _dir) = spawn().await;
        let body = manifest();
        let digest = compute_digest(&body);

        // The device-delivered read/control token is a Bearer value. Presenting
        // it (even with the same secret string) on a write route must be
        // refused — this closes the H-A compromised-device write class.
        let resp = reqwest::Client::new()
            .put(format!("{base}/v2/my-app/manifests/dev"))
            .bearer_auth(WRITE_TOKEN)
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            401,
            "the Bearer read/control token must not authorize a write"
        );
        assert!(
            !store.has_blob(&digest).unwrap(),
            "a rejected write must not persist any content"
        );
        assert_eq!(store.resolve_tag("dev").unwrap(), None);
    }

    #[tokio::test]
    async fn anonymous_write_is_rejected() {
        let (base, store, _dir) = spawn().await;
        let body = manifest();
        let digest = compute_digest(&body);

        let resp = reqwest::Client::new()
            .put(format!("{base}/v2/my-app/manifests/dev"))
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            401,
            "an anonymous write must be refused"
        );
        assert!(!store.has_blob(&digest).unwrap());
        assert_eq!(store.resolve_tag("dev").unwrap(), None);
    }

    #[tokio::test]
    async fn wrong_password_basic_credential_is_rejected() {
        let (base, store, _dir) = spawn().await;
        let body = manifest();
        let digest = compute_digest(&body);

        let resp = reqwest::Client::new()
            .put(format!("{base}/v2/my-app/manifests/dev"))
            .basic_auth(WRITE_USERNAME, Some("not-the-write-token"))
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            401,
            "a Basic credential with the wrong password must be refused"
        );
        assert!(!store.has_blob(&digest).unwrap());
    }

    #[tokio::test]
    async fn write_path_issues_a_basic_challenge_not_bearer() {
        let (base, _store, _dir) = spawn().await;

        // An anonymous request to the write listener must challenge with Basic;
        // a Bearer/token-endpoint challenge on the write path is a falsifier.
        let resp = reqwest::get(format!("{base}/v2/")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 401);
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            challenge.starts_with("basic"),
            "the write path must issue a Basic challenge, got {challenge:?}"
        );
        assert!(
            !challenge.contains("bearer"),
            "the write path must NOT issue a Bearer challenge"
        );
    }

    #[tokio::test]
    async fn valid_token_completes_a_monolithic_blob_upload() {
        let (base, store, _dir) = spawn().await;
        let blob = b"a-container-layer".to_vec();
        let digest = compute_digest(&blob);

        let resp = reqwest::Client::new()
            .post(format!("{base}/v2/my-app/blobs/uploads/?digest={digest}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(blob.clone())
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            201,
            "a monolithic blob upload must complete"
        );
        assert_eq!(
            store.read_blob(&digest).unwrap().as_deref(),
            Some(blob.as_slice()),
            "the uploaded blob bytes must be stored verbatim"
        );
    }

    #[tokio::test]
    async fn head_dedup_probe_is_gated_and_reports_presence() {
        let (base, store, _dir) = spawn().await;
        let blob = b"already-present".to_vec();
        let digest = compute_digest(&blob);
        store.write_blob(&digest, &blob).unwrap();

        // The dedup HEAD is a write-listener route, so it is auth-gated too.
        let anon = reqwest::Client::new()
            .head(format!("{base}/v2/my-app/blobs/{digest}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            anon.status().as_u16(),
            401,
            "an anonymous dedup probe must be refused"
        );

        let authed = reqwest::Client::new()
            .head(format!("{base}/v2/my-app/blobs/{digest}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(
            authed.status().as_u16(),
            200,
            "an authenticated dedup probe must report an existing blob present"
        );
        assert_eq!(
            authed
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some(blob.len().to_string().as_str()),
            "the probe must report the blob's real size, statted rather than read"
        );
    }

    // A multi-chunk layer pushes end to end and lands intact.
    //
    // What this does NOT prove, stated plainly so nobody reads it as more than
    // it is: the >2 GiB single-request case that motivated the streaming change
    // is not reachable in a test - allocating one is impractical, and toggling
    // `DefaultBodyLimit` cannot simulate it either, because that limit is
    // consumed by the `Bytes` extractor and these handlers now take `Body`.
    // The absence of a per-request ceiling is a property of the handler
    // signatures, not something an assertion here can demonstrate.
    //
    // What it does prove is the chunked path over the streaming handlers: six
    // PATCHes, a finalizing PUT, and the exact bytes in the store afterwards.
    // `an_upload_stages_to_disk_not_memory` covers the peak-memory half.
    #[tokio::test]
    async fn a_multi_chunk_layer_pushes_end_to_end() {
        let (base, store, _dir) = spawn().await;
        let client = reqwest::Client::new();

        let chunk = vec![0x5au8; 512 * 1024];
        let chunks = 6; // 3 MiB total
        let mut whole = Vec::new();
        for _ in 0..chunks {
            whole.extend_from_slice(&chunk);
        }
        let digest = compute_digest(&whole);

        let opened = client
            .post(format!("{base}/v2/my-app/blobs/uploads/"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(opened.status().as_u16(), 202);
        let location = opened
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        for i in 0..chunks {
            let patched = client
                .patch(format!("{base}{location}"))
                .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
                .body(chunk.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(
                patched.status().as_u16(),
                202,
                "chunk {i} must be accepted, not 413'd"
            );
        }

        let done = client
            .put(format!("{base}{location}?digest={digest}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(done.status().as_u16(), 201, "the layer must finalize");
        assert_eq!(
            store.blob_size(&digest).unwrap(),
            Some(whole.len() as u64),
            "the whole layer must have landed in the store"
        );
    }

    // The bytes must reach DISK as chunks arrive, not accumulate in memory.
    //
    // The previous version of this asserted `upload.written()` - a plain u64
    // counter incremented in `append` - and claimed that proved write-through. It
    // did not: rewriting `BlobUpload` to accumulate into a `Vec<u8>` and only
    // `write_all` inside `finish()` reintroduces exactly the whole-layer-in-memory
    // behaviour this round removed, and `written`/`hasher` update identically, so
    // the assertion passed unchanged.
    //
    // Stat the staging file mid-upload instead. That is the property, and it is
    // the one a Vec-accumulating implementation cannot fake.
    #[test]
    fn an_upload_writes_each_chunk_through_to_disk() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::at(dir.path(), "proj").expect("store opens");
        let mut upload = store.begin_blob_upload().expect("upload opens");

        let uploads = store.root().join("uploads");
        let staged = || -> u64 {
            std::fs::read_dir(&uploads)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter_map(|e| e.metadata().ok())
                        .filter(|m| m.is_file())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0)
        };

        upload.append(&[1u8; 4096]).unwrap();
        let after_first = staged();
        assert_eq!(
            after_first, 4096,
            "the first chunk must be on disk before finish(), found {after_first} bytes"
        );

        upload.append(&[2u8; 4096]).unwrap();
        let after_second = staged();
        assert_eq!(
            after_second, 8192,
            "the staging file must GROW as chunks arrive, found {after_second} bytes"
        );
    }

    // A mismatched digest must be refused AND leave no blob behind. Split from the
    // test above, which previously asserted both in one body.
    #[test]
    fn a_mismatched_digest_is_refused_and_stores_nothing() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::at(dir.path(), "proj").expect("store opens");
        let mut upload = store.begin_blob_upload().expect("upload opens");
        upload.append(&[7u8; 128]).unwrap();

        let wrong = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        assert!(
            !upload.finish(wrong).unwrap(),
            "a mismatched digest must be rejected"
        );
        assert_eq!(
            store.blob_size(wrong).unwrap(),
            None,
            "the rejected upload must leave no blob behind"
        );
    }

    // A mid-transfer failure must leave the session resumable.
    //
    // The buffered implementation got this for free: the `Bytes` extractor
    // rejected a truncated body before the handler ran, so the session and its
    // accepted chunks survived and `Range` told the client where to resume.
    // Streaming takes the session OUT of the map to protect it from a concurrent
    // sweep, which means an early return drops it - unlinking the staging file and
    // every chunk already accepted, so the layer restarts from byte 0. Fails if
    // the error-path re-insert is removed.
    //
    // Driven through `oneshot` with a body stream that errors, rather than a real
    // truncated request: a short HTTP body just makes the server wait for bytes
    // that never arrive, which hangs instead of failing.
    #[tokio::test]
    async fn a_failed_chunk_stream_leaves_the_session_resumable() {
        use axum::body::Bytes;
        use base64::Engine as _;
        use tower::ServiceExt as _;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let uploads = Arc::new(UploadSessions::default());
        let router =
            write_router_with_uploads(store, WriteToken::new(WRITE_TOKEN), uploads.clone());

        let creds = base64::engine::general_purpose::STANDARD
            .encode(format!("{WRITE_USERNAME}:{WRITE_TOKEN}"));
        let auth = format!("Basic {creds}");

        // Open a session and land one good chunk.
        let opened = router
            .clone()
            .oneshot(
                axum::http::Request::post("/v2/my-app/blobs/uploads/")
                    .header(header::AUTHORIZATION, &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(opened.status().as_u16(), 202);
        let location = opened
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let first = router
            .clone()
            .oneshot(
                axum::http::Request::patch(&location)
                    .header(header::AUTHORIZATION, &auth)
                    .body(Body::from(vec![0x11u8; 4096]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status().as_u16(), 202);
        assert_eq!(
            uploads.inner.lock().unwrap().len(),
            1,
            "the session should be live after a good chunk"
        );

        // Now a chunk whose stream fails partway - the transport failure this
        // guards against.
        let failing = Body::from_stream(futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(&[0x22u8; 2048])),
            Err(std::io::Error::other("connection reset mid-chunk")),
        ]));
        let broken = router
            .clone()
            .oneshot(
                axum::http::Request::patch(&location)
                    .header(header::AUTHORIZATION, &auth)
                    .body(failing)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            broken.status().as_u16(),
            400,
            "a failed stream should be reported to the client"
        );

        // The decisive assertion: the session survived, so the client can resume
        // instead of restarting the layer.
        assert_eq!(
            uploads.inner.lock().unwrap().len(),
            1,
            "the session must survive a failed chunk stream, not be discarded"
        );
        let resumed = router
            .oneshot(
                axum::http::Request::patch(&location)
                    .header(header::AUTHORIZATION, &auth)
                    .body(Body::from(vec![0x33u8; 4096]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resumed.status().as_u16(),
            202,
            "a resumed chunk must be accepted, not 404 BLOB_UPLOAD_UNKNOWN"
        );
    }

    // An upload that opens a session and never finalizes it must not pin its
    // buffer forever: opening a later session reclaims it. Asserted on the map
    // directly because the leak is invisible from the wire - the abandoned
    // session returns nothing, it just occupies memory.
    /// A staging upload backed by a throwaway store, for tests that build
    /// `UploadSession`s by hand.
    fn staging_upload(dir: &TempDir) -> BlobUpload {
        BlobStore::at(dir.path(), "proj")
            .expect("store opens")
            .begin_blob_upload()
            .expect("staging upload opens")
    }

    #[test]
    fn abandoned_upload_sessions_are_evicted_when_a_new_one_opens() {
        let dir = TempDir::new().unwrap();
        let mut sessions = HashMap::new();
        sessions.insert(
            "abandoned".to_string(),
            UploadSession {
                upload: staging_upload(&dir),
                touched: Instant::now() - UPLOAD_SESSION_TTL - Duration::from_secs(1),
            },
        );
        sessions.insert(
            "in-progress".to_string(),
            UploadSession {
                // Older than the TTL as a whole, but still receiving chunks - a
                // slow link must not be mistaken for an abandoned push.
                upload: staging_upload(&dir),
                touched: Instant::now(),
            },
        );

        evict_expired(&mut sessions, Instant::now());

        assert!(
            !sessions.contains_key("abandoned"),
            "a session untouched past the TTL must be reclaimed"
        );
        assert!(
            sessions.contains_key("in-progress"),
            "a session still receiving chunks must survive eviction"
        );
    }

    // The TTL boundary itself: one tick either side must decide differently.
    // Without an injected clock this is unreachable, which is what let the
    // previous version of the PATCH test below assert nothing.
    #[test]
    fn eviction_turns_on_the_ttl_boundary() {
        let dir = TempDir::new().unwrap();
        let base = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "just-inside".to_string(),
            UploadSession {
                upload: staging_upload(&dir),
                touched: base,
            },
        );
        evict_expired(
            &mut sessions,
            base + UPLOAD_SESSION_TTL - Duration::from_millis(1),
        );
        assert!(
            sessions.contains_key("just-inside"),
            "a session one tick inside the TTL must survive"
        );

        evict_expired(&mut sessions, base + UPLOAD_SESSION_TTL);
        assert!(
            sessions.is_empty(),
            "a session at the TTL must be reclaimed"
        );
    }

    /// Serve the write router over a session map the test also holds, so it can
    /// age a live session to the TTL boundary instead of waiting ten minutes.
    async fn spawn_with_uploads() -> (String, Arc<UploadSessions>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let uploads = Arc::new(UploadSessions::default());
        let app = write_router_with_uploads(store, WriteToken::new(WRITE_TOKEN), uploads.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), uploads, dir)
    }

    /// Backdate every live session by `age`, simulating time passing without
    /// spending it.
    fn age_sessions(uploads: &UploadSessions, age: Duration) {
        let mut sessions = uploads.inner.lock().unwrap();
        for session in sessions.values_mut() {
            session.touched -= age;
        }
    }

    // A PATCH must refresh the session clock, so a multi-chunk transfer spanning
    // more than the TTL is not evicted out from under an active client.
    //
    // The falsifier is the backdating: the session is pushed past the TTL, then
    // PATCHed, then swept. It survives ONLY if patch_route actually rewrote
    // `touched`. Deleting that one line fails this test - which the previous
    // version of it did not, because its sweep ran milliseconds after the POST
    // and would have passed with the mechanism removed entirely.
    #[tokio::test]
    async fn patching_a_session_refreshes_its_ttl() {
        let (base, uploads, _dir) = spawn_with_uploads().await;
        let client = reqwest::Client::new();

        let opened = client
            .post(format!("{base}/v2/my-app/blobs/uploads/"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(opened.status().as_u16(), 202);
        let location = opened
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Push the session past the eviction boundary, then send a chunk.
        age_sessions(&uploads, UPLOAD_SESSION_TTL + Duration::from_secs(60));
        let patched = client
            .patch(format!("{base}{location}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(b"chunk".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(
            patched.status().as_u16(),
            202,
            "an in-flight chunk must be accepted"
        );

        // Opening a second session runs the sweep. The first is only safe if the
        // PATCH above reset its clock.
        client
            .post(format!("{base}/v2/other-app/blobs/uploads/"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();

        let still_there = client
            .patch(format!("{base}{location}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(b"more".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(
            still_there.status().as_u16(),
            202,
            "the PATCH must have refreshed the TTL, so the sweep must not evict it"
        );
    }

    // The other half: a session that is NOT patched past the boundary really is
    // swept, and the client learns via 404 rather than silently succeeding.
    // Together with the test above this pins both directions of the mechanism.
    #[tokio::test]
    async fn an_aged_session_is_swept_and_its_next_chunk_404s() {
        let (base, uploads, _dir) = spawn_with_uploads().await;
        let client = reqwest::Client::new();

        let opened = client
            .post(format!("{base}/v2/my-app/blobs/uploads/"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        let location = opened
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        age_sessions(&uploads, UPLOAD_SESSION_TTL + Duration::from_secs(60));

        // No PATCH this time - the sweep on the next POST should reclaim it.
        client
            .post(format!("{base}/v2/other-app/blobs/uploads/"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .send()
            .await
            .unwrap();

        let gone = client
            .patch(format!("{base}{location}"))
            .basic_auth(WRITE_USERNAME, Some(WRITE_TOKEN))
            .body(b"chunk".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(
            gone.status().as_u16(),
            404,
            "an abandoned session must be reclaimed, and its next chunk rejected"
        );
    }
}

#[cfg(test)]
mod bulk_listener {
    use super::*;
    use crate::utils::container_dev::store::BlobStore;
    use crate::utils::container_dev::tls::DevSession;
    use sha2::{Digest as _, Sha256};
    use std::net::SocketAddr;
    use tempfile::TempDir;

    const RUNTIME: &str = "dev-runtime";

    /// Compute the OCI digest (`sha256:<hex>`) of `bytes`.
    fn digest_of(bytes: &[u8]) -> String {
        let hex: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    /// Bind the dedicated bulk read listener over a fresh session's TLS material
    /// and a per-project store seeded with `blob`.
    ///
    /// Returns the loopback `https://` base URL, the minted session (whose CA
    /// cert the client pins and whose read/control token it presents), the live
    /// listener handle (kept alive by the caller), the seeded blob digest, and
    /// the temp-dir guard.
    async fn spawn_bulk(blob: &[u8]) -> (String, DevSession, BulkListener, String, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let digest = digest_of(blob);
        store.write_blob(&digest, blob).unwrap();

        let session = DevSession::mint(RUNTIME).expect("session mints");
        let listener = BulkListener::bind(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            store,
            session.read_token.clone(),
            session.tls.server_config(),
        )
        .await
        .expect("bulk listener binds");
        let base = format!("https://127.0.0.1:{}", listener.local_addr().port());
        (base, session, listener, digest, dir)
    }

    /// A reqwest client that trusts ONLY the session CA, so it validates the
    /// leaf's `127.0.0.1` IP SAN and rejects any other chain.
    fn tls_client(session: &DevSession) -> reqwest::Client {
        let ca = reqwest::Certificate::from_pem(session.tls.ca_cert_pem().as_bytes())
            .expect("session CA cert parses");
        reqwest::Client::builder()
            .add_root_certificate(ca)
            .build()
            .expect("TLS client builds")
    }

    #[tokio::test]
    async fn token_gated_pull_succeeds_over_the_dedicated_bulk_tls_listener() {
        let blob: Vec<u8> = (0u8..=255).collect();
        let (base, session, listener, digest, _dir) = spawn_bulk(&blob).await;

        // The listener owns a real bound loopback socket (its listener identity,
        // design H-1): port 0 was resolved to a concrete port.
        assert_ne!(
            listener.local_addr().port(),
            0,
            "the bulk listener must bind a concrete socket"
        );

        let resp = tls_client(&session)
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .bearer_auth(session.read_token.secret())
            .send()
            .await
            .expect("bulk pull request completes");

        assert_eq!(
            resp.status().as_u16(),
            200,
            "a Bearer-token-gated blob pull must succeed over the dedicated bulk TLS listener"
        );
        assert_eq!(
            resp.headers()
                .get("docker-content-digest")
                .and_then(|h| h.to_str().ok()),
            Some(digest.as_str()),
        );
        // The exact blob bytes come back over the dedicated socket.
        assert_eq!(resp.bytes().await.unwrap().as_ref(), blob.as_slice());
    }

    #[tokio::test]
    async fn bulk_pull_without_the_read_token_is_refused_before_any_bytes() {
        let blob = b"a-container-layer".to_vec();
        let (base, session, _listener, digest, _dir) = spawn_bulk(&blob).await;

        let resp = tls_client(&session)
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .send()
            .await
            .expect("anonymous bulk pull request completes");

        assert_eq!(
            resp.status().as_u16(),
            401,
            "an anonymous pull on the bulk listener must be refused (fail-closed pre-stream)"
        );
        // Fail-closed: the challenge is a bare Bearer, and no blob body leaks.
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            challenge.starts_with("bearer"),
            "the bulk listener must challenge with Bearer, got {challenge:?}"
        );
        assert_ne!(
            resp.bytes().await.unwrap().as_ref(),
            blob.as_slice(),
            "a refused pull must not stream the blob body"
        );
    }

    #[tokio::test]
    async fn bulk_bytes_travel_as_http_not_a_control_websocket_frame() {
        let blob: Vec<u8> = (0u8..200).collect();
        let (base, session, _listener, digest, _dir) = spawn_bulk(&blob).await;

        // Attempt a WebSocket upgrade on the bulk socket while pulling a blob.
        // The dedicated bulk listener carries ONLY the OCI read router (no WS /
        // control route), so the engine gets the blob as a plain HTTP body and
        // NEVER a `101 Switching Protocols` control stream. This is the D9/H-1
        // guarantee: bulk transfers never share the control WS byte stream, so a
        // large pull cannot head-of-line-block a control frame.
        let resp = tls_client(&session)
            .get(format!("{base}/v2/my-app/blobs/{digest}"))
            .bearer_auth(session.read_token.secret())
            .header(reqwest::header::CONNECTION, "Upgrade")
            .header(reqwest::header::UPGRADE, "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .expect("bulk pull with upgrade headers completes");

        assert_ne!(
            resp.status().as_u16(),
            101,
            "the bulk listener must NEVER switch to a WebSocket/control stream"
        );
        assert_eq!(
            resp.status().as_u16(),
            200,
            "the blob must be served as an ordinary HTTP body on the bulk socket"
        );
        assert_eq!(
            resp.bytes().await.unwrap().as_ref(),
            blob.as_slice(),
            "the full blob must arrive over HTTP, not a WS frame"
        );
    }

    #[tokio::test]
    async fn bulk_listener_binds_a_socket_distinct_from_the_write_listener() {
        // The bulk read listener seeded with a blob.
        let blob = b"layer-bytes".to_vec();
        let (bulk_base, session, bulk, digest, _dir) = spawn_bulk(&blob).await;

        // A separate WRITE listener (task 3.3) on its own socket. The three-
        // listener model (design D9/H-1) gives each route class its OWN socket:
        // this is the write leg, distinct from the bulk read leg.
        let write_dir = TempDir::new().unwrap();
        let write_store = Arc::new(BlobStore::at(write_dir.path(), "wproj").expect("store opens"));
        let write_app = write_router(write_store, session.write_token.clone());
        let write_tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let write_addr = write_tcp.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(write_tcp, write_app).await.unwrap();
        });

        // Distinct sockets: the bulk read listener and the write listener never
        // share a socket, so a device handed only the bulk endpoint cannot reach
        // the write listener.
        assert_ne!(
            bulk.local_addr(),
            write_addr,
            "the bulk read listener and the write listener must be distinct sockets"
        );

        // The bulk socket serves the token-gated read.
        let bulk_ok = tls_client(&session)
            .get(format!("{bulk_base}/v2/my-app/blobs/{digest}"))
            .bearer_auth(session.read_token.secret())
            .send()
            .await
            .expect("bulk read completes");
        assert_eq!(bulk_ok.status().as_u16(), 200);

        // The write socket is a different route class: it refuses an anonymous
        // request with a Basic challenge, never a Bearer read/control token.
        let write_anon = reqwest::get(format!("http://{write_addr}/v2/"))
            .await
            .expect("write listener responds");
        assert_eq!(
            write_anon.status().as_u16(),
            401,
            "the write listener gates on the Basic write token, not the read token"
        );
        let write_challenge = write_anon
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            write_challenge.starts_with("basic"),
            "the write listener must issue a Basic challenge, got {write_challenge:?}"
        );
    }

    #[tokio::test]
    async fn vm_write_listener_terminates_tls_not_plaintext() {
        // On the VM push path the guest reaches the write listener via the QEMU
        // host alias 10.0.2.2 (NOT a docker-trusted 127.0.0.0/8 loopback), so the
        // listener MUST terminate the per-project leaf TLS its delivered certs.d
        // CA pins (design A2/H4). A plaintext HTTP write listener there is a bug:
        // the guest daemon, configured for HTTPS via certs.d, could not push.
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "wproj").expect("store opens"));
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let _task = serve_write_router_tls(
            tcp,
            session.tls.server_config(),
            store,
            session.write_token.clone(),
        );

        // HTTPS with the session CA: the TLS handshake succeeds and an
        // unauthenticated write is refused with a Basic challenge (not a
        // transport error).
        let resp = tls_client(&session)
            .get(format!("https://127.0.0.1:{port}/v2/"))
            .send()
            .await
            .expect("an HTTPS request over the TLS write listener completes");
        assert_eq!(
            resp.status().as_u16(),
            401,
            "the TLS write listener must gate an unauthenticated write with 401"
        );

        // A plaintext HTTP request to the same port must fail at the transport
        // layer, proving the listener speaks TLS. Before the fix the write
        // listener served plaintext and this would return an HTTP status.
        let plain = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/v2/"))
            .send()
            .await;
        assert!(
            plain.is_err(),
            "a plaintext HTTP request to the TLS write listener must fail at the transport, \
             got {plain:?}"
        );
    }
}
