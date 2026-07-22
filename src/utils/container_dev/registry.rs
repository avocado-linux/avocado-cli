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
//! module never re-implements storage. The routes are assembled into a
//! [`Router`] here but are bound onto the dedicated bulk read listener by task
//! 3.7 — this module owns only the handlers and their routing. Write endpoints
//! (task 3.3), authentication (tasks 3.3/3.4), and TLS/listeners (tasks
//! 3.6/3.7) are out of scope here.
//!
//! HEAD requests are served by the same handler as GET: axum routes HEAD to the
//! GET handler and strips the response body while preserving the headers, so a
//! HEAD carries the resource's `Content-Length` and `Docker-Content-Digest`
//! with an empty body.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use super::store::BlobStore;

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

/// Build the OCI read router over `store`.
///
/// The returned router serves `GET /v2/`, manifest reads, and blob reads
/// (GET + HEAD). It is merged onto the bulk read listener in task 3.7.
pub fn read_router(store: Arc<BlobStore>) -> Router {
    Router::new()
        .route("/v2/", get(base))
        // A single wildcard route captures `<name>/manifests/<reference>` and
        // `<name>/blobs/<digest>`; `<name>` may itself contain `/`, so it
        // cannot be a fixed path segment. The suffix is dispatched by hand.
        .route("/v2/{*rest}", get(read))
        .with_state(RegistryState::new(store))
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

    /// Start the read router over a fresh per-project store and return the
    /// base URL plus a handle keeping the store's temp dir alive.
    async fn spawn() -> (String, Arc<BlobStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
        let app = read_router(store.clone());
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
