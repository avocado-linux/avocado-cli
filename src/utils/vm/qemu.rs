//! `qemu-system-*` process management.
//!
//! Composes a complete command line from a [`manifest::Manifest`] and a
//! [`state::VmPaths`], spawns it daemonized (via `setsid` + redirected
//! stdio), records the pid, and surfaces a [`QemuHandle`] the lifecycle
//! layer can stop later.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::fdt;
use super::manifest::Manifest;
use super::state::VmPaths;

/// Knobs the CLI passes to QEMU at launch.
#[derive(Debug, Clone)]
pub struct QemuConfig {
    /// Memory in MiB.
    pub memory_mib: u32,
    /// vCPU count.
    pub cpus: u32,
    /// SSH host port (forwarded to guest :22).
    pub ssh_port: u16,
    /// Extra kernel cmdline appended to the manifest's default.
    pub cmdline_extra: Option<String>,
    /// Where the artifact files live (manifest dir).
    pub artifact_dir: PathBuf,
    /// Host path exposed to the guest as a 9p `workspace` share.
    pub workspace: PathBuf,
}

/// Resolve the right qemu-system binary for the manifest's architecture.
pub fn qemu_binary_for(manifest: &Manifest) -> Result<&'static str> {
    match manifest.architecture.as_str() {
        "arm64" | "aarch64" => Ok("qemu-system-aarch64"),
        "x86_64" => Ok("qemu-system-x86_64"),
        other => bail!("unsupported manifest architecture '{other}'"),
    }
}

/// Acceleration flag for the host: HVF on macOS, KVM on Linux, TCG fallback.
fn accel_flag() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "hvf"
    }
    #[cfg(all(not(target_os = "macos"), target_os = "linux"))]
    {
        "kvm"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "tcg"
    }
}

/// Machine type per architecture.
fn machine_for(arch: &str) -> &'static str {
    match arch {
        "arm64" | "aarch64" => "virt",
        "x86_64" => "q35",
        _ => "virt",
    }
}

/// CPU model per arch.
fn cpu_for(arch: &str) -> &'static str {
    match arch {
        "arm64" | "aarch64" => "host",
        "x86_64" => "host",
        _ => "max",
    }
}

