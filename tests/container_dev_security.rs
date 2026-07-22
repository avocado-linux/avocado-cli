//! Security assertions for the Container Dev Mode embedded registry and control
//! WebSocket, driven at the interface level against the REAL listeners
//! (task 8.2).
//!
//! Every case here spins up a live listener from one [`DevSession`] and drives
//! it with a pinned-CA client, then asserts the exact `401` a removed gate would
//! turn into a success. The falsifiers this file guards (ALL must be false):
//!
//! - an unauthenticated read/WS is served;
//! - an unauthenticated write succeeds on either interface;
//! - the Bearer read/control token authorizes a write (H-A compromised device);
//! - a wrong-password Basic credential authorizes a write (G-3);
//! - the Basic write token is honored on a read route (M-2).
//!
//! The write listener is served over plain HTTP (its gate is the Basic write
//! token, which the tests exercise directly); the bulk read listener and the
//! control WS run over the session's pinned-CA TLS leaf, matching production.

use std::net::SocketAddr;
use std::sync::Arc;

use avocado_cli::utils::container_dev::auth::WRITE_USERNAME;
use avocado_cli::utils::container_dev::registry::{write_router, BulkListener};
use avocado_cli::utils::container_dev::store::BlobStore;
use avocado_cli::utils::container_dev::tls::DevSession;
use avocado_cli::utils::container_dev::watcher::arch_guard::HelloArchBook;
use avocado_cli::utils::container_dev::ws::{ControlServer, DesiredState};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const RUNTIME: &str = "dev-runtime";

