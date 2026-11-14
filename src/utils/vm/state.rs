//! State directory layout for `avocado-vm`.
//!
//! Everything the host-side CLI persists about a running VM lives under
//! `~/.avocado/vm/`. Sockets, pidfile and lock are runtime-only; rootfs.img,
//! data.qcow2, ssh-key and manifest.json survive across `avocado vm start`
//! invocations. `data.qcow2` is intentionally never recreated implicitly —
//! it carries the in-VM Docker volumes.

use anyhow::{Context, Result};
use std::path::PathBuf;

use super::endpoint::Endpoint;

/// Paths under `~/.avocado/vm/`.
#[derive(Debug, Clone)]
pub struct VmPaths {
    /// Root of the VM state directory.
    pub root: PathBuf,
}

impl VmPaths {
    /// Resolve `~/.avocado/vm/`. Errors if `$HOME` is unset.
    pub fn resolve() -> Result<Self> {
        let home = dirs_home()?;
        Ok(Self {
            root: home.join(".avocado").join("vm"),
        })
    }

    /// Resolve to a custom root — used by tests with a tempdir.
    #[allow(dead_code)]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Create the directory if missing.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        Ok(())
    }

    pub fn manifest(&self) -> PathBuf {
        self.root.join("manifest.json")
    }
    /// Managed install directory — where `avocado vm update` writes
    /// downloaded artifacts. Distinct from a developer-supplied
    /// `--vm-source` (which the CLI never modifies).
    pub fn install_dir(&self) -> PathBuf {
        self.root.join("install")
    }
    /// Manifest for the managed install (separate from
    /// [`Self::manifest`], which is the copy used by the most recent
    /// `vm start` regardless of whether it came from a dev artifact
    /// dir or the managed install).
    pub fn install_manifest(&self) -> PathBuf {
        self.install_dir().join("manifest.json")
    }

    /// Default artifact directory to boot from when the user didn't
    /// pass `--vm-source` and `AVOCADO_VM_DIR` is unset. Layered:
    ///
    ///   1. [`install_dir`] if it has a manifest (i.e. the user ran
    ///      `avocado vm update` — the common path for end-users).
    ///   2. The `artifact-dir` pointer file written by previous
    ///      `vm start` / `vm rebuild` runs, if it still points at
    ///      an extant directory with a manifest (dev workflow).
    ///   3. `None` — the caller surfaces an error pointing the user
    ///      at `avocado vm update`.
    pub fn default_vm_source(&self) -> Option<PathBuf> {
        // Managed install first — that's what the user gets after
        // `avocado vm update`.
        let install = self.install_dir();
        if install.join("manifest.json").is_file() {
            return Some(install);
        }
        // Fallback: artifact-dir pointer file from the last explicit
        // `vm start --vm-source <dir>` / `vm rebuild`.
        if let Ok(raw) = std::fs::read_to_string(self.artifact_dir_file()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let p = PathBuf::from(trimmed);
                if p.join("manifest.json").is_file() {
                    return Some(p);
                }
            }
        }
        None
    }
    /// Reserved for future artifact caching under ~/.avocado/vm/.
    #[allow(dead_code)]
    pub fn rootfs(&self) -> PathBuf {
        self.root.join("rootfs.img")
    }
    pub fn data_disk(&self) -> PathBuf {
        self.root.join("data.qcow2")
    }
    /// Persistent copy of the var.btrfs that's actually attached to QEMU.
    /// Seeded from the artifact dir's var on first start; preserved across
    /// subsequent starts so /var (Docker volumes, /etc/machine-id, etc.)
    /// survives.
    pub fn var_disk(&self) -> PathBuf {
        self.root.join("var.btrfs")
    }
    /// Reserved for future artifact caching.
    #[allow(dead_code)]
    pub fn kernel(&self) -> PathBuf {
        self.root.join("kernel")
    }
    /// Reserved for future artifact caching.
    #[allow(dead_code)]
    pub fn initramfs(&self) -> PathBuf {
        self.root.join("initramfs")
    }
    pub fn ssh_key(&self) -> PathBuf {
        self.root.join("ssh-key")
    }
    pub fn ssh_pubkey(&self) -> PathBuf {
        self.root.join("ssh-key.pub")
    }
    pub fn ssh_config(&self) -> PathBuf {
        self.root.join("ssh-config")
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.root.join("known_hosts")
    }
    pub fn qmp_socket(&self) -> PathBuf {
        self.root.join("qmp.sock")
    }
    pub fn qga_socket(&self) -> PathBuf {
        self.root.join("qga.sock")
    }
    /// Unix socket for the avocado control plane — a second virtio-serial
    /// port exposed inside the guest as `/dev/virtio-ports/avocado.control`.
    /// Avocado.app (USBHostBridge / ControlPlane) connects to this from
    /// the host side; the in-guest avocado-vm-agent opens the matching
    /// guest-side device for messages like `device_available` /
    /// `device_gone` / `request_twiddle`.
    pub fn control_socket(&self) -> PathBuf {
        self.root.join("control.sock")
    }
    /// Where QEMU serves a chardev on this host: the Unix socket, or on
    /// Windows a loopback TCP port recorded in `<name>.port`.
    fn chardev_endpoint(&self, socket: PathBuf) -> Endpoint {
        if cfg!(windows) {
            Endpoint::TcpPortFile(socket.with_extension("port"))
        } else {
            Endpoint::Unix(socket)
        }
    }
    pub fn qmp_endpoint(&self) -> Endpoint {
        self.chardev_endpoint(self.qmp_socket())
    }
    pub fn qga_endpoint(&self) -> Endpoint {
        self.chardev_endpoint(self.qga_socket())
    }
    pub fn control_endpoint(&self) -> Endpoint {
        self.chardev_endpoint(self.control_socket())
    }
    pub fn serial_log(&self) -> PathBuf {
        self.root.join("serial.log")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("qemu.pid")
    }
    pub fn tpm_dir(&self) -> PathBuf {
        self.root.join("tpm")
    }
    pub fn tpm_socket(&self) -> PathBuf {
        self.root.join("tpm").join("swtpm-sock")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("lock")
    }
    pub fn ssh_port_file(&self) -> PathBuf {
        self.root.join("ssh-port")
    }
    /// Local Unix socket that forwards to `/run/docker.sock` inside the VM
    /// (over an SSH `-L` tunnel managed by [`super::forward`]).
    pub fn docker_socket(&self) -> PathBuf {
        self.root.join("docker.sock")
    }
    /// Windows only: loopback TCP port the docker forward listens on, as
    /// decimal text. `ssh -L` cannot bind a Unix socket there and
    /// `docker.exe` does not speak `unix://`, so the forward lands on TCP.
    pub fn docker_port_file(&self) -> PathBuf {
        self.root.join("docker.port")
    }
    /// The `DOCKER_HOST` value that reaches the VM's dockerd through the
    /// forward: `unix://<docker.sock>`, or on Windows `tcp://127.0.0.1:<port>`
    /// (`None` there until the forward has recorded its port).
    pub fn docker_host(&self) -> Option<String> {
        if cfg!(windows) {
            let raw = std::fs::read_to_string(self.docker_port_file()).ok()?;
            let port: u16 = raw.trim().parse().ok()?;
            Some(format!("tcp://127.0.0.1:{port}"))
        } else {
            Some(format!("unix://{}", self.docker_socket().display()))
        }
    }
    /// Internal-only landing point for the supervisor-managed SSH `-L`
    /// tunnel to `/run/docker.sock`. Lives next to `docker.sock` but
    /// distinct so the user-facing socket can stay alive (owned by the
    /// supervisor) while this one comes and goes with VM wake/pause.
    pub fn docker_socket_internal(&self) -> PathBuf {
        self.root.join("docker.sock.internal")
    }
    /// PID of the SSH process maintaining the docker socket forward.
    pub fn forwarder_pid(&self) -> PathBuf {
        self.root.join("forwarder.pid")
    }
    /// PID of the hibernation supervisor (TCP proxy + QMP stop/cont).
    /// Distinct from `pid_file` (QEMU) so the lifecycle layer can tear
    /// down the supervisor before QEMU on shutdown.
    pub fn supervisor_pid(&self) -> PathBuf {
        self.root.join("supervisor.pid")
    }
    /// Append-only log for the supervisor's pause/resume events. Tail
    /// this while reproducing a hibernation issue — every QMP stop /
    /// cont and every accept hits this file.
    pub fn supervisor_log(&self) -> PathBuf {
        self.root.join("supervisor.log")
    }
    /// Loopback-only port QEMU's SSH hostfwd binds to. The supervisor
    /// listens on the user-facing `ssh-port` and proxies to this one;
    /// callers never connect here directly.
    pub fn internal_ssh_port_file(&self) -> PathBuf {
        self.root.join("internal-ssh-port")
    }
    /// Supervisor's "infrastructure" SSH lane — a second TCP listener
    /// that proxies to QEMU's internal hostfwd identically to the
    /// user-facing port, BUT does not count toward the idle-hibernation
    /// activity tracker. Long-lived telemetry channels (Avocado.app's
    /// agent SSH tunnel, future event-stream consumers) connect here
    /// so they wake the VM on attach but don't pin it awake forever.
    pub fn infra_ssh_port_file(&self) -> PathBuf {
        self.root.join("infra-ssh-port")
    }
    /// "Infrastructure" docker socket. Same backing SSH tunnel as
    /// `docker_socket()`, but connections here don't count toward idle
    /// — meant for streaming subscriptions like `GET /events` that
    /// stay open for the VM's lifetime.
    pub fn docker_socket_stream(&self) -> PathBuf {
        self.root.join("docker-stream.sock")
    }
    /// Absolute path to the artifact directory that was last used for `vm
    /// start`. The macOS Avocado.app reads this when launched without an
    /// AVOCADO_VM_DIR env var (Finder/Dock launches inherit a sanitized env
    /// from LaunchServices).
    pub fn artifact_dir_file(&self) -> PathBuf {
        self.root.join("artifact-dir")
    }
    /// Persistent VM configuration (DNS, future network knobs, etc.). Both
    /// avocado-cli (`vm config set/get`) and Avocado.app's settings UI
    /// read/write this file.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.yaml")
    }
    /// Cache directory for patched DTBs (one per `(arch, smp, memory_mib,
    /// qemu_version)` combination). Tiny files (~10 KB each), no eviction
    /// needed.
    pub fn dtb_cache_dir(&self) -> PathBuf {
        self.root.join("dtb")
    }
}

