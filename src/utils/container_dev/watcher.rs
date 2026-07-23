//! Engine-driver watcher + sync orchestration (design D1, D9; task 4.2).
//!
//! On a watched image *tag* event (streamed by [`super::engine`] over the engine
//! CLI subprocess), the watcher syncs the changed layers to the device then
//! notifies it over the control WS. Three behaviors are load-bearing:
//!
//! 1. **PUSH vs INGEST is chosen by EXPLICIT host-topology detection, never
//!    emergent** (design D1). PUSH is O(delta) — re-tag + `push` into the
//!    embedded registry, so the engine's pull protocol transfers only the
//!    changed layers. INGEST is O(full image) — a `docker-daemon:` style export
//!    — and is the fallback ONLY where PUSH is unreachable. The selector reads
//!    [`is_docker_desktop`]/[`is_vm_routing_active`] (the `avocado deploy`
//!    precedent): the avocado-vm and native Linux take PUSH; Docker-Desktop /
//!    podman-machine WITHOUT the VM take INGEST. Per D1's note (L-A), a
//!    podman-machine is invisible to both selectors, so it lands in the INGEST
//!    bucket by virtue of `is_docker_desktop()` being true on macOS — the
//!    correct outcome, stated explicitly rather than left implicit.
//!
//! 2. **Rapid rebuilds are debounced (300 ms).** A burst of tag events collapses
//!    to a single sync of the latest tag.
//!
//! 3. **A supersede cancels an in-flight push.** A new tag event arriving while a
//!    push is still running drops (cancels) that push and starts fresh. Because
//!    control rides its own WS (design D9), the cancel is not blocked behind a
//!    bulk transfer — it is a plain future-drop on the orchestration task.
//!
//! Notifying the device is a seam ([`Notifier`]): the control WS itself is task
//! 5.1, so this module depends only on the notify contract, never on the socket.
//! Likewise the transfer is a seam ([`Syncer`]) with a concrete engine-backed
//! implementation ([`EngineSyncer`]) that reuses the per-engine write-credential
//! injection from [`super::engine`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::auth::WriteToken;
use super::engine::{EngineDriver, TagEvent, WriteCredential};
use crate::utils::container::{is_docker_desktop, is_vm_routing_active};
use crate::utils::output::{print_warning, OutputLevel};

/// Debounce window for coalescing rapid rebuilds (design task 4.2).
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// How the host transfers a rebuilt image's layers to the device.
///
/// The choice is made by EXPLICIT topology detection ([`HostTopology::sync_mode`]),
/// never emergent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// O(delta): re-tag + `push` into the embedded registry so the device's pull
    /// transfers only the changed layers. The native-Linux and avocado-vm path.
    Push,
    /// O(full image): a `docker-daemon:` style export. The Docker-Desktop /
    /// podman-machine-without-VM fallback ONLY — never chosen on a PUSH-capable
    /// endpoint.
    Ingest,
}

/// The host topology inputs that select PUSH vs INGEST (design D1).
///
/// The two fields mirror the `avocado deploy` detectors so the selection is an
/// explicit function of DETECTED topology, not emergent behavior. Tests drive
/// the selector by constructing this directly; [`HostTopology::detect`] wires
/// the real host detectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTopology {
    /// True on macOS/Windows — a Docker-Desktop or podman-machine style host
    /// whose engine runs in a Linux VM ([`is_docker_desktop`]).
    pub docker_desktop: bool,
    /// True iff `DOCKER_HOST` points at the avocado-vm's forwarded socket, i.e.
    /// the push will execute inside the avocado-vm ([`is_vm_routing_active`]).
    pub vm_routing: bool,
}

impl HostTopology {
    /// Detect the host topology from the real `avocado deploy` selectors.
    pub fn detect() -> Self {
        Self {
            docker_desktop: is_docker_desktop(),
            vm_routing: is_vm_routing_active(),
        }
    }

    /// Select the sync mode from the detected topology (design D1).
    ///
    /// - avocado-vm active (`vm_routing`) -> PUSH (authenticated HTTPS push into
    ///   the routable write listener; the macOS fast path).
    /// - Docker-Desktop / podman-machine WITHOUT the VM -> INGEST (PUSH is
    ///   unreachable: the engine lives in a VM whose loopback is not the host's).
    /// - native Linux -> PUSH (loopback push, the common case).
    ///
    /// `vm_routing` is checked first so a macOS host WITH the avocado-vm routed
    /// takes the PUSH fast path even though `docker_desktop` is also true.
    pub fn sync_mode(&self) -> SyncMode {
        if self.vm_routing {
            SyncMode::Push
        } else if self.docker_desktop {
            SyncMode::Ingest
        } else {
            SyncMode::Push
        }
    }
}