/// Compute the OCI digest (`sha256:<hex>`) of `bytes`.
fn digest_of(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A minimal single-platform image manifest to push at a write route.
fn manifest_bytes() -> Vec<u8> {
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

/// Bind the dedicated bulk READ listener (Bearer-gated, TLS) over a fresh
/// per-project store seeded with `blob`, using `session`'s read token and leaf.
///
/// Returns the loopback `https://` base URL, the live listener handle (kept
/// alive by the caller), the seeded blob digest, and the temp-dir guard.
async fn spawn_bulk(session: &DevSession, blob: &[u8]) -> (String, BulkListener, String, TempDir) {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
    let digest = digest_of(blob);
    store.write_blob(&digest, blob).unwrap();

    let listener = BulkListener::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        store,
        session.read_token.clone(),
        session.tls.server_config(),
    )
    .await
    .expect("bulk listener binds");
    let base = format!("https://127.0.0.1:{}", listener.local_addr().port());
    (base, listener, digest, dir)
}

/// Start the WRITE listener (Basic-gated) over a fresh per-project store, using
/// `session`'s write token. Served over plain HTTP; the gate under test is the
/// Basic credential, not the transport. Returns its base URL and the temp-dir
/// guard.
async fn spawn_write(session: &DevSession) -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(BlobStore::at(dir.path(), "wproj").expect("store opens"));
    let app = write_router(store, session.write_token.clone());
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(tcp, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// Start the control WS server over the session's pinned-CA TLS leaf; return its
/// `wss://` base URL. The gate under test is the Bearer read/control token on
/// the upgrade.
async fn spawn_ws_tls(session: &DevSession) -> String {
    let server = ControlServer::new(
        session.read_token.clone(),
        DesiredState::default(),
        HelloArchBook::new(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(session.tls.server_config());
    tokio::spawn(async move { server.serve_tls(listener, acceptor).await });
    format!("wss://{addr}/")
}

/// A reqwest client trusting ONLY the session CA, so it validates the leaf's
/// `127.0.0.1` IP SAN and rejects any other chain (never native roots).
fn tls_client(session: &DevSession) -> reqwest::Client {
    let ca = reqwest::Certificate::from_pem(session.tls.ca_cert_pem().as_bytes())
        .expect("session CA cert parses");
    reqwest::Client::builder()
        .add_root_certificate(ca)
        .build()
        .expect("TLS client builds")
}

/// A `tokio_tungstenite` TLS connector trusting ONLY `ca_cert_pem` — the same
/// pinned-CA discipline the production control-WS client uses.
fn pinned_ca_connector(ca_cert_pem: &str) -> tokio_tungstenite::Connector {
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

// ---- 1. an unauthenticated read on the bulk listener is rejected ----

#[tokio::test]
async fn unauthenticated_read_is_rejected_with_401() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let (base, _listener, digest, _dir) = spawn_bulk(&session, b"a-container-layer").await;
    let client = tls_client(&session);

    // The API version ping with no Authorization header.
    let ping = client
        .get(format!("{base}/v2/"))
        .send()
        .await
        .expect("anonymous ping completes");
    assert_eq!(
        ping.status().as_u16(),
        401,
        "an unauthenticated read on the bulk listener must be refused"
    );

    // A blob path with no Authorization header must also be refused before any
    // bytes are served.
    let blob = client
        .get(format!("{base}/v2/my-app/blobs/{digest}"))
        .send()
        .await
        .expect("anonymous blob pull completes");
    assert_eq!(
        blob.status().as_u16(),
        401,
        "an unauthenticated blob pull must be refused"
    );
}

// ---- 2. an unauthenticated WS upgrade is rejected ----

#[tokio::test]
async fn unauthenticated_ws_upgrade_is_rejected_with_401() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let url = spawn_ws_tls(&session).await;
    let connector = pinned_ca_connector(session.tls.ca_cert_pem());

    // No Authorization header on the upgrade request at all.
    let request = url.into_client_request().expect("ws request builds");
    let err =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .expect_err("an unauthenticated WS upgrade must be rejected");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(
            resp.status().as_u16(),
            401,
            "a tokenless control-WS upgrade must be 401"
        ),
        other => panic!("expected an HTTP 401 on the WS upgrade, got {other:?}"),
    }
}

// ---- 3. an unauthenticated write is refused on BOTH interfaces ----

#[tokio::test]
async fn unauthenticated_write_is_refused_on_both_interfaces() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let body = manifest_bytes();

    // (a) the write listener: manifest PUT, blob-upload POST, and the gated
    // `GET /v2/` ping all refuse an anonymous request.
    let (write_base, _wdir) = spawn_write(&session).await;
    let anon = reqwest::Client::new();

    let put = anon
        .put(format!("{write_base}/v2/my-app/manifests/dev"))
        .body(body.clone())
        .send()
        .await
        .expect("anonymous manifest PUT completes");
    assert_eq!(
        put.status().as_u16(),
        401,
        "an anonymous manifest write must be refused"
    );

    let post = anon
        .post(format!("{write_base}/v2/my-app/blobs/uploads/"))
        .send()
        .await
        .expect("anonymous upload POST completes");
    assert_eq!(
        post.status().as_u16(),
        401,
        "an anonymous blob-upload open must be refused"
    );

    let ping = anon
        .get(format!("{write_base}/v2/"))
        .send()
        .await
        .expect("anonymous write-listener ping completes");
    assert_eq!(
        ping.status().as_u16(),
        401,
        "the write listener's gated ping must refuse an anonymous request"
    );

    // (b) the bulk READ listener exposes NO write route: an anonymous write verb
    // is refused by the read gate before any routing, so it never reaches a
    // write handler (there is none on this listener).
    let (bulk_base, _listener, _digest, _bdir) = spawn_bulk(&session, b"seed").await;
    let bulk_put = tls_client(&session)
        .put(format!("{bulk_base}/v2/my-app/manifests/dev"))
        .body(body)
        .send()
        .await
        .expect("anonymous PUT to the bulk listener completes");
    assert_eq!(
        bulk_put.status().as_u16(),
        401,
        "a write verb on the bulk read listener must not be served/authorized"
    );
}

// ---- 4. the Bearer read/control token is refused on EVERY write route (H-A) ----

#[tokio::test]
async fn bearer_read_control_token_is_refused_on_every_write_route() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let (write_base, _wdir) = spawn_write(&session).await;
    let read = session.read_token.secret();
    let client = reqwest::Client::new();

    // A compromised device holds ONLY the Bearer read/control token. Presenting
    // it on any write route must be refused — the write listener requires Basic.

    // manifest PUT
    let put = client
        .put(format!("{write_base}/v2/my-app/manifests/dev"))
        .bearer_auth(read)
        .body(manifest_bytes())
        .send()
        .await
        .expect("bearer manifest PUT completes");
    assert_eq!(
        put.status().as_u16(),
        401,
        "the Bearer read/control token must not authorize a manifest write"
    );

    // blob-upload POST
    let post = client
        .post(format!("{write_base}/v2/my-app/blobs/uploads/"))
        .bearer_auth(read)
        .send()
        .await
        .expect("bearer upload POST completes");
    assert_eq!(
        post.status().as_u16(),
        401,
        "the Bearer read/control token must not authorize a blob-upload open"
    );

    // dedup HEAD probe
    let missing = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let head = client
        .head(format!("{write_base}/v2/my-app/blobs/{missing}"))
        .bearer_auth(read)
        .send()
        .await
        .expect("bearer dedup HEAD completes");
    assert_eq!(
        head.status().as_u16(),
        401,
        "the Bearer read/control token must not authorize a dedup probe"
    );
}

// ---- 5. a wrong-password Basic credential is refused on a write route (G-3) ----

#[tokio::test]
async fn wrong_password_basic_credential_is_refused_on_a_write_route() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let (write_base, _wdir) = spawn_write(&session).await;

    // The correct username but a password that is not the session write token.
    let resp = reqwest::Client::new()
        .put(format!("{write_base}/v2/my-app/manifests/dev"))
        .basic_auth(WRITE_USERNAME, Some("not-the-write-token"))
        .body(manifest_bytes())
        .send()
        .await
        .expect("wrong-password write completes");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "a Basic credential with the wrong password must be refused on a write route"
    );
}

// ---- 6. the Basic write token is refused on a read route (M-2) ----

#[tokio::test]
async fn basic_write_token_is_refused_on_a_read_route() {
    let session = DevSession::mint(RUNTIME).expect("session mints");
    let (base, _listener, digest, _dir) = spawn_bulk(&session, b"layer-bytes").await;

    // The host-only write token presented in its Basic transport form on the
    // bulk READ listener must be refused: read routes accept only Bearer.
    let resp = tls_client(&session)
        .get(format!("{base}/v2/my-app/blobs/{digest}"))
        .basic_auth(WRITE_USERNAME, Some(session.write_token.secret()))
        .send()
        .await
        .expect("basic-on-read pull completes");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "the Basic write token must not be honored on a read route"
    );
}
