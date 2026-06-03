//! High-level `start` / `stop` / `status` orchestration.
//!
//! Glue layer above [`manifest`], [`qemu`], [`qmp`], [`qga`], [`boot_sync`],
//! [`ssh`] and [`state`]. Knows nothing about clap; the `commands/vm/`
//! subcommands call into these functions.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::manifest::Manifest;
use super::qemu::{self, QemuConfig};
#[cfg(unix)]
use super::qmp::QmpClient;
use super::ssh::SshTarget;
use super::state::{self, VmPaths};

// POSIX signal numbers, declared here so call sites compile on Windows
// where `libc::SIGTERM` / `libc::SIGKILL` are not defined. `send_signal`
// itself is `#[cfg(unix)]` and is a no-op elsewhere.
const SIGTERM: libc::c_int = 15;
const SIGKILL: libc::c_int = 9;

/// Best-effort one-way notification to Avocado.app (macOS only). All call
/// sites are fire-and-forget: never blocks the CLI on the desktop's
/// responsiveness, silently no-ops when the desktop isn't running or
/// installed. The desktop has a pidfile reconciler as a backstop, so a
/// dropped notification self-heals within ~2 s.
fn notify_desktop(method: &str, params: serde_json::Value) {
    #[cfg(target_os = "macos")]
    super::client::notify(method, params);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (method, params);
    }
}

/// Knobs the `avocado vm start` command resolves from args + env.
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// Directory containing the `direct` profile output (manifest.json + artifacts).
    pub vm_source: PathBuf,
    /// Memory in MiB. `None` → read from `runtime.memory_mib` in
    /// ~/.avocado/vm/config.yaml, else fall back to [`DEFAULT_MEMORY_MIB`].
    /// When `Some(_)`, the value is also persisted back to the config so
    /// the next flag-less `vm start` (and Avocado.app) sees it.
    pub memory_mib: Option<u32>,
    /// vCPU count. Same resolution / persistence rules as [`memory_mib`].
    pub cpus: Option<u32>,
    /// Host port for ssh-into-VM. `None` → pick a free port.
    pub ssh_port: Option<u16>,
    /// Extra kernel cmdline.
    pub cmdline_extra: Option<String>,
    /// Host path exposed to the guest as a 9p workspace share. `None` =>
    /// pick via `share::resolve_workspace` (env / $HOME).
    pub workspace: Option<PathBuf>,
    /// Target size of the persistent var.btrfs (e.g. "50G"). The file is
    /// truncated up to this size on every start (sparse — no disk use
    /// until written), then btrfs is resized to fill it inside the VM.
    /// Shrinking is refused. `None` → default ([`DEFAULT_VAR_SIZE`]).
    pub var_size: Option<String>,
    /// One-shot DNS override for this start only. Wins over the persisted
    /// `network.dns` in `~/.avocado/vm/config.yaml`; the persisted value
    /// is unchanged. `None` → fall through to the persisted config (or
    /// SLIRP's DHCP-supplied 10.0.2.3 if neither is set).
    pub dns_override: Option<Vec<String>>,
}

/// Default target size for the persistent var.btrfs. 50 GiB sparse — large
/// enough for an SDK image + a comfortable working set of containers without
/// being wasteful on disk-pressure-sensitive hosts (the file only consumes
/// what's actually written).
pub const DEFAULT_VAR_SIZE: &str = "50G";

/// Fallback CPU count when neither the `--cpus` flag nor `runtime.cpus` in
/// `~/.avocado/vm/config.yaml` is set.
pub const DEFAULT_CPUS: u32 = 4;

/// Fallback memory size (MiB) when neither the `--memory-mib` flag nor
/// `runtime.memory_mib` in `~/.avocado/vm/config.yaml` is set.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// What the `status` command needs to render.
#[derive(Debug, Clone)]
pub struct VmStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub ssh_port: Option<u16>,
    pub manifest_platform: Option<String>,
    pub manifest_arch: Option<String>,
    pub paths: VmPaths,
}