/// The device-notify seam (design D9): the control WS is task 5.1, so the
/// watcher depends only on this contract, never on the socket.
///
/// The returned future is boxed and `Send` so the watcher can be spawned on the
/// multi-threaded runtime without an unstable return-type-notation Send bound.
pub trait Notifier: Send + Sync {
    /// Notify the device that `event`'s image/tag/digest is now available to
    /// pull. Realized over the control WS by task 5.1.
    fn notify<'a>(
        &'a self,
        event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// The layer-transfer seam: PUSH (O(delta)) or INGEST (O(full image)).
///
/// The concrete host implementation is [`EngineSyncer`]; tests substitute a
/// recording double so the watcher's debounce/supersede orchestration is
/// asserted without a real engine or registry.
pub trait Syncer: Send + Sync {
    /// Transfer `event`'s image to the embedded registry using `mode`.
    fn sync<'a>(
        &'a self,
        mode: SyncMode,
        event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// Drive the watcher: consume tag events from `rx`, debounce, sync with `mode`,
/// then notify — superseding an in-flight sync when a newer event arrives.
///
/// The loop runs until the event channel closes (all senders dropped, e.g. on
/// `down`): a pending debounce or an in-flight sync completes first, then the
/// loop exits. Sync/notify errors are surfaced as warnings and do not abort the
/// watcher — a later rebuild retries.
pub async fn run_watcher(
    mut rx: mpsc::Receiver<TagEvent>,
    mode: SyncMode,
    syncer: Arc<dyn Syncer>,
    notifier: Arc<dyn Notifier>,
    debounce: Duration,
) {
    // An event carried over from a supersede that cancelled the previous sync.
    let mut pending: Option<TagEvent> = None;
    // Set once the channel closes; we then stop listening for supersedes and let
    // the current work finish rather than treating close as a cancel.
    let mut closed = false;

    loop {
        // Acquire the event to work on: a carried-over supersede, else the next
        // from the channel.
        let first = match pending.take() {
            Some(e) => e,
            None => {
                if closed {
                    return;
                }
                match rx.recv().await {
                    Some(e) => e,
                    None => return,
                }
            }
        };

        // Debounce: keep only the latest event arriving within `debounce`.
        let mut latest = first;
        if !closed {
            loop {
                tokio::select! {
                    _ = sleep(debounce) => break,
                    got = rx.recv() => match got {
                        Some(e) => latest = e,   // supersede within the window
                        None => { closed = true; break; }
                    }
                }
            }
        }

        // Sync + notify. A superseding event (Some) cancels the in-flight work by
        // dropping its future; a channel close (None) stops supersede-listening
        // so the current work runs to completion.
        if closed {
            do_sync_and_notify(mode, syncer.as_ref(), notifier.as_ref(), &latest).await;
        } else {
            let work = do_sync_and_notify(mode, syncer.as_ref(), notifier.as_ref(), &latest);
            tokio::pin!(work);
            loop {
                tokio::select! {
                    () = &mut work => break,
                    got = rx.recv(), if !closed => match got {
                        // Supersede: dropping `work` cancels the in-flight push.
                        Some(e) => { pending = Some(e); break; }
                        // Channel closed mid-work: stop listening, finish `work`.
                        None => { closed = true; }
                    }
                }
            }
        }
    }
}

/// Run one sync + notify, surfacing (but not propagating) failures.
async fn do_sync_and_notify(
    mode: SyncMode,
    syncer: &dyn Syncer,
    notifier: &dyn Notifier,
    event: &TagEvent,
) {
    if let Err(e) = syncer.sync(mode, event).await {
        print_warning(
            &format!("container dev: sync of `{}` failed: {e:#}", event.image),
            OutputLevel::Normal,
        );
        return;
    }
    if let Err(e) = notifier.notify(event).await {
        print_warning(
            &format!("container dev: notify for `{}` failed: {e:#}", event.image),
            OutputLevel::Normal,
        );
    }
}

/// The PUSH command plan (O(delta)): re-tag the local image onto the embedded
/// registry and push it, injecting the host-only write credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    /// The registry-qualified target ref the image is re-tagged to and pushed.
    pub target_ref: String,
    /// `<engine> tag <local> <target>`.
    pub tag_argv: Vec<String>,
    /// `<engine> push <target>` (credential injected at execution).
    pub push_argv: Vec<String>,
    /// How the write credential is injected on the push (design D2/A10).
    pub credential: WriteCredential,
}

/// The INGEST command plan (O(full image)): a full-image `save` export, the
/// fallback used only where PUSH is unreachable. It never targets the embedded
/// registry — that is the whole point of the O(full-image) cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestPlan {
    /// The local image exported wholesale.
    pub source_ref: String,
    /// `<engine> save <image>` — exports every layer, not just the delta.
    pub export_argv: Vec<String>,
}

