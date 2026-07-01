//! Software TPM (swtpm) provisioning for the avocado-vm.
//!
//! The avocado-vm image is built with the `tpm2` distro feature
//! (meta-avocado `kas/feature/tpm.yml`), so systemd waits for `/dev/tpm0` at
//! boot. Provide a swtpm-backed TPM 2.0 the same way meta-avocado's
//! `meta-avocado-qemu/scripts/run-qemux86-64-swtpm` script does; without it
//! the guest blocks ~90s on the missing TPM device before boot continues.
//!
//! When `swtpm` is not installed the caller instead appends
//! `systemd.tpm2_wait=false` to the kernel cmdline (see
//! [`super::qemu::build_qemu_args`]) so the guest still boots promptly, just
//! without a TPM.

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::state::VmPaths;

/// True when a usable `swtpm` binary is on `PATH`.
pub fn swtpm_available() -> bool {
    Command::new("swtpm")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// QEMU args wiring the guest TPM to the running swtpm control socket. Mirrors
/// meta-avocado's `run-qemux86-64-swtpm`: `tpm-tis` on x86 (the `q35` machine
/// exposes the LPC bus it needs); arm64 `virt` uses the sysbus
/// `tpm-tis-device`.
pub fn qemu_tpm_args(paths: &VmPaths, arch: &str) -> Vec<String> {
    let device = match arch {
        "arm64" | "aarch64" => "tpm-tis-device,tpmdev=tpm0",
        _ => "tpm-tis,tpmdev=tpm0",
    };
    vec![
        "-chardev".into(),
        format!("socket,id=chrtpm,path={}", paths.tpm_socket().display()),
        "-tpmdev".into(),
        "emulator,id=tpm0,chardev=chrtpm".into(),
        "-device".into(),
        device.into(),
    ]
}

/// Start swtpm as a daemon with a unix control socket under the VM state dir,
/// blocking until the socket appears (up to ~10s). Kills any leftover swtpm
/// for this state dir first so a stale daemon can't hold the socket.
pub fn spawn_swtpm(paths: &VmPaths) -> Result<()> {
    let dir = paths.tpm_dir();
    let sock = paths.tpm_socket();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create tpm state dir {}", dir.display()))?;
    stop_swtpm(paths);
    let _ = std::fs::remove_file(&sock);

    let status = Command::new("swtpm")
        .arg("socket")
        .arg("--tpmstate")
        .arg(format!("dir={}", dir.display()))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={}", sock.display()))
        .arg("--log")
        .arg(format!("file={},level=1", dir.join("swtpm.log").display()))
        .arg("--tpm2")
        .arg("--daemon")
        .status()
        .context("spawn swtpm")?;
    if !status.success() {
        bail!("swtpm exited with {:?}", status.code());
    }

    for _ in 0..100 {
        if sock.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("swtpm control socket {} never appeared", sock.display());
}

/// Best-effort teardown of the swtpm daemon backing this VM's TPM. Matches on
/// the state-dir path so only this VM's swtpm is killed, mirroring the
/// `pkill -f "swtpm socket.*${TPM_DIR}"` cleanup in the meta-avocado script.
pub fn stop_swtpm(paths: &VmPaths) {
    let dir = paths.tpm_dir();
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(format!("swtpm socket.*{}", dir.display()))
        .status();
}