/// Build the full `qemu-system-*` argv (excluding the program name).
///
/// The arguments are recipe-pinned for the avocado-vm use case:
/// - direct kernel/initramfs boot (no bootloader)
/// - virtio-blk drives for rootfs (read-only) and data disk (rw, if present)
/// - usermode networking with a single TCP port-forward to guest sshd
/// - QMP + qga sockets on Unix paths
/// - serial console redirected to a logfile for postmortem
/// - one xhci controller so USB hot-plug works in Phase 3 without restart
pub fn build_qemu_args(
    manifest: &Manifest,
    paths: &VmPaths,
    cfg: &QemuConfig,
) -> Result<Vec<String>> {
    let arch = manifest.architecture.clone();

    let kernel = manifest
        .artifact_path("kernel", &cfg.artifact_dir)
        .context("manifest has no `kernel` artifact")?;
    let initrd = manifest
        .artifact_path("initramfs", &cfg.artifact_dir)
        .context("manifest has no `initramfs` artifact")?;
    let rootfs = manifest
        .artifact_path("rootfs", &cfg.artifact_dir)
        .context("manifest has no `rootfs` artifact")?;

    // /var mounting is handled by the rootfs overlay's /etc/fstab (which
    // mounts /var from /dev/vdb directly). Don't duplicate it on the
    // kernel cmdline — that triggers a systemd-fstab-generator conflict
    // ("Duplicate entry in /proc/cmdline?") which aborts fstab processing
    // entirely and cascades into logind/timesyncd/ldconfig failures.
    let mut cmdline = manifest.cmdline_default.trim().to_string();
    if let Some(extra) = &cfg.cmdline_extra {
        cmdline.push(' ');
        cmdline.push_str(extra.trim());
    }

    let mut args: Vec<String> = Vec::new();

    args.push("-machine".into());
    args.push(machine_for(&arch).into());

    args.push("-accel".into());
    args.push(accel_flag().into());

    args.push("-cpu".into());
    args.push(cpu_for(&arch).into());

    args.push("-smp".into());
    args.push(cfg.cpus.to_string());

    args.push("-m".into());
    args.push(format!("{}M", cfg.memory_mib));

    args.push("-nographic".into());

    // Kernel + initrd direct boot
    args.push("-kernel".into());
    args.push(kernel.to_string_lossy().into_owned());
    args.push("-initrd".into());
    args.push(initrd.to_string_lossy().into_owned());
    args.push("-append".into());
    args.push(cmdline);

    // Rootfs as virtio-blk read-only
    args.push("-drive".into());
    args.push(format!(
        "file={},if=virtio,format=raw,readonly=on",
        rootfs.display()
    ));

    // /var image. The artifact's var.btrfs is read-only (we never modify
    // the artifact dir); the writable copy lives in the VM state dir and
    // is seeded by lifecycle::seed_var_disk. Attach the state-dir copy.
    let var_disk = paths.var_disk();
    if var_disk.exists() {
        args.push("-drive".into());
        args.push(format!("file={},if=virtio,format=raw", var_disk.display()));
    }

    // Optional persistent /data disk in the VM state dir
    let data = paths.data_disk();
    if data.exists() {
        args.push("-drive".into());
        args.push(format!("file={},if=virtio,format=qcow2", data.display()));
    }

    // Usermode networking; one port-forward to guest sshd
    args.push("-netdev".into());
    args.push(format!(
        "user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22",
        cfg.ssh_port
    ));
    args.push("-device".into());
    // virtio-net-pci, not virtio-net-device: the x86 `q35` machine exposes
    // PCIe, not the virtio-mmio bus that `virtio-net-device` binds to, so the
    // mmio variant aborts qemu at startup with "No 'virtio-bus' bus found for
    // device 'virtio-net-device'" and `vm start` never boots. arm64 `virt`
    // also has PCIe, so -pci is correct for both arches (matches virtio-9p-pci).
    args.push("virtio-net-pci,netdev=net0".into());

    // USB host controller (empty; USB devices are attached at runtime via QMP)
    args.push("-device".into());
    args.push("qemu-xhci,id=xhci".into());

    // 9p workspace share — declared once at launch, mounted post-boot at
    // /mnt/workspace by share::ensure_mounted_in_guest().
    for a in super::share::qemu_args_for(&cfg.workspace) {
        args.push(a);
    }

    // QMP control socket
    args.push("-chardev".into());
    args.push(format!(
        "socket,id=qmp,path={},server=on,wait=off",
        paths.qmp_socket().display()
    ));
    args.push("-mon".into());
    args.push("chardev=qmp,mode=control".into());

    // qga virtio-serial channel
    args.push("-chardev".into());
    args.push(format!(
        "socket,id=qga,path={},server=on,wait=off",
        paths.qga_socket().display()
    ));
    args.push("-device".into());
    args.push("virtio-serial".into());
    args.push("-device".into());
    args.push("virtserialport,chardev=qga,name=org.qemu.guest_agent.0".into());

    // Avocado control plane — second virtserialport on the same
    // virtio-serial bus. The guest side opens
    // `/dev/virtio-ports/avocado.control`; the host side gets a Unix
    // socket at `paths.control_socket()` that Avocado.app connects to
    // from `USBHostBridge`/`ControlPlane`. Without this, the desktop
    // app's "Attach to VM" flow stalls on "agent hasn't responded"
    // (the helper waits forever for `control.sock` to materialize).
    args.push("-chardev".into());
    args.push(format!(
        "socket,id=avocadoctl,path={},server=on,wait=off",
        paths.control_socket().display()
    ));
    args.push("-device".into());
    args.push("virtserialport,chardev=avocadoctl,name=avocado.control".into());

    // Serial console -> logfile
    args.push("-serial".into());
    args.push(format!("file:{}", paths.serial_log().display()));

    // Pidfile so the lifecycle layer can find us
    args.push("-pidfile".into());
    args.push(paths.pid_file().to_string_lossy().into_owned());

    // On arm64, splice PSCI idle-states into the DTB so the in-guest
    // cpuidle driver actually binds. See fdt.rs for the why; the short
    // version is "QEMU virt doesn't emit cpu-idle-states bindings, so
    // CONFIG_ARM_PSCI_CPUIDLE has nothing to attach to and idle CPUs spin
    // through HVF vmexit/vmenter at ~80% host each."
    //
    // Failures degrade gracefully: log + skip, kernel falls back to the
    // auto-generated DTB it would have used anyway.
    if matches!(arch.as_str(), "arm64" | "aarch64") {
        let dtb_override = std::env::var("AVOCADO_VM_DTB")
            .ok()
            .filter(|s| !s.is_empty());
        match dtb_override {
            Some(path) => {
                args.push("-dtb".into());
                args.push(path);
            }
            None => match ensure_idle_states_dtb(paths, cfg) {
                Ok(path) => {
                    args.push("-dtb".into());
                    args.push(path.to_string_lossy().into_owned());
                }
                Err(e) => {
                    eprintln!(
                        "warn: PSCI idle-states DTB preparation failed ({e}); booting with \
                         auto-generated DTB (expect higher host CPU at guest idle)"
                    );
                }
            },
        }
    }

    Ok(args)
}

