//! Cross-arch refusal integration test (task 8.3).
//!
//! Falsifier this file guards (must be false): a mismatched-arch image is
//! delivered to the device.
//!
//! The cross-arch guard (task 4.3) lives at
//! `avocado_cli::utils::container_dev::watcher::arch_guard`. A container image
//! built for one CPU architecture cannot run on a device of another, so the
//! guard probes an image's platform architecture, compares it against every
//! connected device's reported `hello.arch`, and REFUSES the sync before the
//! wrapped syncer ships anything. This file asserts that refusal at the
//! integration level against the REAL guard types — `ArchGuardSyncer`,
//! `HelloArchBook` (the live device-arch book fed by `record_hello`),
//! `DeviceArch`, `check_arch`, and `ArchMismatch`. The only doubles are the two
//! seams the guard was designed to accept: an [`ImageArchProbe`] (image arch,
//! standing in for a real `image inspect`) and an inner [`Syncer`] (the thing
//! that would actually ship). A refusal is proven concretely: the returned
//! error is an [`ArchMismatch`] AND the inner syncer's ship count stays 0, so a
//! wrong-arch image never reaches the device.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;

use avocado_cli::utils::container_dev::engine::TagEvent;
use avocado_cli::utils::container_dev::watcher::arch_guard::{
    check_arch, ArchGuardSyncer, ArchMismatch, DeviceArch, DeviceArchBook, HelloArchBook,
    ImageArchBook, ImageArchProbe,
};
use avocado_cli::utils::container_dev::watcher::{SyncMode, Syncer};

fn ev(image: &str) -> TagEvent {
    TagEvent {
        image: image.to_string(),
        image_id: Some(format!("sha256:{image}")),
    }
}

/// A probe reporting a fixed image architecture, so the guard's refusal logic is
/// exercised without a real engine `image inspect`.
struct FixedProbe(&'static str);

impl ImageArchProbe for FixedProbe {
    fn image_arch<'a>(
        &'a self,
        _event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<DeviceArch>> + Send + 'a>> {
        let arch = DeviceArch::parse(self.0);
        Box::pin(async move { Ok(arch) })
    }
}

/// The thing that would actually ship the image. It records every sync call so a
/// refusal is provable as "the ship never happened" (count stays 0), and a pass
/// is provable as "the ship ran exactly once".
#[derive(Default)]
struct ShipRecorder {
    ships: AtomicUsize,
}

impl ShipRecorder {
    fn ship_count(&self) -> usize {
        self.ships.load(Ordering::SeqCst)
    }
}

impl Syncer for ShipRecorder {
    fn sync<'a>(
        &'a self,
        _mode: SyncMode,
        _event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        self.ships.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

// ---- assertion 1: a mismatched-arch image is refused, not shipped ----

#[tokio::test]
async fn a_mismatched_arch_image_is_refused_and_never_ships() {
    let inner = Arc::new(ShipRecorder::default());
    let book = HelloArchBook::new();
    book.record_hello("dev-1", "aarch64"); // device reports arm64

    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")), // image built for x86_64
        Arc::new(book) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    let err = guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect_err("an amd64 image must be refused for an arm64 device");

    let mismatch = err
        .downcast_ref::<ArchMismatch>()
        .expect("the refusal must be an ArchMismatch, not some unrelated error");
    assert_eq!(mismatch.image_arch, "amd64");
    assert_eq!(mismatch.device_arch, "arm64");

    assert_eq!(
        inner.ship_count(),
        0,
        "a refused cross-arch sync must never ship the wrong-arch image to the device"
    );
}

// ---- assertion 2: a matching-arch image IS shipped (positive control) ----

#[tokio::test]
async fn a_matching_arch_image_is_shipped() {
    let inner = Arc::new(ShipRecorder::default());
    let book = HelloArchBook::new();
    book.record_hello("dev-1", "x86_64"); // device reports amd64

    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")), // image built for x86_64: matches
        Arc::new(book) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect("a matching-arch image must be allowed through the guard");

    assert_eq!(
        inner.ship_count(),
        1,
        "a matching-arch image must be shipped exactly once (the guard discriminates, \
         it does not refuse everything)"
    );
}

// ---- assertion 3: fleet model is fleet-wide — ANY mismatched device refuses ----
//
// The implemented guard is NOT per-device: `check_arch` refuses the whole sync
// if the image mismatches ANY connected device (watcher.rs `check_arch`). These
// cases assert that real design, not a per-device ship-to-the-matching-one model.

#[tokio::test]
async fn any_single_mismatched_device_in_a_fleet_refuses_the_whole_sync() {
    let inner = Arc::new(ShipRecorder::default());
    let book = HelloArchBook::new();
    book.record_hello("dev-amd64", "x86_64"); // matches the amd64 image
    book.record_hello("dev-arm64", "aarch64"); // does NOT match

    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")),
        Arc::new(book) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    let err = guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect_err("an amd64 image must be refused because the arm64 device cannot run it");

    let mismatch = err
        .downcast_ref::<ArchMismatch>()
        .expect("the refusal must be an ArchMismatch");
    assert_eq!(mismatch.device_arch, "arm64");

    assert_eq!(
        inner.ship_count(),
        0,
        "a fleet-wide refusal must ship to NO device, not even the matching amd64 one"
    );
}

#[tokio::test]
async fn a_homogeneous_matching_fleet_is_shipped() {
    let inner = Arc::new(ShipRecorder::default());
    let book = HelloArchBook::new();
    book.record_hello("dev-a", "aarch64"); // arm64
    book.record_hello("dev-b", "arm64"); // arm64 (uname vs GOARCH spelling)

    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("arm64")), // image matches every device
        Arc::new(book) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect("an image matching every device in the fleet must be allowed");

