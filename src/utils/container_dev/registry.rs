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
use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use uuid::Uuid;

use super::auth::{require_basic_write, require_bearer_read, ReadToken, WriteToken};
use super::store::{BlobStore, StoreError};

/// Non-standard OCI response header carrying the content digest of the served
/// manifest or blob.
const DOCKER_CONTENT_DIGEST: &str = "docker-content-digest";

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

/// In-flight chunked-upload sessions, keyed by upload UUID.
///
/// The OCI blob-upload protocol is stateful: `POST` opens a session, `PATCH`
/// appends chunks, and `PUT` finalizes with the expected digest. The buffered
/// bytes live here until finalization writes them into the content-addressed
/// store. Dev-loop scale (a handful of layers per push) keeps in-memory
/// buffering acceptable.
#[derive(Default)]
struct UploadSessions {
    inner: Mutex<HashMap<String, Vec<u8>>>,
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
    let state = WriteState {
        store,
        uploads: Arc::new(UploadSessions::default()),
    };
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
        .with_state(state)
}

/// `POST /v2/<name>/blobs/uploads/[?digest=<digest>]` — open a chunked upload,
/// or complete a monolithic upload when a `digest` query is present.
async fn post_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
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

    if let Some(digest) = q.get("digest") {
        // Monolithic upload: the whole blob arrives with the POST.
        return store_blob(&state, name, digest, &body);
    }

    let uuid = Uuid::new_v4().to_string();
    state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned")
        .insert(uuid.clone(), Vec::new());
    upload_accepted(name, &uuid, 0)
}

/// `PATCH /v2/<name>/blobs/uploads/<uuid>` — append a chunk to a session.
async fn patch_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    body: Bytes,
) -> Response {
    let Some((name, uuid)) = split_upload(&rest) else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "unsupported write path",
        );
    };
    let mut sessions = state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned");
    let Some(buf) = sessions.get_mut(uuid) else {
        return oci_error(
            StatusCode::NOT_FOUND,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown",
        );
    };
    let start = buf.len() as u64;
    buf.extend_from_slice(&body);
    let end = buf.len() as u64;
    upload_range_accepted(name, uuid, start, end)
}

/// `PUT` on the write listener: finalize a blob upload
/// (`.../blobs/uploads/<uuid>?digest=`) or store a manifest
/// (`.../manifests/<reference>`).
async fn put_route(
    State(state): State<WriteState>,
    Path(rest): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Some((name, reference)) = rest.split_once("/manifests/") {
        return put_manifest(&state, name, reference, &body);
    }
    if let Some((name, uuid)) = split_upload(&rest) {
        return finalize_upload(
            &state,
            name,
            uuid,
            q.get("digest").map(String::as_str),
            &body,
        );
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
    match state.store.read_blob(digest) {
        Ok(Some(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, bytes.len().to_string())
            .header(DOCKER_CONTENT_DIGEST, digest)
            .body(Body::empty())
            .expect("blob-head response is always valid"),
        Ok(None) => blob_unknown(),
        Err(e) => store_error(&e),
    }
}

/// Complete a chunked upload: append the final `body`, verify it hashes to the
/// client-supplied `digest`, and store it.
fn finalize_upload(
    state: &WriteState,
    name: &str,
    uuid: &str,
    digest: Option<&str>,
    body: &[u8],
) -> Response {
    let Some(digest) = digest else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "digest query parameter required to finalize an upload",
        );
    };
    let mut buf = match state
        .uploads
        .inner
        .lock()
        .expect("upload sessions mutex is not poisoned")
        .remove(uuid)
    {
        Some(b) => b,
        None => {
            return oci_error(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload session unknown",
            )
        }
    };
    buf.extend_from_slice(body);
    store_blob(state, name, digest, &buf)
}

/// Verify `bytes` hashes to `digest` and write it to the store, returning the
/// `201 Created` a completed blob upload expects.
fn store_blob(state: &WriteState, name: &str, digest: &str, bytes: &[u8]) -> Response {
    let computed = compute_digest(bytes);
    if computed != digest {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "uploaded content does not match the supplied digest",
        );
    }
    match state.store.write_blob(digest, bytes) {
        Ok(_) => blob_created(name, digest),
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
        StoreError::NoHome | StoreError::Io(_) => oci_error(
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
        serve_blob(&state, &headers, digest)
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
fn serve_blob(state: &RegistryState, headers: &HeaderMap, digest: &str) -> Response {
    let bytes = match state.store.read_blob(digest) {
        Ok(Some(b)) => b,
        _ => return blob_unknown(),
    };
    let total = bytes.len() as u64;

    if let Some(range) = headers.get(header::RANGE) {
        return match parse_range(range, total) {
            Some((start, end)) => {
                let slice = bytes[start as usize..=end as usize].to_vec();
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    )
                    .header(DOCKER_CONTENT_DIGEST, digest)
                    .body(Body::from(slice))
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
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::from(bytes))
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
    }
}