/// Start the VM. Errors if one is already running. Performs manifest sha256
/// verification, ensures QEMU is on $PATH, ensures the state dir, picks a
/// port, spawns qemu, and waits for the qga handshake.
pub async fn start(opts: StartOptions) -> Result<VmStatus> {
    let paths = VmPaths::resolve()?;
    paths.ensure()?;

    // Reject if already running.
    if let Some(pid) = state::read_pid(&paths)? {
        if state::pid_alive(pid) {
            bail!("avocado-vm already running (pid {pid}); use `avocado vm stop` first");
        }
        // Stale pidfile — clean it up.
        state::cleanup_transient(&paths);
    }

    // Announce intent so the desktop dashboard can flip to "Starting…"
    // before the qemu pid even exists. Backstopped by the pidfile poller
    // in case the notification doesn't get through.
    notify_desktop("vm.notify.starting", serde_json::json!({}));

    // Resolve manifest + verify artifacts
    let artifact_dir = opts.vm_source;
    if !artifact_dir.is_dir() {
        bail!(
            "vm-source path {} is not a directory",
            artifact_dir.display()
        );
    }
    let manifest_path = artifact_dir.join("manifest.json");
    let manifest = Manifest::load(&manifest_path)
        .with_context(|| format!("loading manifest at {}", manifest_path.display()))?;
    manifest
        .verify_all(&artifact_dir)
        .context("artifact sha256 verification failed")?;

    // Record the manifest under ~/.avocado/vm/ for staleness checks later.
    std::fs::copy(&manifest_path, paths.manifest())
        .with_context(|| format!("copying manifest into {}", paths.root.display()))?;

    // Record the artifact dir so Avocado.app (which inherits a sanitized
    // env from LaunchServices on macOS) can find it without AVOCADO_VM_DIR.
    let _ = std::fs::write(
        paths.artifact_dir_file(),
        artifact_dir.display().to_string(),
    );

    qemu::ensure_qemu_available(&manifest)?;
    ensure_ssh_key(&paths)?;

    // Seed the persistent var disk from the artifact's var.btrfs if we don't
    // already have one. The artifact dir stays read-only (sha256s remain
    // valid across reboots); the state-dir copy is the writable disk QEMU
    // attaches. `avocado vm rebuild --reset-data` wipes it back to the seed.
    seed_var_disk(&paths, &manifest, &artifact_dir)?;

    // Grow the var.btrfs file (sparse) to the user-requested size, if
    // larger than current. The matching `btrfs filesystem resize max` runs
    // inside the VM after boot.
    let var_target_bytes = parse_size(opts.var_size.as_deref().unwrap_or(DEFAULT_VAR_SIZE))
        .context("invalid --var-size")?;
    grow_var_file(&paths, var_target_bytes)?;

    let workspace = super::share::resolve_workspace(opts.workspace.as_deref())?;
    super::share::record_workspace(&paths, &workspace)?;

    let ssh_port = match opts.ssh_port {
        Some(p) => p,
        None => qemu::pick_free_port()?,
    };
    state::write_ssh_port(&paths, ssh_port)?;

    // Loopback-only port QEMU's hostfwd binds to. The supervisor
    // listens on the user-facing `ssh_port` and proxies through to
    // this one; downstream callers (vm shell, forward.rs, Avocado.app)
    // only ever see `ssh_port`.
    let internal_ssh_port = qemu::pick_free_port()?;
    std::fs::write(
        paths.internal_ssh_port_file(),
        internal_ssh_port.to_string(),
    )
    .with_context(|| format!("writing {}", paths.internal_ssh_port_file().display()))?;

    // Now that the port is known, write the ssh-config + wire it into
    // ~/.ssh/config. This is required for `DOCKER_HOST=ssh://avocado-vm`
    // to resolve in any subprocess we spawn — Docker's ssh transport reads
    // ~/.ssh/config but has no env hook for our key/known_hosts.
    write_ssh_config(&paths, ssh_port)?;

    // Resolve cpu/memory: explicit flag wins, else persisted config, else
    // the built-in defaults. When the user passed a flag, also persist it
    // back to ~/.avocado/vm/config.yaml so the next flag-less `vm start`
    // and Avocado.app's settings UI both see the same value.
    let (cpus, memory_mib) = resolve_and_persist_runtime(&paths, opts.cpus, opts.memory_mib)?;

    let cfg = QemuConfig {
        memory_mib,
        cpus,
        ssh_port: internal_ssh_port,
        cmdline_extra: opts.cmdline_extra,
        artifact_dir: artifact_dir.clone(),
        workspace: workspace.clone(),
    };

    // The CLI is authoritative for the qemu lifecycle on every platform.
    // Avocado.app (when installed) observes via pidfile adoption rather
    // than owning the process — that decoupling keeps the CLI usable on
    // its own and avoids the IPC stall we used to hit when `vm stop` had
    // to round-trip through the app.
    let pid = {
        let p = qemu::spawn_detached(&manifest, &paths, &cfg).await?;
        // qemu's -pidfile flag will (re)write the pid; give it a moment, then
        // also write our spawn-time pid as a fallback if the file doesn't appear.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !paths.pid_file().exists() {
            let _ = std::fs::write(paths.pid_file(), p.to_string());
        }
        p
    };

    // Spawn the hibernation supervisor. Owns the user-facing SSH port
    // and proxies through to QEMU's internal hostfwd. After
    // `idle_after_secs` of no proxied activity, sends QMP `stop` to
    // halt the vCPUs; wakes on the next incoming TCP. boot_sync below
    // goes through the proxy, which is why we spawn before waiting.
    let idle_after_secs = resolve_idle_after_secs(&paths);
    spawn_supervisor(&paths, ssh_port, internal_ssh_port, idle_after_secs).await?;

    // Wait for the guest to become ready — first signal wins (qga vs SSH).
    let signal = super::boot_sync::wait_for_guest_ready(&paths.qga_socket(), ssh_port, None)
        .await
        .context("guest never became ready (check `avocado vm logs`)")?;
    let _ = signal; // currently informational; callers may want to log which path won

    // Mount the 9p workspace share inside the guest. Best-effort during
    // start — if it fails, we surface the error so the user can debug,
    // but the VM stays up.
    let target = super::ssh::SshTarget::local(&paths, ssh_port);
    if let Err(e) = super::share::ensure_mounted_in_guest(&target).await {
        // Don't tear down the VM — user can `avocado vm shell` and inspect.
        return Err(e).context("workspace 9p mount in guest");
    }

    // Grow /var inside the VM to fill the resized block device. Idempotent
    // — no-op if /var is already at the device's max size. Non-fatal so a
    // btrfs hiccup doesn't tear down the VM.
    if let Err(e) = target.exec("btrfs filesystem resize max /var").await {
        crate::utils::output::print_warning(
            &format!(
                "btrfs resize on /var failed: {e:#}. /var size = {} bytes on host but the FS inside may not reflect that yet.",
                var_target_bytes
            ),
            crate::utils::output::OutputLevel::Normal,
        );
    }

    // Apply persisted network config (+ any one-shot --dns override). The
    // most common reason this matters: macOS host on a VPN that pushes DNS
    // via scoped resolvers, which QEMU's slirp DNS proxy (10.0.2.3) can't
    // see. Pointing the guest at public resolvers via SLIRP's NAT works.
    if let Err(e) = apply_network_config(&target, opts.dns_override.as_deref()).await {
        crate::utils::output::print_warning(
            &format!("applying network config in guest failed: {e:#}. Falling back to slirp's default DNS (10.0.2.3)."),
            crate::utils::output::OutputLevel::Normal,
        );
    }

    // Docker socket. With hibernation enabled (supervisor running), the
    // supervisor owns `docker.sock` directly and manages an SSH `-L`
    // tunnel internally with VM wake/pause lifecycle. Without
    // hibernation (idle_after_secs == 0), keep the legacy long-lived
    // forwarder behavior so existing setups don't regress.
    //
    // Non-fatal on error: a working VM with just SSH access is still
    // useful for debugging.
    if idle_after_secs == 0 {
        if let Err(e) = super::forward::start(&paths, ssh_port).await {
            crate::utils::output::print_warning(
                &format!(
                    "docker socket forward failed: {e:#}. Local DOCKER_HOST routing won't work until you start it. \
                     (`avocado vm stop && avocado vm start` retries.)"
                ),
                crate::utils::output::OutputLevel::Normal,
            );
        }
    }

    notify_desktop(
        "vm.notify.running",
        serde_json::json!({ "pid": pid, "ssh_port": ssh_port }),
    );

    Ok(VmStatus {
        running: true,
        pid: Some(pid),
        ssh_port: Some(ssh_port),
        manifest_platform: Some(manifest.platform),
        manifest_arch: Some(manifest.architecture),
        paths,
    })
}