/// Strip a leading registry component (`localhost/…`, `host.tld/…`,
/// `host:port/…`) from an image reference, leaving `repo[:tag]`.
///
/// podman qualifies a local ref as `localhost/my-app:dev`; docker leaves it
/// `my-app:dev`. Both normalize to `my-app:dev` so the embedded-registry target
/// is `<registry>/my-app:dev` regardless of engine.
fn repo_and_tag(image: &str) -> String {
    match image.split_once('/') {
        Some((first, rest))
            if first == "localhost" || first.contains('.') || first.contains(':') =>
        {
            rest.to_string()
        }
        _ => image.to_string(),
    }
}

/// Build the PUSH plan for `event` targeting `registry` (`host:port`).
pub fn build_push_plan(
    driver: &dyn EngineDriver,
    registry: &str,
    event: &TagEvent,
    token: &WriteToken,
) -> PushPlan {
    let target_ref = format!("{registry}/{}", repo_and_tag(&event.image));
    let tag_argv = vec!["tag".to_string(), event.image.clone(), target_ref.clone()];
    let push_argv = vec!["push".to_string(), target_ref.clone()];
    let credential = driver.write_credential(registry, token);
    PushPlan {
        target_ref,
        tag_argv,
        push_argv,
        credential,
    }
}

/// Build the INGEST plan for `event`: a full-image export.
pub fn build_ingest_plan(event: &TagEvent) -> IngestPlan {
    IngestPlan {
        source_ref: event.image.clone(),
        export_argv: vec!["save".to_string(), event.image.clone()],
    }
}

/// The concrete host [`Syncer`]: drives the engine CLI to PUSH (delta) or INGEST
/// (full export), reusing the per-engine write-credential injection from
/// [`super::engine`].
///
/// PUSH re-tags the image onto the embedded registry and pushes it with the
/// host-only write token — injected via an ephemeral `DOCKER_CONFIG` (docker) or
/// `--creds` (podman), NEVER a persisted `docker login` against the user's real
/// config (design M-E). INGEST is the O(full-image) fallback export.
pub struct EngineSyncer {
    driver: Box<dyn EngineDriver>,
    /// The write listener `host:port` — byte-identical to the tag host so docker
    /// attaches the injected credential (H-3).
    registry: String,
    write_token: WriteToken,
    /// Per-project dir the ephemeral `DOCKER_CONFIG` and export tar live under.
    project_dir: PathBuf,
}

impl EngineSyncer {
    /// Construct a syncer for `driver` pushing to `registry` under `project_dir`.
    pub fn new(
        driver: Box<dyn EngineDriver>,
        registry: impl Into<String>,
        write_token: WriteToken,
        project_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            driver,
            registry: registry.into(),
            write_token,
            project_dir: project_dir.into(),
        }
    }

    async fn push(&self, event: &TagEvent) -> Result<()> {
        let plan = build_push_plan(
            self.driver.as_ref(),
            &self.registry,
            event,
            &self.write_token,
        );
        let binary = self.driver.binary();

        run_engine(binary, &plan.tag_argv, None).await?;

        match &plan.credential {
            WriteCredential::DockerConfigEnv {
                registry,
                username,
                token,
            } => {
                // Write an ephemeral DOCKER_CONFIG whose auths key is byte-identical
                // to the tagged registry host:port (H-3), 0600, under the per-project
                // dir — deleted when `dir` drops after the push. NEVER merged into
                // the user's real ~/.docker/config.json (M-E).
                let dir = tempfile::Builder::new()
                    .prefix("docker-config-")
                    .tempdir_in(&self.project_dir)
                    .context("creating ephemeral DOCKER_CONFIG dir")?;
                write_docker_config(dir.path(), registry, username, token)?;
                run_engine(binary, &plan.push_argv, Some(("DOCKER_CONFIG", dir.path()))).await?;
            }
            WriteCredential::PodmanCreds { username, token } => {
                // podman takes the credential per-invocation on argv (design A10).
                let argv = vec![
                    "push".to_string(),
                    "--creds".to_string(),
                    format!("{username}:{token}"),
                    plan.target_ref.clone(),
                ];
                run_engine(binary, &argv, None).await?;
            }
        }
        Ok(())
    }

    async fn ingest(&self, event: &TagEvent) -> Result<()> {
        let plan = build_ingest_plan(event);
        let tar = self.project_dir.join("ingest.tar");
        // A full-image export: `save -o <tar> <image>`, O(full image) by design —
        // the fallback where PUSH is unreachable, never on a PUSH-capable endpoint.
        let mut argv = plan.export_argv.clone();
        argv.insert(1, "-o".to_string());
        argv.insert(2, tar.to_string_lossy().into_owned());
        run_engine(self.driver.binary(), &argv, None).await
    }
}

impl Syncer for EngineSyncer {
    fn sync<'a>(
        &'a self,
        mode: SyncMode,
        event: &'a TagEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match mode {
                SyncMode::Push => self.push(event).await,
                SyncMode::Ingest => self.ingest(event).await,
            }
        })
    }
}

