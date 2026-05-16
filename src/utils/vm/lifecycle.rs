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
use super::qmp::QmpClient;
use super::ssh::SshTarget;
use super::state::{self, VmPaths};

/// Knobs the `avocado vm start` command resolves from args + env.
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// Directory containing the `direct` profile output (manifest.json + artifacts).
    pub vm_source: PathBuf,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// vCPU count.
    pub cpus: u32,
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
}

/// Default target size for the persistent var.btrfs. 50 GiB sparse — large
/// enough for an SDK image + a comfortable working set of containers without
/// being wasteful on disk-pressure-sensitive hosts (the file only consumes
/// what's actually written).
pub const DEFAULT_VAR_SIZE: &str = "50G";

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
            bail!(
                "avocado-vm already running (pid {pid}); use `avocado vm stop` first"
            );
        }
        // Stale pidfile — clean it up.
        state::cleanup_transient(&paths);
    }

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
    let _ = std::fs::write(paths.artifact_dir_file(), artifact_dir.display().to_string());

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
    let var_target_bytes = parse_size(
        opts.var_size.as_deref().unwrap_or(DEFAULT_VAR_SIZE),
    )
    .context("invalid --var-size")?;
    grow_var_file(&paths, var_target_bytes)?;

    let workspace = super::share::resolve_workspace(opts.workspace.as_deref())?;
    super::share::record_workspace(&paths, &workspace)?;

    let ssh_port = match opts.ssh_port {
        Some(p) => p,
        None => qemu::pick_free_port()?,
    };
    state::write_ssh_port(&paths, ssh_port)?;

    // Now that the port is known, write the ssh-config + wire it into
    // ~/.ssh/config. This is required for `DOCKER_HOST=ssh://avocado-vm`
    // to resolve in any subprocess we spawn — Docker's ssh transport reads
    // ~/.ssh/config but has no env hook for our key/known_hosts.
    write_ssh_config(&paths, ssh_port)?;

    let cfg = QemuConfig {
        memory_mib: opts.memory_mib,
        cpus: opts.cpus,
        ssh_port,
        cmdline_extra: opts.cmdline_extra,
        artifact_dir: artifact_dir.clone(),
        workspace: workspace.clone(),
    };

    // On macOS, Avocado.app owns the QEMU process so its dashboard /
    // USB bridge / virtio-serial control plane can observe lifecycle and
    // wire up the rest of the supervisor stack. The CLI keeps its
    // pre-flight (ssh keys, ssh config, var disk seeding) and post-boot
    // wiring (workspace mount, docker forward); only the spawn step is
    // delegated. Linux keeps the direct spawn.
    let pid = {
        #[cfg(target_os = "macos")]
        {
            let _ = (&manifest, &cfg); // suppress unused-var warnings on this arm
            delegate_start_to_app(&artifact_dir, opts.memory_mib, opts.cpus, ssh_port).await?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let p = qemu::spawn_detached(&manifest, &paths, &cfg).await?;
            // qemu's -pidfile flag will (re)write the pid; give it a moment, then
            // also write our spawn-time pid as a fallback if the file doesn't appear.
            tokio::time::sleep(Duration::from_millis(200)).await;
            if !paths.pid_file().exists() {
                let _ = std::fs::write(paths.pid_file(), p.to_string());
            }
            p
        }
    };

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
    if let Err(e) = target
        .exec("btrfs filesystem resize max /var")
        .await
    {
        crate::utils::output::print_warning(
            &format!(
                "btrfs resize on /var failed: {e:#}. /var size = {} bytes on host but the FS inside may not reflect that yet.",
                var_target_bytes
            ),
            crate::utils::output::OutputLevel::Normal,
        );
    }

    // Bring up the docker-socket SSH forward so DOCKER_HOST=unix://… works
    // from the host without touching the user's ~/.ssh/config. Non-fatal
    // on error: a working VM with just SSH access is still useful for
    // debugging.
    if let Err(e) = super::forward::start(&paths, ssh_port).await {
        crate::utils::output::print_warning(
            &format!(
                "docker socket forward failed: {e:#}. Local DOCKER_HOST routing won't work until you start it. \
                 (`avocado vm stop && avocado vm start` retries.)"
            ),
            crate::utils::output::OutputLevel::Normal,
        );
    }

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
    let paths = VmPaths::resolve()?;

    // Always try to tear down the docker socket forward first — the SSH
    // process can outlive QEMU if we shut down the VM by signal, leaving
    // a stale `docker.sock` on the host.
    let _ = super::forward::stop(&paths).await;

    let pid = match state::read_pid(&paths)? {
        Some(pid) if state::pid_alive(pid) => pid,
        _ => {
            // Already stopped — clean up any stragglers.
            state::cleanup_transient(&paths);
            return Ok(());
        }
    };

    // On macOS, ask Avocado.app to stop its supervised qemu so the
    // dashboard + lifecycle observers see the transition. Falls through to
    // the pidfile-signal path if the app isn't reachable.
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut client) = super::client::Client::connect() {
            if client.request("vm.stop", serde_json::json!({})).is_ok() {
                // Wait for the app's monitored process to exit.
                for _ in 0..40 {
                    if !state::pid_alive(pid) {
                        state::cleanup_transient(&paths);
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                // Fall through to forceful path below.
            }
        }
    }

    // Try QMP quit first if the socket is around.
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
        send_signal(pid, libc::SIGKILL);
    } else {
        send_signal(pid, libc::SIGTERM);
        for _ in 0..20 {
            if !state::pid_alive(pid) {
                state::cleanup_transient(&paths);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        send_signal(pid, libc::SIGKILL);
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
    n.checked_mul(mult).ok_or_else(|| anyhow::anyhow!("size {s} overflows u64"))
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
    let mut tmp = tempfile::NamedTempFile::new_in(
        path.parent().context("ssh key parent")?,
    )
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
    while privblock.len() % 8 != 0 {
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

/// Ask Avocado.app to spawn qemu and wait for ready. Auto-launches the app
/// if it isn't running yet. Returns the qemu pid the app reports.
///
/// Pre-flight (ssh keys, ssh-config, var.btrfs seeding, manifest sha256
/// verify) has already happened by the time this is called — both sides
/// trust ~/.avocado/vm/ for the supporting state.
#[cfg(target_os = "macos")]
async fn delegate_start_to_app(
    artifact_dir: &Path,
    memory_mib: u32,
    cpus: u32,
    ssh_port: u16,
) -> Result<u32> {
    let mut client = super::client::Client::connect_or_launch()
        .context("failed to reach Avocado.app for vm.start")?;

    let params = serde_json::json!({
        "vm_dir": artifact_dir.display().to_string(),
        "memory_mib": memory_mib,
        "cpus": cpus,
        "ssh_port": ssh_port,
    });
    let _ = client.request("vm.start", params).context("vm.start dispatch")?;

    // Block until the app reports running (or errors). The app's own
    // monitor + pidfile sync gives us the authoritative pid.
    let ready = client
        .request("vm.wait_ready", serde_json::json!({ "timeout_sec": 120.0 }))
        .context("vm.wait_ready")?;
    let pid = ready
        .get("pid")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("vm.wait_ready returned no pid: {ready}"))?;
    Ok(pid as u32)
}
