//! End-to-end round-trip for Container Dev Mode (task 8.1), driven at the
//! interface level against the REAL listeners a device talks to.
//!
//! Two falsifiable properties of the sync round-trip are asserted:
//!
//! 1. **Delta pull.** After a one-line change confined to the final layer, a
//!    device that already holds the previous image pulls ONLY the changed layer
//!    over the dedicated bulk listener — the shared config and base layers are
//!    byte-identical by digest and are never re-transferred. Falsifier: the
//!    whole image is re-pulled on a one-line change.
//! 2. **Restart trigger.** The host tells a connected device to move to the new
//!    digest over the control WS: a device that reconnects reporting the stale
//!    running digest receives a `sync` frame carrying the new digest — the
//!    signal that drives the device to pull-and-restart the container.
//!    Falsifier: no sync is delivered, so the container is never restarted.
//!
//! The push side uses the plain-HTTP write listener (Basic write token); the
//! pull and control sides use the session's pinned-CA TLS leaf, matching
//! production. Push and pull share ONE per-project store, so a blob pushed on
//! the write leg is pullable on the bulk leg — the actual round-trip.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use avocado_cli::utils::container_dev::auth::WRITE_USERNAME;
use avocado_cli::utils::container_dev::registry::{write_router, BulkListener};
use avocado_cli::utils::container_dev::store::BlobStore;
use avocado_cli::utils::container_dev::tls::DevSession;
use avocado_cli::utils::container_dev::watcher::arch_guard::HelloArchBook;
use avocado_cli::utils::container_dev::ws::ControlServer;
use avocado_cli::utils::container_dev::ws::{DesiredState, DeviceFrame, Hello, HostFrame};

use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

const RUNTIME: &str = "dev-runtime";
const NAME: &str = "my-app";
const TAG: &str = "dev";

/// Compute the OCI digest (`sha256:<hex>`) of `bytes`.
fn digest_of(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A single-platform image manifest referencing `config` and `layers` by digest.
fn manifest_for(config: &[u8], layers: &[&[u8]]) -> Vec<u8> {
    let layer_entries: Vec<_> = layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": digest_of(l),
                "size": l.len(),
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": digest_of(config),
            "size": config.len(),
        },
        "layers": layer_entries,
    }))
    .unwrap()
}

/// The push (write) + pull (bulk) round-trip harness over ONE shared store.
struct Harness {
    write_base: String,
    bulk_base: String,
    session: DevSession,
    _bulk: BulkListener,
    _dir: TempDir,
}

/// Stand up the write listener (plain HTTP, Basic-gated) and the bulk read
/// listener (TLS, Bearer-gated) over a single shared per-project store.
async fn harness() -> Harness {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(BlobStore::at(dir.path(), "proj").expect("store opens"));
    let session = DevSession::mint(RUNTIME).expect("session mints");

    let write_app = write_router(Arc::clone(&store), session.write_token.clone());
    let write_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let write_addr = write_tcp.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(write_tcp, write_app).await.unwrap();
    });

    let bulk = BulkListener::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Arc::clone(&store),
        session.read_token.clone(),
        session.tls.server_config(),
    )
    .await
    .expect("bulk listener binds");

    Harness {
        write_base: format!("http://{write_addr}"),
        bulk_base: format!("https://127.0.0.1:{}", bulk.local_addr().port()),
        session,
        _bulk: bulk,
        _dir: dir,
    }
}

/// A reqwest client trusting ONLY the session CA (validates the leaf's SANs).
fn tls_client(session: &DevSession) -> reqwest::Client {
    let ca = reqwest::Certificate::from_pem(session.tls.ca_cert_pem().as_bytes())
        .expect("session CA cert parses");
    reqwest::Client::builder()
        .add_root_certificate(ca)
        .build()
        .expect("TLS client builds")
}

/// Push a blob monolithically to the write listener with the Basic write token.
async fn push_blob(h: &Harness, bytes: &[u8]) {
    let digest = digest_of(bytes);
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v2/{NAME}/blobs/uploads/?digest={digest}",
            h.write_base
        ))
        .basic_auth(WRITE_USERNAME, Some(h.session.write_token.secret()))
        .body(bytes.to_vec())
        .send()
        .await
        .expect("blob push completes");
    assert_eq!(resp.status().as_u16(), 201, "a blob push must be created");
}

/// Push a manifest under `TAG` with the Basic write token; returns its digest.
async fn push_manifest(h: &Harness, manifest: &[u8]) -> String {
    let resp = reqwest::Client::new()
        .put(format!("{}/v2/{NAME}/manifests/{TAG}", h.write_base))
        .basic_auth(WRITE_USERNAME, Some(h.session.write_token.secret()))
        .body(manifest.to_vec())
        .send()
        .await
        .expect("manifest push completes");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "a manifest push must be created"
    );
    digest_of(manifest)
}

/// The digests a manifest references (config + every layer), in wire order.
fn referenced_digests(manifest: &[u8]) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    let mut out = vec![v["config"]["digest"].as_str().unwrap().to_string()];
    for layer in v["layers"].as_array().unwrap() {
        out.push(layer["digest"].as_str().unwrap().to_string());
    }
    out
}