/// Stop the VM gracefully. Sends QMP `quit`; falls back to SIGTERM/SIGKILL.
pub async fn stop(force: bool) -> Result<()> {
    let result = stop_inner(force).await;
    // Announce regardless of how stop_inner exited (graceful, signal, or
    // already-dead) — the desktop's pidfile reconciler reaches the same
    // conclusion eventually, this just shortens the latency.
    notify_desktop("vm.notify.stopped", serde_json::json!({}));
    result
}

async fn stop_inner(force: bool) -> Result<()> {
    let paths = VmPaths::resolve()?;

    // Tear down auxiliary host-side processes BEFORE QEMU. The supervisor
    // owns the user-facing SSH port; if we left it running after QEMU
    // exited, the next `vm start` would race against a still-bound port.
    // The docker socket forwarder is an SSH child that can outlive QEMU
    // if we shut down by signal, leaving a stale `docker.sock`.
    stop_supervisor(&paths);
    let _ = super::forward::stop(&paths).await;

    let pid = match state::read_pid(&paths)? {
        Some(pid) if state::pid_alive(pid) => pid,
        _ => {
            // Already stopped — clean up any stragglers.
            state::cleanup_transient(&paths);
            return Ok(());
        }
    };

    // Now that we know there's actually a live VM to take down, flip the
    // dashboard to "Stopping…" right away rather than wait for the
    // reconciler to notice the pid is gone.
    notify_desktop("vm.notify.stopping", serde_json::json!({ "pid": pid }));

    // Try QMP quit first if the socket is around.
    #[cfg(unix)]
    if paths.qmp_socket().exists() {
        if let Ok(mut q) = QmpClient::connect(&paths.qmp_socket()).await {
            let _ = q.quit().await;
        }
    }
    // Wait briefly for graceful shutdown.
    for _ in 0..20 {
        if !state::pid_alive(pid) {
            state::cleanup_transient(&paths);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    if force {
        send_signal(pid, SIGKILL);
    } else {
        send_signal(pid, SIGTERM);
        for _ in 0..20 {
            if !state::pid_alive(pid) {
                state::cleanup_transient(&paths);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        send_signal(pid, SIGKILL);
    }

    // Final cleanup
    state::cleanup_transient(&paths);
    Ok(())
}

/// Read current status without launching anything.
pub async fn status() -> Result<VmStatus> {
    let paths = VmPaths::resolve()?;
    let pid = state::read_pid(&paths)?;
    let running = pid.map(state::pid_alive).unwrap_or(false);
    let ssh_port = state::read_ssh_port(&paths)?;

    let (platform, arch) = if paths.manifest().exists() {
        match Manifest::load(&paths.manifest()) {
            Ok(m) => (Some(m.platform), Some(m.architecture)),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    Ok(VmStatus {
        running,
        pid: if running { pid } else { None },
        ssh_port: if running { ssh_port } else { None },
        manifest_platform: platform,
        manifest_arch: arch,
        paths,
    })
}

/// Resolve the SSH target for a running VM. Errors if not running.
pub fn ssh_target_for_running() -> Result<SshTarget> {
    let paths = VmPaths::resolve()?;
    let pid = state::read_pid(&paths)?
        .ok_or_else(|| anyhow::anyhow!("avocado-vm is not running (no pidfile)"))?;
    if !state::pid_alive(pid) {
        bail!("avocado-vm pidfile is stale; run `avocado vm stop` to clean up");
    }
    let port = state::read_ssh_port(&paths)?
        .ok_or_else(|| anyhow::anyhow!("avocado-vm ssh-port file missing"))?;
    Ok(SshTarget::local(&paths, port))
}

/// Resolve effective cpu/memory and, if either was explicitly passed via
/// CLI flags, persist them back to ~/.avocado/vm/config.yaml so subsequent
/// flag-less starts (and Avocado.app's settings UI) converge.
///
/// Returns `(cpus, memory_mib)` actually used for the launch.
fn resolve_and_persist_runtime(
    paths: &VmPaths,
    flag_cpus: Option<u32>,
    flag_memory_mib: Option<u32>,
) -> Result<(u32, u32)> {
    use super::config::{RuntimeConfig, VmConfig};

    let mut cfg = VmConfig::load(paths).unwrap_or_default();
    let persisted_cpus = cfg.runtime.as_ref().and_then(|r| r.cpus);
    let persisted_memory = cfg.runtime.as_ref().and_then(|r| r.memory_mib);

    let cpus = flag_cpus.or(persisted_cpus).unwrap_or(DEFAULT_CPUS);
    let memory_mib = flag_memory_mib
        .or(persisted_memory)
        .unwrap_or(DEFAULT_MEMORY_MIB);

    // Only write back when the user actually passed a flag AND it
    // differs from what's already on disk. Avoids touching the file
    // (and growing its mtime) on every routine `vm start`.
    let cpus_changed = flag_cpus.is_some() && persisted_cpus != Some(cpus);
    let memory_changed = flag_memory_mib.is_some() && persisted_memory != Some(memory_mib);
    if cpus_changed || memory_changed {
        let runtime = cfg.runtime.get_or_insert_with(RuntimeConfig::default);
        if cpus_changed {
            runtime.cpus = Some(cpus);
        }
        if memory_changed {
            runtime.memory_mib = Some(memory_mib);
        }
        cfg.save(paths)
            .context("persisting runtime overrides to vm config")?;
    }

    Ok((cpus, memory_mib))
}

/// Apply the persisted [`config::VmConfig`] (+ optional `--dns` one-shot
/// override) inside the running guest. Currently only DNS is implemented;
/// future knobs (MTU, http_proxy, …) hang off the same entry point.
///
/// Resolution order for DNS: `dns_override` if `Some(non-empty)`, else
/// the persisted `network.dns`, else no-op (leave SLIRP's 10.0.2.3).
async fn apply_network_config(
    target: &super::ssh::SshTarget,
    dns_override: Option<&[String]>,
) -> Result<()> {
    use super::config::VmConfig;

    let paths = VmPaths::resolve()?;
    let persisted = VmConfig::load(&paths).unwrap_or_default();
    let persisted_dns = persisted
        .network
        .as_ref()
        .and_then(|n| n.dns.as_ref())
        .map(|v| v.as_slice());
    let persisted_search = persisted
        .network
        .as_ref()
        .and_then(|n| n.dns_search.as_ref())
        .map(|v| v.as_slice());

    let effective_dns: Option<&[String]> = match dns_override {
        Some(v) if !v.is_empty() => Some(v),
        _ => persisted_dns,
    };

    let Some(dns_list) = effective_dns else {
        return Ok(()); // nothing to apply
    };

    for s in dns_list {
        if !looks_like_ip(s) {
            bail!("network.dns entry {s:?} is not an IPv4/IPv6 literal");
        }
    }
    let dns_args = dns_list.join(" ");
    target
        .exec(&format!("resolvectl dns eth0 {dns_args}"))
        .await
        .with_context(|| format!("resolvectl dns eth0 {dns_args}"))?;

    // If the user set explicit search domains, honor them verbatim; else
    // install `~.` so the user-supplied resolvers handle every suffix
    // (otherwise systemd-resolved still routes some queries to 10.0.2.3
    // via per-link defaults).
    let domains = match persisted_search {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|d| shell_quote(d))
            .collect::<Vec<_>>()
            .join(" "),
        _ => "'~.'".to_string(),
    };
    target
        .exec(&format!("resolvectl domain eth0 {domains}"))
        .await
        .with_context(|| format!("resolvectl domain eth0 {domains}"))?;

    crate::utils::output::print_info(
        &format!("applied guest DNS: {}", dns_list.join(", ")),
        crate::utils::output::OutputLevel::Normal,
    );
    Ok(())
}

/// Very lax IP literal check — we just want to refuse obvious shell-injection
/// attempts before passing user input into a remote command. Anything that
/// isn't [0-9a-fA-F.:%/_] gets rejected; the kernel will catch malformed
/// IPs later.
fn looks_like_ip(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '%' | '/' | '_'))
}

/// Single-quote a search-domain string for shell. Search domains are
/// constrained (RFC 1035 label charset), but be defensive anyway.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Parse a human-readable size string ("50G", "10M", "1024K", "12345" bytes).
/// Suffixes are powers of 1024 (KiB/MiB/GiB), matching what `truncate(1)`
/// and `btrfs filesystem resize` accept on Linux.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("size must not be empty");
    }
    // Split at the first non-digit char.
    let split = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num_s, suffix) = s.split_at(split);
    let n: u64 = num_s
        .parse()
        .with_context(|| format!("can't parse number in size {s:?}"))?;
    let mult: u64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1u64 << 40,
        other => bail!("unknown size suffix {other:?} in {s:?}"),
    };
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("size {s} overflows u64"))
}