/// Write an ephemeral docker `config.json` with a single `auths` entry keyed to
/// `registry`, mode 0600.
fn write_docker_config(
    dir: &std::path::Path,
    registry: &str,
    username: &str,
    token: &str,
) -> Result<()> {
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
    let body = serde_json::json!({ "auths": { registry: { "auth": auth } } });
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&body)?)
        .with_context(|| format!("writing ephemeral docker config to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("chmod 0600 on ephemeral docker config")?;
    }
    Ok(())
}

/// Run `<binary> <argv...>` with an optional single env override, failing on a
/// non-zero exit.
async fn run_engine(
    binary: &str,
    argv: &[String],
    env: Option<(&str, &std::path::Path)>,
) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.args(argv);
    if let Some((key, val)) = env {
        cmd.env(key, val);
    }
    let status = cmd
        .status()
        .await
        .with_context(|| format!("running `{binary} {}`", argv.join(" ")))?;
    if !status.success() {
        anyhow::bail!("`{binary} {}` exited with {status}", argv.join(" "));
    }
    Ok(())
}

/// Cross-arch guard (task 4.3, design "cross-arch refusal").
///
/// A container image built for one CPU architecture cannot run on a device of
/// another, so syncing an amd64 image to an arm64 device is a silent
/// wrong-arch delivery the device engine would fail (or worse, a manifest that
/// pulls but never runs). The guard sits IN the sync path as a [`Syncer`]
/// decorator: it probes the image's platform architecture, compares it against
/// every connected device's reported `hello.arch`, and REFUSES the sync (with
/// actionable buildx guidance) before the wrapped syncer pushes or exports
/// anything. Because a refused sync returns `Err`, [`do_sync_and_notify`] also
/// skips the device notify — so a mismatch never reaches push OR notify.
///
/// The device architecture comes from the control-WS `hello` frame's `arch`
/// field (task 5.1 records it into a [`DeviceArchBook`]); the guard only reads
/// the snapshot, so it does not depend on the WS implementation.
pub mod arch_guard {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use anyhow::{Context, Result};
    use tokio::process::Command;

    use super::super::engine::{EngineDriver, TagEvent};
    use super::{SyncMode, Syncer};

    /// A CPU architecture canonicalized to the OCI/GOARCH spelling.
    ///
    /// A device reports `hello.arch` in `uname -m` form (`x86_64`, `aarch64`),
    /// while an image's platform architecture is GOARCH (`amd64`, `arm64`).
    /// Normalizing both to one spelling lets them compare equal. An unrecognized
    /// value is lowercased and compared verbatim, so two identical unknown
    /// arches still match rather than spuriously refusing.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DeviceArch(String);

    impl DeviceArch {
        /// Canonicalize a raw arch string from an image platform or a device
        /// `hello.arch`.
        pub fn parse(raw: &str) -> Self {
            let lowered = raw.trim().to_ascii_lowercase();
            let canon = match lowered.as_str() {
                "x86_64" | "amd64" | "x64" => "amd64",
                "aarch64" | "arm64" | "arm64v8" => "arm64",
                "armv7l" | "armv6l" | "armhf" | "arm" | "arm32v7" => "arm",
                "i386" | "i486" | "i586" | "i686" | "386" | "x86" => "386",
                "riscv64" => "riscv64",
                "ppc64le" => "ppc64le",
                "s390x" => "s390x",
                _ => lowered.as_str(),
            };
            DeviceArch(canon.to_string())
        }

        /// The canonical GOARCH string (`amd64`, `arm64`, …).
        pub fn as_str(&self) -> &str {
            &self.0
        }
    }

