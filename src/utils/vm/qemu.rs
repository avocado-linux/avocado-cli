//! `qemu-system-*` process management.
//!
//! Composes a complete command line from a [`manifest::Manifest`] and a
//! [`state::VmPaths`], spawns it daemonized (via `setsid` + redirected
//! stdio), records the pid, and surfaces a [`QemuHandle`] the lifecycle
//! layer can stop later.
//!
//! On macOS the spawn step is delegated to Avocado.app (see
//! `utils/vm/client.rs` + `lifecycle::delegate_start_to_app`), so this
//! module's spawn helpers are unused there — the cfg_attr below silences
//! the dead-code lint specifically for that platform without hiding
//! actual bugs on Linux.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;

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
    args.push("virtio-net-device,netdev=net0".into());

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

    // Serial console -> logfile
    args.push("-serial".into());
    args.push(format!("file:{}", paths.serial_log().display()));

    // Pidfile so the lifecycle layer can find us
    args.push("-pidfile".into());
    args.push(paths.pid_file().to_string_lossy().into_owned());

    Ok(args)
}

/// Spawn QEMU detached from the controlling terminal. Returns the child pid.
/// The child writes its own pidfile thanks to `-pidfile`; we also capture
/// the spawn-time pid so the caller can `kill` it directly if needed.
pub async fn spawn_detached(
    manifest: &Manifest,
    paths: &VmPaths,
    cfg: &QemuConfig,
) -> Result<u32> {
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
        bail!(
            "{bin} not found in $PATH; install QEMU (e.g. `brew install qemu` on macOS)"
        );
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