/// Produce a DTB patched with PSCI `idle-states` for the current QEMU
/// config. Cached under `~/.avocado/vm/dtb/`, keyed by parameters that
/// affect the DT layout (memory range and cpu count change DT nodes;
/// QEMU version may change auto-generated property shapes).
///
/// The cache miss path runs `qemu-system-aarch64 -machine virt,dumpdtb=…`
/// to capture QEMU's auto-generated DTB, splices in the missing nodes,
/// then atomically renames into place. Cost is ~500ms per cache miss,
/// hidden under the rest of VM boot. Cache hits return immediately.
fn ensure_idle_states_dtb(paths: &VmPaths, cfg: &QemuConfig) -> Result<PathBuf> {
    // Cache key uses the QEMU binary's mtime instead of `--version` to
    // avoid shelling out on every launch. A `brew upgrade qemu` bumps
    // the mtime, which naturally invalidates the cache. The mtime stat
    // is microseconds; the `--version` subprocess pays the full dyld
    // load cost (~300-500 ms on macOS) for libsnappy/libpng/libfdt.
    let qemu_tag = qemu_binary_tag("qemu-system-aarch64")?;
    let cache_dir = paths.dtb_cache_dir();
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let cache_path = cache_dir.join(format!(
        "virt-smp{}-m{}-q{}.dtb",
        cfg.cpus, cfg.memory_mib, qemu_tag
    ));
    if cache_path.is_file() {
        return Ok(cache_path);
    }
    let tmp = tempfile::NamedTempFile::new_in(&cache_dir)
        .context("failed to create temp file for DTB dump")?;
    dump_base_dtb("qemu-system-aarch64", cfg, tmp.path())
        .context("failed to dump base DTB from QEMU")?;
    let raw = std::fs::read(tmp.path())
        .with_context(|| format!("failed to read dumped DTB at {}", tmp.path().display()))?;
    let mut fdt = fdt::parse(&raw).context("failed to parse QEMU-generated DTB")?;
    fdt::patch_idle_states(&mut fdt, cfg.cpus).context("failed to splice idle-states into DTB")?;
    let patched = fdt::serialize(&fdt);
    std::fs::write(tmp.path(), &patched)
        .with_context(|| format!("failed to write patched DTB to {}", tmp.path().display()))?;
    tmp.persist(&cache_path)
        .with_context(|| format!("failed to install patched DTB at {}", cache_path.display()))?;
    Ok(cache_path)
}

/// Run `qemu-system-aarch64 -machine virt,dumpdtb=PATH` and let QEMU
/// write its auto-generated DTB, then exit. We pass the same
/// `-machine`, `-smp`, `-m`, `-cpu`, `-accel` flags that affect DT
/// generation so the dumped tree matches what the real launch would see.
fn dump_base_dtb(qemu_bin: &str, cfg: &QemuConfig, out: &Path) -> Result<()> {
    let machine = format!("virt,dumpdtb={}", out.display());
    let status = std::process::Command::new(qemu_bin)
        .args([
            "-machine",
            &machine,
            "-accel",
            accel_flag(),
            "-cpu",
            cpu_for("aarch64"),
            "-smp",
            &cfg.cpus.to_string(),
            "-m",
            &format!("{}M", cfg.memory_mib),
            "-nographic",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to spawn {qemu_bin} for dumpdtb"))?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        bail!(
            "{qemu_bin} dumpdtb exited with {}: {}",
            status.status,
            stderr.trim()
        );
    }
    Ok(())
}

/// Stable, filename-safe identifier for the QEMU binary, used in the
/// DTB cache key. Uses the binary's mtime (seconds since epoch) rather
/// than `--version` so we don't spawn a subprocess on every launch.
/// `brew upgrade qemu` bumps the mtime, naturally invalidating the
/// cache; a binary that hasn't been touched produces the same key
/// indefinitely.
fn qemu_binary_tag(qemu_bin: &str) -> Result<String> {
    let path = which_on_path(qemu_bin).with_context(|| format!("{qemu_bin} not found on $PATH"))?;
    let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("m{mtime}"))
}