/// Find the user's home directory. Wraps the `directories` crate so callers
/// don't have to depend on it.
fn dirs_home() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .context("could not determine home directory; is $HOME set?")?;
    Ok(dirs.home_dir().to_path_buf())
}

/// Read a previously-written `ssh-port` file. Returns `None` if missing.
pub fn read_ssh_port(paths: &VmPaths) -> Result<Option<u16>> {
    let p = paths.ssh_port_file();
    if !p.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&p).with_context(|| format!("failed to read {}", p.display()))?;
    let port: u16 = raw
        .trim()
        .parse()
        .with_context(|| format!("ssh-port file {} has invalid content", p.display()))?;
    Ok(Some(port))
}

/// Write the ssh port file. Atomic via `tempfile::persist`.
pub fn write_ssh_port(paths: &VmPaths, port: u16) -> Result<()> {
    paths.ensure()?;
    let mut tmp = tempfile::NamedTempFile::new_in(&paths.root)
        .context("failed to create temp file for ssh-port")?;
    use std::io::Write;
    writeln!(tmp, "{port}").context("failed to write ssh-port temp file")?;
    tmp.persist(paths.ssh_port_file())
        .context("failed to persist ssh-port file")?;
    Ok(())
}

/// Read the QEMU pid, if a pidfile is present.
pub fn read_pid(paths: &VmPaths) -> Result<Option<u32>> {
    let p = paths.pid_file();
    if !p.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&p).with_context(|| format!("failed to read {}", p.display()))?;
    let pid: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("pidfile {} has invalid content", p.display()))?;
    Ok(Some(pid))
}