/// Grow `var.btrfs` (sparse) up to `target_bytes` if it's smaller. Shrinking
/// is refused — btrfs shrink is risky with live data and we'd rather force
/// the user to `vm rebuild --reset-data` if they really want a smaller VM.
fn grow_var_file(paths: &VmPaths, target_bytes: u64) -> Result<()> {
    let var = paths.var_disk();
    if !var.exists() {
        return Ok(());
    }
    let current = std::fs::metadata(&var)
        .with_context(|| format!("stat {}", var.display()))?
        .len();
    if target_bytes <= current {
        return Ok(());
    }
    use std::fs::OpenOptions;
    let f = OpenOptions::new()
        .write(true)
        .open(&var)
        .with_context(|| format!("open {} to grow", var.display()))?;
    f.set_len(target_bytes)
        .with_context(|| format!("set_len {} -> {target_bytes}", var.display()))?;
    Ok(())
}

/// Copy the artifact's var.btrfs into the VM state dir as `var.btrfs` if it
/// isn't already there. This is the persistent /var disk for the VM; once
/// seeded, the file is owned by the VM and the artifact dir is never
/// written back to.
fn seed_var_disk(paths: &VmPaths, manifest: &Manifest, artifact_dir: &Path) -> Result<()> {
    let dest = paths.var_disk();
    if dest.exists() {
        return Ok(());
    }
    let Some(src) = manifest.artifact_path("var", artifact_dir) else {
        // No var artifact in this manifest — fine; the VM boots without /var.
        return Ok(());
    };
    if !src.exists() {
        return Ok(());
    }
    paths.ensure()?;
    std::fs::copy(&src, &dest)
        .with_context(|| format!("seed var.btrfs: {} → {}", src.display(), dest.display()))?;
    Ok(())
}