/// Spawn QEMU detached from the controlling terminal. Returns the child pid.
/// The child writes its own pidfile thanks to `-pidfile`; we also capture
/// the spawn-time pid so the caller can `kill` it directly if needed.
pub async fn spawn_detached(manifest: &Manifest, paths: &VmPaths, cfg: &QemuConfig) -> Result<u32> {
    let bin = qemu_binary_for(manifest)?;
    let args = build_qemu_args(manifest, paths, cfg)?;

    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(&args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // Put the child in its own session so Ctrl-C in the avocado CLI doesn't
    // kill QEMU (the user manages lifetime via `avocado vm stop`). The
    // `pre_exec` method is exposed by tokio::process::Command directly on
    // unix; no extra import needed.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {bin}"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("spawned child has no pid"))?;
    // Detach from the child; QEMU runs independently. The pidfile mechanism
    // handles tracking from here on.
    drop(child);
    Ok(pid)
}

/// Check that the qemu-system binary for this manifest is on `$PATH`.
pub fn ensure_qemu_available(manifest: &Manifest) -> Result<()> {
    let bin = qemu_binary_for(manifest)?;
    // Cross-platform `which`: scan `$PATH`.
    if which_on_path(bin).is_none() {
        bail!("{bin} not found in $PATH; install QEMU (e.g. `brew install qemu` on macOS)");
    }
    Ok(())
}

fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(bin);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// Pick a free TCP port on the loopback by binding to 0 and reading what
/// the OS assigned, then dropping the listener so QEMU can claim it.
pub fn pick_free_port() -> Result<u16> {
    use std::net::TcpListener;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to bind temp listener for port discovery")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// For tests / debugging: render the argv as a shell-ish string.
#[allow(dead_code)]
pub fn render_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('=') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_manifest(arch: &str) -> Manifest {
        let raw = json!({
            "format": "avocado-direct",
            "format_version": 1,
            "platform": format!("avocado-qemu{arch}"),
            "architecture": arch,
            "artifacts": {
                "kernel":    { "file": "Image",     "sha256": "00", "type": "kernel" },
                "initramfs": { "file": "initramfs", "sha256": "00", "type": "initramfs-cpio-zst" },
                "rootfs":    { "file": "rootfs",    "sha256": "00", "type": "erofs-lz4" },
                "var":       { "file": "var",       "sha256": "00", "type": "btrfs" }
            },
            "cmdline_default": "console=ttyAMA0 root=/dev/vda rw"
        });
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn picks_correct_binary() {
        let m = fake_manifest("arm64");
        assert_eq!(qemu_binary_for(&m).unwrap(), "qemu-system-aarch64");
        let m = fake_manifest("x86_64");
        assert_eq!(qemu_binary_for(&m).unwrap(), "qemu-system-x86_64");
    }

    #[test]
    fn assembles_expected_arg_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let m = fake_manifest("arm64");
        let paths = VmPaths::at(tmp.path());
        let cfg = QemuConfig {
            memory_mib: 2048,
            cpus: 2,
            ssh_port: 51234,
            cmdline_extra: Some("init=/sbin/init".into()),
            artifact_dir: tmp.path().to_path_buf(),
            workspace: tmp.path().to_path_buf(),
        };
        let args = build_qemu_args(&m, &paths, &cfg).unwrap();
        let rendered = args.join(" ");
        assert!(rendered.contains("-machine virt"));
        assert!(rendered.contains("-smp 2"));
        assert!(rendered.contains("-m 2048M"));
        assert!(rendered.contains("-kernel "));
        assert!(rendered.contains("-initrd "));
        assert!(rendered.contains("init=/sbin/init"));
        assert!(rendered.contains("hostfwd=tcp:127.0.0.1:51234-:22"));
        assert!(rendered.contains("qemu-xhci"));
        assert!(rendered.contains("org.qemu.guest_agent.0"));
        assert!(rendered.contains("readonly=on"));
        assert!(rendered.contains("mount_tag=workspace"));
    }

    #[test]
    fn pick_free_port_returns_high_port() {
        let p = pick_free_port().unwrap();
        assert!(p > 0);
    }
}