    assert_eq!(
        inner.ship_count(),
        1,
        "an all-matching fleet must ship exactly once"
    );
}

// ---- the pure guard function `check_arch` discriminates match from mismatch ----

#[test]
fn check_arch_refuses_a_mismatch_and_names_the_arches() {
    let err = check_arch(
        "my-app:dev",
        &DeviceArch::parse("amd64"),
        &[DeviceArch::parse("aarch64")],
    )
    .expect_err("an amd64 image must be refused for an arm64 device");
    assert_eq!(err.image, "my-app:dev");
    assert_eq!(err.image_arch, "amd64");
    assert_eq!(err.device_arch, "arm64");

    // The refusal is actionable: it names buildx and the device target platform,
    // so a bare `Err(())` sentinel would fail this.
    let msg = err.to_string();
    assert!(
        msg.contains("buildx"),
        "refusal must give buildx guidance: {msg}"
    );
    assert!(
        msg.contains("linux/arm64"),
        "refusal must name the device target platform: {msg}"
    );
}

#[test]
fn check_arch_allows_a_uname_vs_goarch_match() {
    // A device reporting uname `aarch64` and an image with GOARCH `arm64` are the
    // same architecture; the guard must NOT spuriously refuse them.
    check_arch(
        "app:dev",
        &DeviceArch::parse("arm64"),
        &[DeviceArch::parse("aarch64")],
    )
    .expect("a uname/GOARCH-equivalent arch must pass the guard");
}

// ---- assertion 5: a device that disconnects stops constraining the guard ----

/// Within one `up`, a developer tests against an aarch64 board, unplugs it, and
/// attaches an amd64 one. The arch book must not still be refusing amd64 syncs
/// on behalf of a board that is gone - `check_arch` refuses on ANY mismatch, so
/// a stale entry blocks the rest of the session and its buildx guidance names an
/// architecture nothing connected reports.
///
/// Asserted through the guard rather than by inspecting the book, because the
/// property that matters is "the sync goes through", not "the map is empty".
#[tokio::test]
async fn a_disconnected_device_no_longer_blocks_a_sync() {
    let book = HelloArchBook::new();
    let inner = Arc::new(ShipRecorder::default());
    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")), // the image the developer now builds
        Arc::new(book.clone()) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    // The arm64 board is connected: an amd64 sync is correctly refused.
    let arm_session = book.record_session("dev-arm64", "aarch64");
    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect_err("an amd64 image must be refused while an arm64 board is attached");
    assert_eq!(inner.ship_count(), 0);

    // The board is unplugged - its session ends.
    drop(arm_session);

    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect("the departed board must not keep refusing syncs");
    assert_eq!(
        inner.ship_count(),
        1,
        "the sync must actually ship once nothing disagrees with it"
    );
}