fn send_signal(pid: u32, sig: libc::c_int) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
    }
}

/// Generate the CLI's ed25519 keypair if it isn't already on disk. The
/// public key is later baked into the VM's overlay; for the MVP we
/// authorize it via cloud-init style /root/.ssh/authorized_keys in the VM.
fn ensure_ssh_key(paths: &VmPaths) -> Result<()> {
    let priv_path = paths.ssh_key();
    let pub_path = paths.ssh_pubkey();
    if priv_path.exists() && pub_path.exists() {
        return Ok(());
    }
    use ed25519_compact::KeyPair;
    let kp = KeyPair::generate();

    // OpenSSH-format files. ed25519-compact gives raw bytes; we serialize
    // the keys in OpenSSH text form (the openssh-key-v1 wrapper). Simplest
    // path: emit the PEM block ssh-keygen would produce.
    let priv_pem = openssh_private_pem(&kp);
    let pub_line = openssh_public_line(&kp);

    write_secret(&priv_path, &priv_pem)?;
    std::fs::write(&pub_path, &pub_line)
        .with_context(|| format!("writing {}", pub_path.display()))?;
    Ok(())
}

fn write_secret(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("ssh key parent")?)
        .context("temp file for ssh key")?;
    tmp.write_all(content.as_bytes())
        .context("write ssh key tmp")?;
    tmp.flush().context("flush ssh key tmp")?;
    let f = tmp.persist(path).context("persist ssh key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = f;
    Ok(())
}