/// Is the process at this pid still alive? Cross-platform best-effort.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // signal 0 is "check liveness without sending"; returns Ok if process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // A process handle is signaled once the process has exited, so a
        // zero-length wait that times out means it is still running.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            alive
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Stop children spawned from now on inheriting this process's own
/// stdin/stdout/stderr. Call before spawning a process meant to outlive the
/// CLI (QEMU, the ssh forwarder).
///
/// Windows hands every inheritable handle to a child, not just the three
/// it is given as stdio. Our own stdout is inheritable (that is how we got
/// it), so without this a detached QEMU keeps the write end of whatever
/// pipe the CLI was started with, and anything reading `avocado vm start`
/// to EOF — a shell pipeline, Avocado Desktop — waits for as long as the VM
/// runs. Later children that are given `Stdio::inherit()` are unaffected:
/// std duplicates the handle for them.
#[cfg(windows)]
pub fn stop_std_handle_inheritance() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Forcibly end a process on Windows, where there are no signals. The
/// graceful path is QMP `quit` (QEMU) — this is the fallback, and the only
/// way to stop helper processes such as the ssh forwarder.
#[cfg(windows)]
pub fn terminate_pid(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Remove transient state (sockets, pidfiles, ssh-port) on a clean shutdown.
/// Errors are swallowed — best-effort cleanup.
pub fn cleanup_transient(paths: &VmPaths) {
    for p in [
        paths.qmp_socket(),
        paths.qga_socket(),
        paths.control_socket(),
        paths.qmp_endpoint().file().to_path_buf(),
        paths.qga_endpoint().file().to_path_buf(),
        paths.control_endpoint().file().to_path_buf(),
        paths.pid_file(),
        paths.ssh_port_file(),
        paths.lock_file(),
        paths.docker_socket(),
        paths.docker_port_file(),
        paths.docker_socket_internal(),
        paths.docker_socket_stream(),
        paths.forwarder_pid(),
        paths.supervisor_pid(),
        paths.internal_ssh_port_file(),
        paths.infra_ssh_port_file(),
    ] {
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_constructs_under_given_root() {
        use std::path::Path;
        let p = VmPaths::at("/tmp/test-vm");
        assert_eq!(p.manifest(), Path::new("/tmp/test-vm/manifest.json"));
        assert_eq!(p.rootfs(), Path::new("/tmp/test-vm/rootfs.img"));
        assert_eq!(p.qmp_socket(), Path::new("/tmp/test-vm/qmp.sock"));
        assert_eq!(p.ssh_port_file(), Path::new("/tmp/test-vm/ssh-port"));
    }

    #[test]
    fn ssh_port_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = VmPaths::at(tmp.path());
        assert!(read_ssh_port(&paths).unwrap().is_none());
        write_ssh_port(&paths, 51234).unwrap();
        assert_eq!(read_ssh_port(&paths).unwrap(), Some(51234));
    }

    #[test]
    fn pid_round_trip_and_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = VmPaths::at(tmp.path());
        assert!(read_pid(&paths).unwrap().is_none());

        // Write our own pid; we're definitely alive.
        let my_pid = std::process::id();
        std::fs::write(paths.pid_file(), format!("{my_pid}\n")).unwrap();
        assert_eq!(read_pid(&paths).unwrap(), Some(my_pid));
        assert!(pid_alive(my_pid));

        // A pid we're confident is unused.
        assert!(!pid_alive(0xFFFF_FFFE));
    }
}