/// Two overlapping connections from one device must not evict each other: the
/// older session ending cannot remove an entry the newer one still needs, or a
/// reconnect would silently disarm the guard.
#[tokio::test]
async fn overlapping_sessions_for_one_device_refcount() {
    let book = HelloArchBook::new();
    let inner = Arc::new(ShipRecorder::default());
    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")),
        Arc::new(book.clone()) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    let first = book.record_session("dev-arm64", "aarch64");
    let second = book.record_session("dev-arm64", "aarch64");

    drop(first);
    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect_err("the device is still connected on its second session");

    drop(second);
    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect("with every session closed the guard must stop refusing");
}

/// Trust the session CA the way the device agent does, so the control-WS
/// upgrade is exercised over the real pinned-CA TLS rather than plaintext.
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

/// The guard only works because the book the `ControlServer` writes and the book
/// the `ArchGuardSyncer` reads are the SAME map. Every other test in this file
/// hands the guard a book it populated by hand, so all of them would still pass
/// with the two sides disconnected - which is exactly how the guard sat
/// unreachable before it was wired into `up`.
///
/// This drives the real path: a device sends a `Hello` over the control WS, the
/// server records its arch, and the guard - holding only a clone of the book it
/// was constructed with - refuses a mismatched image it never saw recorded.
/// Making `HelloArchBook::clone` a deep clone fails here.
///
/// Unwiring the guard in `up` does NOT - this test builds the `ControlServer` and
/// `ArchGuardSyncer` itself and clones the book by hand, so it verifies the
/// sharing semantics the wiring depends on, not the wiring. Nothing in the suite
/// exercises `DevUpCommand`, so changing `up` to hand the guard a fresh book
/// would leave every test green. That gap is real and unclosed.
#[tokio::test]
async fn a_hello_recorded_by_the_control_server_is_visible_to_the_guard() {
    use avocado_cli::utils::container_dev::tls::DevSession;
    use avocado_cli::utils::container_dev::ws::{ControlServer, DesiredState, DeviceFrame, Hello};
    use futures_util::SinkExt as _;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
    use tokio_tungstenite::tungstenite::Message;

    let session = DevSession::mint("dev-runtime").expect("session mints");

    // One book, cloned into both halves — exactly what `up` does.
    let book = HelloArchBook::new();
    let server = ControlServer::new(
        session.read_token.clone(),
        DesiredState::default(),
        book.clone(),
        ImageArchBook::new(),
    );
    let inner = Arc::new(ShipRecorder::default());
    // A third handle on the same book, used only to observe when the server has
    // processed the hello - so the poll below does not itself drive syncs.
    let observer = book.clone();
    let guard = ArchGuardSyncer::new(
        inner.clone() as Arc<dyn Syncer>,
        Arc::new(FixedProbe("amd64")), // image built for x86_64
        Arc::new(book) as Arc<dyn DeviceArchBook>,
        ImageArchBook::new(),
    );

    // Nothing recorded yet: the guard has no device to disagree with.
    guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect("an empty book has nobody to mismatch");
    assert_eq!(inner.ship_count(), 1);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(session.tls.server_config());
    tokio::spawn(async move { server.serve_tls(listener, acceptor).await });

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

    // An aarch64 device announces itself. Only the SERVER touches the book here.
    let hello = DeviceFrame::Hello(Hello {
        device_id: "dev-arm64".to_string(),
        arch: "aarch64".to_string(),
        running_digest: String::new(),
    });
    ws.send(Message::text(serde_json::to_string(&hello).unwrap()))
        .await
        .expect("hello sends");

    // Wait for the server to record it, bounded so a regression fails rather
    // than hangs. Observing the book directly keeps the wait from driving syncs
    // of its own, so the ship count below means exactly one thing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while observer.device_arches().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the control server's hello never reached the guard's book: the two halves \
             are not sharing one map"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let before = inner.ship_count();
    let refused = guard
        .sync(SyncMode::Push, &ev("my-app:dev"))
        .await
        .expect_err("an amd64 image must be refused once an arm64 device has said hello");

    let mismatch = refused
        .downcast_ref::<ArchMismatch>()
        .expect("the refusal must be an ArchMismatch");
    assert_eq!(mismatch.device_arch, "arm64");
    assert_eq!(mismatch.image_arch, "amd64");
    assert_eq!(
        inner.ship_count(),
        before,
        "the refused sync must not have shipped anything"
    );
}