fn openssh_public_line(kp: &ed25519_compact::KeyPair) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    // OpenSSH ssh-ed25519 public format:
    //   string  "ssh-ed25519"
    //   string  <32-byte raw pubkey>
    let mut wire: Vec<u8> = Vec::new();
    write_ssh_string(&mut wire, b"ssh-ed25519");
    write_ssh_string(&mut wire, kp.pk.as_ref());
    let b64 = STANDARD.encode(&wire);
    format!("ssh-ed25519 {b64} avocado-vm\n")
}

fn openssh_private_pem(kp: &ed25519_compact::KeyPair) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    // openssh-key-v1 unencrypted format. See PROTOCOL.key in OpenSSH source.
    // Structure (all "string" types are uint32 length + bytes):
    //   "openssh-key-v1\0"        (literal)
    //   string  ciphername  ("none")
    //   string  kdfname     ("none")
    //   string  kdfoptions  ("")
    //   uint32  number_of_keys = 1
    //   string  publickey1 = (ssh-ed25519 + raw pk)
    //   string  privatekeyblock1 = (
    //       uint32 checkint
    //       uint32 checkint     (must equal previous)
    //       string keytype      "ssh-ed25519"
    //       string pubkey-raw   (32 bytes)
    //       string privkey-raw  (64 bytes = priv32 || pub32)
    //       string comment      "avocado-vm"
    //       padding (1..n to round to cipher block size; "none" uses 8)
    //   )
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"openssh-key-v1\0");
    write_ssh_string(&mut body, b"none"); // ciphername
    write_ssh_string(&mut body, b"none"); // kdfname
    write_ssh_string(&mut body, b""); // kdfoptions
    write_u32(&mut body, 1); // number_of_keys

    // public key block
    let mut pubblock: Vec<u8> = Vec::new();
    write_ssh_string(&mut pubblock, b"ssh-ed25519");
    write_ssh_string(&mut pubblock, kp.pk.as_ref());
    write_ssh_string(&mut body, &pubblock);

    // private key block
    let mut privblock: Vec<u8> = Vec::new();
    let checkint: u32 = rand::random();
    write_u32(&mut privblock, checkint);
    write_u32(&mut privblock, checkint);
    write_ssh_string(&mut privblock, b"ssh-ed25519");
    write_ssh_string(&mut privblock, kp.pk.as_ref());
    // OpenSSH stores priv as a 64-byte concatenation (seed||pub).
    let mut full_priv = Vec::with_capacity(64);
    full_priv.extend_from_slice(kp.sk.as_ref());
    write_ssh_string(&mut privblock, &full_priv);
    write_ssh_string(&mut privblock, b"avocado-vm");
    // Padding to multiple of 8 (cipher block size for "none")
    let mut pad_byte: u8 = 1;
    while !privblock.len().is_multiple_of(8) {
        privblock.push(pad_byte);
        pad_byte += 1;
    }
    write_ssh_string(&mut body, &privblock);

    let b64 = STANDARD.encode(&body);
    // Wrap at 70 cols like ssh-keygen does
    let wrapped: String = b64
        .as_bytes()
        .chunks(70)
        .map(std::str::from_utf8)
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{wrapped}\n-----END OPENSSH PRIVATE KEY-----\n")
}

