//! Docker routing: set `DOCKER_HOST=ssh://…` (and `DOCKER_CLI_SSH_OPTS`) so
//! every spawned `docker` subprocess talks to the avocado-vm's dockerd over
//! SSH instead of the local Docker Desktop daemon.
//!
//! The injection is done by `set_process_env_for_routing` once at the top
//! of `main.rs`, before dispatch. From then on every `Command::new("docker")`
//! site inherits the env naturally — no per-site changes needed.
//!
//! Activation gates:
//!   1. Host is macOS or Windows (`is_docker_desktop()`).
//!   2. `AVOCADO_VM_AUTO_START` is not `"0"` and not `"false"`.
//!   3. The user did **not** pass `--runs-on` (legacy remote-docker path wins).
//!   4. Either the VM is already running, OR `AVOCADO_VM_DIR` is set so
//!      we can auto-start it.
//!
//! Anything falling outside these gates is a no-op: legacy Docker Desktop
//! behavior is preserved verbatim.

use anyhow::Result;
use std::path::PathBuf;

use crate::utils::container::is_docker_desktop;
use crate::utils::output::{print_info, print_warning, OutputLevel};
use crate::utils::vm::lifecycle::{self, StartOptions};
use crate::utils::vm::manifest::Manifest;
use crate::utils::vm::ssh::SshTarget;
use crate::utils::vm::state::{self, VmPaths};

/// Whether VM routing should be considered for this invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    /// Not on a Docker-Desktop host — pass through.
    NotApplicable,
    /// On macOS/Windows but the user opted out via env / flag / --runs-on.
    OptedOut,
    /// Apply: route through the avocado-vm.
    Apply,
}

/// Resolve the routing mode for the current process.
pub fn resolve_mode(disable_via_flag: bool, runs_on_set: bool) -> RoutingMode {
    if !is_docker_desktop() {
        return RoutingMode::NotApplicable;
    }
    if disable_via_flag {
        return RoutingMode::OptedOut;
    }
    if runs_on_set {
        return RoutingMode::OptedOut;
    }
    if env_disabled() {
        return RoutingMode::OptedOut;
    }
    RoutingMode::Apply
}

fn env_disabled() -> bool {
    match std::env::var("AVOCADO_VM_AUTO_START") {
        Ok(v) => matches!(v.as_str(), "0" | "false" | "FALSE" | "no" | "NO"),
        Err(_) => false,
    }
}

/// Ensure the VM is running (auto-start if needed), then set
/// `DOCKER_HOST` and `DOCKER_CLI_SSH_OPTS` for the rest of the process.
///
/// Caller passes `disable_via_flag = cli.no_vm_auto_start` and
/// `runs_on_set = cli.runs_on.is_some()` so flag handling stays in main.
pub async fn ensure_routed_for_process(
    disable_via_flag: bool,
    runs_on_set: bool,
) -> Result<RoutingMode> {
    let mode = resolve_mode(disable_via_flag, runs_on_set);
    match mode {
        RoutingMode::Apply => {}
        _ => return Ok(mode),
    }

    let paths = VmPaths::resolve()?;

    // 1. If a VM is already running, use it.
    let running = state::read_pid(&paths)?
        .map(state::pid_alive)
        .unwrap_or(false);
    let port = if running {
        // Manifest-staleness check: warn if the user's AVOCADO_VM_DIR points
        // at a newer manifest than the one we recorded at start time.
        warn_if_stale(&paths);
        state::read_ssh_port(&paths)?
            .ok_or_else(|| anyhow::anyhow!("avocado-vm is running but ssh-port file is missing"))?
    } else {
        // 2. Auto-start path.
        let Some(vm_source) = vm_source_from_env() else {
            // Can't auto-start without artifacts. Print a clear hint and
            // proceed without routing — user can also opt out explicitly.
            print_warning(
                "avocado-vm not running and AVOCADO_VM_DIR is unset; falling back to local docker. \
                 Set AVOCADO_VM_DIR or run `avocado vm start --vm-source <dir>` to enable VM routing.",
                OutputLevel::Normal,
            );
            return Ok(RoutingMode::OptedOut);
        };
        print_info(
            &format!("Starting avocado-vm from {}…", vm_source.display()),
            OutputLevel::Normal,
        );
        let status = lifecycle::start(StartOptions {
            vm_source,
            memory_mib: 4096,
            cpus: 4,
            ssh_port: None,
            cmdline_extra: None,
            workspace: None,
            var_size: None,
            dns_override: None,
        })
        .await?;
        status
            .ssh_port
            .ok_or_else(|| anyhow::anyhow!("avocado-vm started without an ssh-port"))?
    };

    let _ = port; // forwarder was set up at vm-start; we just need its socket path
    let _ = SshTarget::local(&paths, port); // kept for `vm shell` path

    // The avocado-vm sets up a local Unix-socket SSH forward to the VM's
    // /run/docker.sock at start time (see [`super::forward`]). Pointing
    // DOCKER_HOST at that socket means every spawned `docker` subprocess
    // just talks to a Unix socket — no ssh transport in the docker client,
    // no ssh-config modifications, no host-key verification edge cases.
    let socket = paths.docker_socket();
    if !socket.exists() {
        // Be loud — auto-routing without a working socket leaves the user
        // hitting "Cannot connect to the Docker daemon" mysteriously.
        print_warning(
            &format!(
                "docker socket forward {} is missing; the VM may not be fully up or the forwarder failed to start. \
                 Run `avocado vm stop && avocado vm start` to retry.",
                socket.display()
            ),
            OutputLevel::Normal,
        );
        return Ok(RoutingMode::OptedOut);
    }
    std::env::set_var("DOCKER_HOST", format!("unix://{}", socket.display()));
    Ok(RoutingMode::Apply)
}