/// Simulate a device pull over the bulk listener: fetch the manifest, then GET
/// only the referenced blobs NOT already in `local`. Records each fetched blob
/// into `local` and returns the total bytes of blob bodies actually fetched.
async fn device_pull(h: &Harness, local: &mut HashSet<String>) -> u64 {
    let client = tls_client(&h.session);
    let manifest = client
        .get(format!("{}/v2/{NAME}/manifests/{TAG}", h.bulk_base))
        .bearer_auth(h.session.read_token.secret())
        .send()
        .await
        .expect("manifest pull completes");
    assert_eq!(
        manifest.status().as_u16(),
        200,
        "the manifest must be pullable"
    );
    let manifest_bytes = manifest.bytes().await.unwrap();

    let mut fetched_bytes = 0u64;
    for digest in referenced_digests(&manifest_bytes) {
        if local.contains(&digest) {
            continue; // already on the device — a delta pull skips it
        }
        let blob = client
            .get(format!("{}/v2/{NAME}/blobs/{digest}", h.bulk_base))
            .bearer_auth(h.session.read_token.secret())
            .send()
            .await
            .expect("blob pull completes");
        assert_eq!(
            blob.status().as_u16(),
            200,
            "a referenced blob must be pullable"
        );
        let body = blob.bytes().await.unwrap();
        assert_eq!(
            digest_of(&body),
            digest,
            "the pulled blob must match its digest"
        );
        fetched_bytes += body.len() as u64;
        local.insert(digest);
    }
    fetched_bytes
}

// ---- 1. a one-line change pulls only the changed layer over the bulk listener ----

#[tokio::test]
async fn a_one_line_change_pulls_only_the_changed_layer() {
    let h = harness().await;

    // A shared config and base layer, plus a top layer that differs between the
    // two builds — "a one-line change confined to the final layer".
    let config = b"image-config-json".to_vec();
    let base_layer = vec![0xABu8; 512 * 1024]; // 512 KiB shared base
    let top_v1 = b"top layer, revision 1".to_vec();
    let top_v2 = b"top layer, revision 2 (one line changed)".to_vec();

    // Push v1 and pull it: the device now holds config + base + top_v1.
    push_blob(&h, &config).await;
    push_blob(&h, &base_layer).await;
    push_blob(&h, &top_v1).await;
    let v1_manifest = manifest_for(&config, &[&base_layer, &top_v1]);
    let v1_digest = push_manifest(&h, &v1_manifest).await;

    let mut device_blobs = HashSet::new();
    let v1_bytes = device_pull(&h, &mut device_blobs).await;
    assert_eq!(
        v1_bytes,
        (config.len() + base_layer.len() + top_v1.len()) as u64,
        "the first pull fetches the whole image (config + base + top)"
    );

    // One-line change: only the top layer differs. Push v2 (shared blobs dedup
    // in the content-addressed store) and re-tag.
    push_blob(&h, &top_v2).await;
    let v2_manifest = manifest_for(&config, &[&base_layer, &top_v2]);
    let v2_digest = push_manifest(&h, &v2_manifest).await;
    assert_ne!(
        v1_digest, v2_digest,
        "a changed image must have a new manifest digest"
    );

    // The device pulls again. It must fetch ONLY the changed top layer: the
    // shared config and base layer are byte-identical by digest and already
    // local, so a delta pull never re-transfers them.
    let v2_bytes = device_pull(&h, &mut device_blobs).await;
    assert_eq!(
        v2_bytes,
        top_v2.len() as u64,
        "the second pull must transfer ONLY the changed layer, not the whole image \
         (got {v2_bytes} bytes, expected {})",
        top_v2.len()
    );
    // The shared base layer (the bulk of the image) was NOT re-pulled.
    assert!(
        v2_bytes < base_layer.len() as u64,
        "a one-line change must not re-transfer the shared base layer"
    );
}

// ---- 2. the device is told to restart with the new digest over the control WS ----

#[tokio::test]
async fn a_stale_device_is_synced_to_the_new_digest_over_the_control_ws() {
    let session = DevSession::mint(RUNTIME).expect("session mints");

    let v1_digest = digest_of(b"running-image-v1");
    let v2_digest = digest_of(b"running-image-v2");

    // Desired state after the change: the tag points at v2 (re-derived at `up`
    // from the engine's current watched tags, design D5).
    let desired = DesiredState::derive_from_watched_tags([(
        NAME.to_string(),
        TAG.to_string(),
        v2_digest.clone(),
    )]);
    let server = ControlServer::new(session.read_token.clone(), desired, HelloArchBook::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(session.tls.server_config());
    tokio::spawn(async move { server.serve_tls(listener, acceptor).await });

    // The device dials the control WS with the Bearer read/control token, pinning
    // the session CA (production discipline).
    let mut request = format!("wss://127.0.0.1:{}/", addr.port())
        .into_client_request()
        .expect("ws request builds");
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", session.read_token.secret())
            .parse()
            .unwrap(),
    );
    let connector = pinned_ca_connector(session.tls.ca_cert_pem());
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .expect("authenticated control-WS upgrade succeeds");

    // The device reports the STALE digest it is currently running.
    let hello = DeviceFrame::Hello(Hello {
        device_id: "dev-1".to_string(),
        arch: "x86_64".to_string(),
        running_digest: v1_digest.clone(),
    });
    ws.send(Message::text(serde_json::to_string(&hello).unwrap()))
        .await
        .expect("hello sends");

    // The host reconciles and pushes a sync to the NEW digest — the trigger that
    // drives the device to pull-and-restart the container.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("a sync frame arrives before the timeout")
        .expect("the ws stream yields a frame")
        .expect("the frame is not an error");
    let frame: HostFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(
        frame,
        HostFrame::Sync {
            image: NAME.to_string(),
            tag: TAG.to_string(),
            digest: v2_digest.clone(),
        },
        "a device reporting the stale digest must be told to move to the new digest"
    );
}

/// A `tokio_tungstenite` TLS connector trusting ONLY `ca_cert_pem` — the pinned-CA
/// discipline the production control-WS client uses.
fn pinned_ca_connector(ca_cert_pem: &str) -> tokio_tungstenite::Connector {
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