fn write_ssh_string(out: &mut Vec<u8>, s: &[u8]) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}
fn write_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

/// Write `~/.avocado/vm/ssh-config` with a `Host avocado-vm` stanza pinned
/// to the current port + CLI-managed key + known_hosts. Lives in our own
/// state dir only; we never touch `~/.ssh/config`. Power users can opt in
/// with `ssh -F ~/.avocado/vm/ssh-config avocado-vm`.
fn write_ssh_config(paths: &VmPaths, ssh_port: u16) -> Result<()> {
    let content = format!(
        "Host avocado-vm\n  HostName 127.0.0.1\n  Port {ssh_port}\n  User root\n  IdentityFile {}\n  UserKnownHostsFile {}\n  StrictHostKeyChecking no\n  PasswordAuthentication no\n  BatchMode yes\n  LogLevel ERROR\n",
        paths.ssh_key().display(),
        paths.known_hosts().display(),
    );
    std::fs::write(paths.ssh_config(), content)
        .with_context(|| format!("writing {}", paths.ssh_config().display()))?;
    Ok(())
}

/// Default idle timeout in seconds when neither config nor env var sets
/// one. One minute strikes a balance between freeing host CPU promptly
/// when the user steps away from active work and not pausing mid-pause
/// during normal SSH/docker bursts. Users with snappier wake budgets
/// can lower via `avocado vm config set idle.hibernate_after_secs N`.
const DEFAULT_IDLE_AFTER_SECS: u64 = 60;