/// Read AVOCADO_VM_DIR from env if it points at an extant directory.
fn vm_source_from_env() -> Option<PathBuf> {
    let raw = std::env::var("AVOCADO_VM_DIR").ok()?;
    let p = PathBuf::from(raw);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// If the user's AVOCADO_VM_DIR manifest disagrees with the one we recorded
/// at start time, print a warning. Don't fail — they may have a reason.
fn warn_if_stale(paths: &VmPaths) {
    let Some(src) = vm_source_from_env() else {
        return;
    };
    let src_manifest = src.join("manifest.json");
    if !src_manifest.exists() || !paths.manifest().exists() {
        return;
    }
    let Ok(latest) = Manifest::load(&src_manifest) else {
        return;
    };
    let Ok(recorded) = Manifest::load(&paths.manifest()) else {
        return;
    };
    let drift = latest
        .artifacts
        .iter()
        .any(|(role, a)| match recorded.artifacts.get(role) {
            Some(b) => !a.sha256.eq_ignore_ascii_case(&b.sha256),
            None => true,
        });
    if drift {
        print_warning(
            &format!(
                "AVOCADO_VM_DIR ({}) has artifacts that differ from the running avocado-vm; \
                 run `avocado vm stop && avocado vm start --vm-source {}` to refresh.",
                src.display(),
                src.display(),
            ),
            OutputLevel::Normal,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env mutation is process-global; serialize the relevant tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_disabled_recognizes_falsy_values() {
        let _g = ENV_LOCK.lock().unwrap();
        for v in ["0", "false", "FALSE", "no", "NO"] {
            std::env::set_var("AVOCADO_VM_AUTO_START", v);
            assert!(env_disabled(), "expected '{v}' to disable VM routing");
        }
        std::env::set_var("AVOCADO_VM_AUTO_START", "1");
        assert!(!env_disabled());
        std::env::remove_var("AVOCADO_VM_AUTO_START");
        assert!(!env_disabled());
    }

    #[test]
    fn resolve_mode_respects_runs_on() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AVOCADO_VM_AUTO_START");
        // On Linux is_docker_desktop() returns false, so resolve always returns NotApplicable.
        // We still verify the OptedOut branches don't accidentally short-circuit.
        if is_docker_desktop() {
            assert_eq!(resolve_mode(false, true), RoutingMode::OptedOut);
            assert_eq!(resolve_mode(true, false), RoutingMode::OptedOut);
        } else {
            assert_eq!(resolve_mode(false, false), RoutingMode::NotApplicable);
            assert_eq!(resolve_mode(true, true), RoutingMode::NotApplicable);
        }
    }

    #[test]
    fn vm_source_from_env_requires_a_directory() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AVOCADO_VM_DIR");
        assert!(vm_source_from_env().is_none());

        std::env::set_var("AVOCADO_VM_DIR", "/this/definitely/does/not/exist");
        assert!(vm_source_from_env().is_none());

        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("AVOCADO_VM_DIR", tmp.path());
        assert_eq!(vm_source_from_env().as_deref(), Some(tmp.path()));

        std::env::remove_var("AVOCADO_VM_DIR");
    }
}
