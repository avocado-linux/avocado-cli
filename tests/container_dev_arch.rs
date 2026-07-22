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

use avocado_cli::utils::container_dev::engine::TagEvent;
use avocado_cli::utils::container_dev::watcher::arch_guard::{
    check_arch, ArchGuardSyncer, ArchMismatch, DeviceArch, DeviceArchBook, HelloArchBook,
    ImageArchProbe,
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