/// Resolve the hibernate timeout. Env var wins (one-shot override for
/// experimentation), else the persisted `idle.hibernate_after_secs`,
/// else the default. `0` disables hibernation while keeping the proxy
/// up — useful for isolating proxy issues from QMP issues.
fn resolve_idle_after_secs(paths: &VmPaths) -> u64 {
    if let Ok(raw) = std::env::var("AVOCADO_VM_IDLE_HIBERNATE_SECS") {
        if let Ok(parsed) = raw.parse::<u64>() {
            return parsed;
        }
    }
    if let Ok(cfg) = super::config::VmConfig::load(paths) {
        if let Some(idle) = &cfg.idle {
            if let Some(v) = idle.hibernate_after_secs {
                return v;
            }
        }
    }
    DEFAULT_IDLE_AFTER_SECS
}

/// Spawn `avocado vm supervise` as a detached child. Same daemonization
/// pattern as QEMU (setsid + null stdio), pid recorded so `stop_inner`
/// can take it down before QEMU. We re-exec the running binary
/// (`std::env::current_exe`) rather than expecting an installed
/// `avocado` on PATH — that way a `cargo run` or out-of-tree binary
/// supervises itself instead of pulling in a stale system copy.
/// Best-effort SIGTERM → SIGKILL on the supervisor pid, then remove
/// its pidfile + internal-ssh-port marker. Idempotent — missing
/// pidfile / dead pid is a no-op.
fn stop_supervisor(paths: &VmPaths) {
    let pidfile = paths.supervisor_pid();
    if let Ok(raw) = std::fs::read_to_string(&pidfile) {
        if let Ok(pid) = raw.trim().parse::<u32>() {
            if state::pid_alive(pid) {
                send_signal(pid, SIGTERM);
                for _ in 0..20 {
                    if !state::pid_alive(pid) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if state::pid_alive(pid) {
                    send_signal(pid, SIGKILL);
                }
            }
        }
    }
    let _ = std::fs::remove_file(pidfile);
    let _ = std::fs::remove_file(paths.internal_ssh_port_file());
}

async fn spawn_supervisor(
    paths: &VmPaths,
    user_port: u16,
    internal_port: u16,
    idle_after_secs: u64,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating current avocado binary")?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args([
        "vm",
        "supervise",
        "--user-port",
        &user_port.to_string(),
        "--internal-port",
        &internal_port.to_string(),
        "--qmp-socket",
        &paths.qmp_socket().to_string_lossy(),
        "--idle-after-secs",
        &idle_after_secs.to_string(),
        "--pid-file",
        &paths.supervisor_pid().to_string_lossy(),
        "--docker-socket",
        &paths.docker_socket().to_string_lossy(),
        "--docker-socket-internal",
        &paths.docker_socket_internal().to_string_lossy(),
        "--ssh-key",
        &paths.ssh_key().to_string_lossy(),
        "--known-hosts",
        &paths.known_hosts().to_string_lossy(),
    ]);
    // Append the supervisor's stderr to ~/.avocado/vm/supervisor.log so
    // pause/resume events are recoverable post-mortem. `tail -F` is
    // robust to the file appearing only after first launch.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.supervisor_log())
        .with_context(|| format!("opening {}", paths.supervisor_log().display()))?;
    let log_dup = log.try_clone().context("cloning supervisor log handle")?;
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(log);
    cmd.stderr(log_dup);
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn supervisor: {}", exe.display()))?;
    let spawn_pid = child.id().unwrap_or(0);
    drop(child);

    // Poll briefly for the supervisor's listener to come up — proves the
    // proxy is ready before boot_sync starts pumping connections through.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", user_port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            // Don't fail the whole boot — log + carry on with whatever
            // the supervisor managed to do. Worst case the user-facing
            // port refuses connections and the user sees a normal SSH
            // connection error.
            crate::utils::output::print_warning(
                &format!(
                    "hibernation supervisor (pid {spawn_pid}) didn't bind 127.0.0.1:{user_port} within 5s; \
                     proxy may be down. SSH may not work until you restart with `vm stop && vm start`."
                ),
                crate::utils::output::OutputLevel::Normal,
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