    /// A refused cross-arch sync: the image platform does not match a device.
    ///
    /// The `Display` is the user-facing refusal and carries buildx guidance that
    /// names the device's target platform, so a developer can rebuild for the
    /// right architecture without guessing the flag.
    #[derive(Debug, thiserror::Error)]
    #[error(
        "refusing to sync image `{image}` (platform `{image_arch}`) to a device reporting arch \
         `{device_arch}`: this would ship a wrong-architecture image the device cannot run. \
         Rebuild for the device platform with buildx, e.g.:\n    \
         docker buildx build --platform linux/{device_arch} -t {image} .\n  \
         then re-run the sync."
    )]
    pub struct ArchMismatch {
        /// The image reference that was refused.
        pub image: String,
        /// The image's platform architecture (canonical GOARCH).
        pub image_arch: String,
        /// The mismatched device's reported architecture (canonical GOARCH).
        pub device_arch: String,
    }

    /// Refuse the sync unless `image_arch` matches EVERY connected device.
    ///
    /// A single mismatched device is a refusal — we never ship a wrong-arch
    /// image to any device in a fleet. With no connected devices there is
    /// nothing to mismatch, so the sync is allowed (it simply reaches no one).
    pub fn check_arch(
        image: &str,
        image_arch: &DeviceArch,
        device_arches: &[DeviceArch],
    ) -> Result<(), ArchMismatch> {
        for dev in device_arches {
            if dev != image_arch {
                return Err(ArchMismatch {
                    image: image.to_string(),
                    image_arch: image_arch.as_str().to_string(),
                    device_arch: dev.as_str().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Probe an image's platform architecture (task 4.3 seam).
    ///
    /// The concrete host implementation is [`EngineArchProbe`]; tests substitute
    /// a fixed double so the guard's refusal logic is asserted without a real
    /// engine.
    pub trait ImageArchProbe: Send + Sync {
        /// Report the platform architecture of `event`'s image.
        fn image_arch<'a>(
            &'a self,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<DeviceArch>> + Send + 'a>>;
    }

    /// A snapshot of the architectures of currently-connected devices, sourced
    /// from their `hello.arch` control frames (task 5.1 populates it).
    pub trait DeviceArchBook: Send + Sync {
        /// The architectures of every device currently known from a `hello`.
        fn device_arches(&self) -> Vec<DeviceArch>;
    }

    /// In-memory [`DeviceArchBook`] keyed by device id, populated from `hello`
    /// frames. A reconnecting device overwrites its prior entry, so a snapshot
    /// never double-counts one device.
    #[derive(Default, Clone)]
    pub struct HelloArchBook {
        by_device: Arc<Mutex<BTreeMap<String, DeviceArch>>>,
    }

    impl HelloArchBook {
        /// A book with no devices recorded yet.
        pub fn new() -> Self {
            Self::default()
        }

        /// Record a device's `hello.arch` (task 5.1 calls this on a hello frame).
        pub fn record_hello(&self, device_id: &str, arch: &str) {
            self.by_device
                .lock()
                .unwrap()
                .insert(device_id.to_string(), DeviceArch::parse(arch));
        }
    }

    impl DeviceArchBook for HelloArchBook {
        fn device_arches(&self) -> Vec<DeviceArch> {
            self.by_device.lock().unwrap().values().cloned().collect()
        }
    }

    /// Probe the image architecture via `<engine> image inspect --format
    /// {{.Architecture}} <ref>` — the engine CLI, consistent with the rest of
    /// the driver (no API socket).
    pub struct EngineArchProbe {
        binary: &'static str,
    }

    impl EngineArchProbe {
        /// Build a probe driving `driver`'s engine CLI binary.
        pub fn new(driver: &dyn EngineDriver) -> Self {
            Self {
                binary: driver.binary(),
            }
        }
    }

    impl ImageArchProbe for EngineArchProbe {
        fn image_arch<'a>(
            &'a self,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<DeviceArch>> + Send + 'a>> {
            Box::pin(async move {
                let output = Command::new(self.binary)
                    .args([
                        "image",
                        "inspect",
                        "--format",
                        "{{.Architecture}}",
                        &event.image,
                    ])
                    .output()
                    .await
                    .with_context(|| {
                        format!("running `{} image inspect {}`", self.binary, event.image)
                    })?;
                if !output.status.success() {
                    anyhow::bail!(
                        "`{} image inspect {}` failed: {}",
                        self.binary,
                        event.image,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                let arch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if arch.is_empty() {
                    anyhow::bail!(
                        "`{} image inspect {}` reported an empty architecture",
                        self.binary,
                        event.image
                    );
                }
                Ok(DeviceArch::parse(&arch))
            })
        }
    }

    /// A [`Syncer`] decorator that refuses a cross-arch sync BEFORE delegating.
    ///
    /// It probes the image architecture, compares it against the device book,
    /// and returns [`ArchMismatch`] on a mismatch — so `inner` (the real
    /// PUSH/INGEST syncer) is never invoked and the watcher skips notify.
    pub struct ArchGuardSyncer {
        inner: Arc<dyn Syncer>,
        probe: Arc<dyn ImageArchProbe>,
        devices: Arc<dyn DeviceArchBook>,
    }

    impl ArchGuardSyncer {
        /// Wrap `inner`, guarding it with `probe` (image arch) and `devices`
        /// (connected-device arches).
        pub fn new(
            inner: Arc<dyn Syncer>,
            probe: Arc<dyn ImageArchProbe>,
            devices: Arc<dyn DeviceArchBook>,
        ) -> Self {
            Self {
                inner,
                probe,
                devices,
            }
        }
    }

    impl Syncer for ArchGuardSyncer {
        fn sync<'a>(
            &'a self,
            mode: SyncMode,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let image_arch = self.probe.image_arch(event).await?;
                let device_arches = self.devices.device_arches();
                // A mismatch refuses here, before the wrapped syncer pushes or
                // exports anything.
                check_arch(&event.image, &image_arch, &device_arches)?;
                self.inner.sync(mode, event).await
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::super::{do_sync_and_notify, Notifier};

        fn ev(image: &str) -> TagEvent {
            TagEvent {
                image: image.to_string(),
                image_id: None,
            }
        }

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

        #[derive(Default)]
        struct CountingSyncer {
            calls: AtomicUsize,
        }
        impl Syncer for CountingSyncer {
            fn sync<'a>(
                &'a self,
                _mode: SyncMode,
                _event: &'a TagEvent,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        #[derive(Default)]
        struct CountingNotifier {
            calls: AtomicUsize,
        }
        impl Notifier for CountingNotifier {
            fn notify<'a>(
                &'a self,
                _event: &'a TagEvent,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
        }

        // ---- arch normalization ----

        #[test]
        fn parse_canonicalizes_uname_and_goarch_spellings() {
            assert_eq!(DeviceArch::parse("x86_64"), DeviceArch::parse("amd64"));
            assert_eq!(DeviceArch::parse("aarch64"), DeviceArch::parse("arm64"));
            assert_eq!(DeviceArch::parse("armv7l"), DeviceArch::parse("arm"));
            assert_eq!(DeviceArch::parse("AMD64"), DeviceArch::parse("amd64"));
            assert_ne!(DeviceArch::parse("amd64"), DeviceArch::parse("arm64"));
        }

        // ---- pure check_arch logic ----

        #[test]
        fn a_matching_arch_passes_the_check() {
            // uname `aarch64` device vs a GOARCH `arm64` image: equal after
            // normalization.
            assert!(check_arch(
                "app:dev",
                &DeviceArch::parse("arm64"),
                &[DeviceArch::parse("aarch64")]
            )
            .is_ok());
        }

        #[test]
        fn an_amd64_image_is_refused_on_an_arm64_device() {
            let err = check_arch(
                "my-app:dev",
                &DeviceArch::parse("amd64"),
                &[DeviceArch::parse("aarch64")],
            )
            .expect_err("an amd64 image must be refused for an arm64 device");
            assert_eq!(err.image_arch, "amd64");
            assert_eq!(err.device_arch, "arm64");
        }

        #[test]
        fn any_single_mismatched_device_refuses_the_whole_sync() {
            // A fleet with one arm64 and one amd64 device: an amd64 image cannot
            // run on the arm64 one, so the whole sync is refused.
            let devices = [DeviceArch::parse("arm64"), DeviceArch::parse("amd64")];
            let err = check_arch("app:dev", &DeviceArch::parse("amd64"), &devices)
                .expect_err("a mismatch on any device refuses the sync");
            assert_eq!(err.device_arch, "arm64");
        }

        #[test]
        fn no_connected_devices_is_not_a_mismatch() {
            assert!(check_arch("app:dev", &DeviceArch::parse("amd64"), &[]).is_ok());
        }

        #[test]
        fn the_refusal_names_buildx_and_the_device_target_platform() {
            let err = check_arch(
                "my-app:dev",
                &DeviceArch::parse("amd64"),
                &[DeviceArch::parse("aarch64")],
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("buildx"),
                "the refusal must give buildx guidance, not a bare error: {msg}"
            );
            assert!(
                msg.contains("linux/arm64"),
                "the refusal must name the device's target platform: {msg}"
            );
        }

        // ---- the guard sits in the sync path (via do_sync_and_notify) ----

        #[tokio::test]
        async fn an_amd64_image_is_refused_before_push_or_notify_on_an_arm64_device() {
            let inner = Arc::new(CountingSyncer::default());
            let notifier = CountingNotifier::default();
            let book = HelloArchBook::new();
            book.record_hello("dev-1", "aarch64"); // device reports arm64

            let guard = ArchGuardSyncer::new(
                inner.clone() as Arc<dyn Syncer>,
                Arc::new(FixedProbe("amd64")),
                Arc::new(book),
            );

            do_sync_and_notify(SyncMode::Push, &guard, &notifier, &ev("my-app:dev")).await;

            assert_eq!(
                inner.calls.load(Ordering::SeqCst),
                0,
                "a cross-arch image must be refused before the push runs"
            );
            assert_eq!(
                notifier.calls.load(Ordering::SeqCst),
                0,
                "a refused sync must not notify the device"
            );
        }

        #[tokio::test]
        async fn a_matching_arch_image_proceeds_to_push_and_notify() {
            let inner = Arc::new(CountingSyncer::default());
            let notifier = CountingNotifier::default();
            let book = HelloArchBook::new();
            book.record_hello("dev-1", "x86_64"); // amd64 device

            let guard = ArchGuardSyncer::new(
                inner.clone() as Arc<dyn Syncer>,
                Arc::new(FixedProbe("amd64")),
                Arc::new(book),
            );

            do_sync_and_notify(SyncMode::Push, &guard, &notifier, &ev("my-app:dev")).await;

            assert_eq!(
                inner.calls.load(Ordering::SeqCst),
                1,
                "a matching-arch image is pushed"
            );
            assert_eq!(
                notifier.calls.load(Ordering::SeqCst),
                1,
                "a matching-arch image notifies the device after the push"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::Notify;
    use tokio::time::{timeout, Duration};

    use crate::utils::container_dev::auth::WRITE_USERNAME;
    use crate::utils::container_dev::engine::{DockerDriver, PodmanDriver};

    fn ev(image: &str) -> TagEvent {
        TagEvent {
            image: image.to_string(),
            image_id: Some(format!("sha256:{image}")),
        }
    }

    // ---- topology selection: explicit detection, not emergent (D1) ----

    #[test]
    fn native_linux_selects_push() {
        let topo = HostTopology {
            docker_desktop: false,
            vm_routing: false,
        };
        assert_eq!(topo.sync_mode(), SyncMode::Push);
    }

    #[test]
    fn avocado_vm_selects_push_even_on_a_docker_desktop_host() {
        // macOS with the avocado-vm routed: docker_desktop is true, but the VM
        // push fast path wins.
        let topo = HostTopology {
            docker_desktop: true,
            vm_routing: true,
        };
        assert_eq!(topo.sync_mode(), SyncMode::Push);
    }

    #[test]
    fn docker_desktop_without_vm_selects_ingest() {
        // Docker-Desktop / podman-machine with no avocado-vm: PUSH is unreachable,
        // so the topology-detected fallback is INGEST — not emergent behavior.
        let topo = HostTopology {
            docker_desktop: true,
            vm_routing: false,
        };
        assert_eq!(topo.sync_mode(), SyncMode::Ingest);
    }

    // ---- PUSH is delta into the registry; INGEST is a full local export ----

    #[test]
    fn push_plan_retags_onto_the_registry_and_injects_the_write_credential() {
        let plan = build_push_plan(
            &DockerDriver,
            "127.0.0.1:5599",
            &ev("my-app:dev"),
            &WriteToken::new("wtok"),
        );
        assert_eq!(plan.target_ref, "127.0.0.1:5599/my-app:dev");
        assert_eq!(
            plan.tag_argv,
            vec!["tag", "my-app:dev", "127.0.0.1:5599/my-app:dev"]
        );
        assert_eq!(plan.push_argv, vec!["push", "127.0.0.1:5599/my-app:dev"]);
        // The delta path pushes to the embedded registry with the host-only write
        // token (Basic, via an ephemeral DOCKER_CONFIG keyed to the registry).
        match plan.credential {
            WriteCredential::DockerConfigEnv {
                registry,
                username,
                token,
            } => {
                assert_eq!(registry, "127.0.0.1:5599");
                assert_eq!(username, WRITE_USERNAME);
                assert_eq!(token, "wtok");
            }
            other => panic!("expected an ephemeral DOCKER_CONFIG credential, got {other:?}"),
        }
    }

    #[test]
    fn push_plan_strips_a_podman_localhost_qualifier() {
        let plan = build_push_plan(
            &PodmanDriver,
            "127.0.0.1:5599",
            &ev("localhost/my-app:dev"),
            &WriteToken::new("wtok"),
        );
        // The registry qualifier is stripped so the target is the same repo:tag as
        // the docker case, not `127.0.0.1:5599/localhost/my-app:dev`.
        assert_eq!(plan.target_ref, "127.0.0.1:5599/my-app:dev");
    }

    #[test]
    fn ingest_plan_is_a_full_image_export_not_a_registry_push() {
        let plan = build_ingest_plan(&ev("my-app:dev"));
        assert_eq!(plan.source_ref, "my-app:dev");
        assert_eq!(plan.export_argv, vec!["save", "my-app:dev"]);
        // INGEST must never target the embedded registry — that is the O(full
        // image) fallback, distinct from the delta PUSH.
        assert!(
            !plan
                .export_argv
                .iter()
                .any(|a| a.contains(':') && a.contains('/')),
            "INGEST is a local export, it must not push to a registry endpoint: {:?}",
            plan.export_argv
        );
        assert_eq!(plan.export_argv[0], "save");
    }

    // ---- watcher orchestration: recording doubles for the seams ----

    #[derive(Default)]
    struct Recorder {
        /// Ordered log across both seams: `sync-start:<img>`, `sync-done:<img>`,
        /// `notify:<img>`.
        log: Mutex<Vec<String>>,
        /// Images whose sync started.
        started: Mutex<Vec<String>>,
        /// Images whose sync ran to completion (i.e. was not cancelled).
        completed: Mutex<Vec<String>>,
        /// Fired after a sync records its start, so a test can send a superseding
        /// event only once a push is genuinely in flight.
        started_signal: Notify,
        /// An image whose sync blocks (models a slow, cancellable push).
        slow_image: Mutex<Option<String>>,
    }

    impl Recorder {
        fn arc() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn set_slow(&self, image: &str) {
            *self.slow_image.lock().unwrap() = Some(image.to_string());
        }
    }

    impl Syncer for Recorder {
        fn sync<'a>(
            &'a self,
            mode: SyncMode,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("sync-start:{}:{:?}", event.image, mode));
                self.started.lock().unwrap().push(event.image.clone());
                self.started_signal.notify_one();
                let slow = self.slow_image.lock().unwrap().clone();
                if slow.as_deref() == Some(event.image.as_str()) {
                    // Block long enough that a supersede cancels this future.
                    sleep(Duration::from_secs(30)).await;
                }
                self.completed.lock().unwrap().push(event.image.clone());
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("sync-done:{}", event.image));
                Ok(())
            })
        }
    }

    impl Notifier for Recorder {
        fn notify<'a>(
            &'a self,
            event: &'a TagEvent,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("notify:{}", event.image));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn a_tag_event_pushes_then_notifies() {
        let rec = Recorder::arc();
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(run_watcher(
            rx,
            SyncMode::Push,
            rec.clone() as Arc<dyn Syncer>,
            rec.clone() as Arc<dyn Notifier>,
            DEBOUNCE,
        ));

        tx.send(ev("my-app:dev")).await.unwrap();
        // Give the debounce window + a slack margin to settle and run the work.
        sleep(DEBOUNCE + Duration::from_millis(200)).await;
        drop(tx);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("watcher exits after the channel closes")
            .unwrap();

        // The sync ran once with the PUSH mode, then the notify followed it.
        let log = rec.log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "sync-start:my-app:dev:Push".to_string(),
                "sync-done:my-app:dev".to_string(),
                "notify:my-app:dev".to_string(),
            ],
            "a rebuild must push (delta) then notify, in that order"
        );
    }

    #[tokio::test]
    async fn a_second_event_within_the_debounce_window_supersedes_the_first() {
        let rec = Recorder::arc();
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(run_watcher(
            rx,
            SyncMode::Push,
            rec.clone() as Arc<dyn Syncer>,
            rec.clone() as Arc<dyn Notifier>,
            DEBOUNCE,
        ));

        // Two events well inside the 300 ms window.
        tx.send(ev("v1")).await.unwrap();
        sleep(Duration::from_millis(50)).await;
        tx.send(ev("v2")).await.unwrap();

        sleep(DEBOUNCE + Duration::from_millis(200)).await;
        drop(tx);
        timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();

        // Only the latest event synced; v1 was superseded and never pushed.
        let started = rec.started.lock().unwrap().clone();
        assert_eq!(
            started,
            vec!["v2".to_string()],
            "the burst coalesces to the latest tag"
        );
        let log = rec.log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "sync-start:v2:Push".to_string(),
                "sync-done:v2".to_string(),
                "notify:v2".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_superseding_event_cancels_an_in_flight_push() {
        let rec = Recorder::arc();
        rec.set_slow("v1"); // v1's push blocks until cancelled
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(run_watcher(
            rx,
            SyncMode::Push,
            rec.clone() as Arc<dyn Syncer>,
            rec.clone() as Arc<dyn Notifier>,
            DEBOUNCE,
        ));

        // v1 settles through the debounce and starts a (blocking) push.
        tx.send(ev("v1")).await.unwrap();
        rec.started_signal.notified().await;

        // Now supersede with v2 while v1's push is in flight.
        tx.send(ev("v2")).await.unwrap();

        sleep(DEBOUNCE + Duration::from_millis(200)).await;
        drop(tx);
        timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();

        let started = rec.started.lock().unwrap().clone();
        let completed = rec.completed.lock().unwrap().clone();
        // Both pushes started, but v1's in-flight push was cancelled by the
        // supersede: only v2 completes and notifies.
        assert!(started.contains(&"v1".to_string()), "v1's push started");
        assert!(started.contains(&"v2".to_string()), "v2's push started");
        assert_eq!(
            completed,
            vec!["v2".to_string()],
            "the superseded v1 push was cancelled before completion"
        );
        let log = rec.log.lock().unwrap().clone();
        assert!(
            log.contains(&"notify:v2".to_string()),
            "v2 notifies after its push"
        );
        assert!(
            !log.contains(&"notify:v1".to_string()),
            "the cancelled v1 push must not notify"
        );
        assert!(
            !log.contains(&"sync-done:v1".to_string()),
            "the cancelled v1 push must not complete"
        );
    }

    #[test]
    fn debounce_default_is_300ms() {
        assert_eq!(DEBOUNCE, Duration::from_millis(300));
    }
}
