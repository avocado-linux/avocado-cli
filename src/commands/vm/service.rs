//! `avocado vm service install|uninstall|status` — run the VM headless under
//! a systemd user unit.
//!
//! The unit is a thin wrapper over `vm start` / `vm stop`: `vm start` already
//! daemonizes QEMU and the supervisor (setsid) and writes `qemu.pid`, so the
//! unit is `Type=forking` on that pidfile. setsid does not leave the unit's
//! cgroup, so systemd reaps QEMU, the supervisor and swtpm on stop. Idle
//! hibernation is forced off: a hibernated VM drops its Connect session.
//!
//! ponytail: systemd user unit only; a system unit and launchd come later.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::{lifecycle, state::VmPaths};

pub const UNIT_NAME: &str = "avocado-vm.service";

pub enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

pub struct ServiceCommand {
    pub action: ServiceAction,
}

impl ServiceCommand {
    pub async fn execute(self) -> Result<()> {
        match self.action {
            ServiceAction::Install => install().await,
            ServiceAction::Uninstall => uninstall(),
            ServiceAction::Status => status().await,
        }
    }
}

fn unit_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("could not determine home directory")?;
    Ok(dirs
        .config_dir()
        .join("systemd")
        .join("user")
        .join(UNIT_NAME))
}

/// Render the unit. `exe` is the running binary (not whatever `avocado` is on
/// PATH), and `path` is the installing shell's PATH so qemu, swtpm and ssh
/// resolve the same way they do interactively.
pub fn render_unit(exe: &Path, pid_file: &Path, path: &str) -> String {
    let exe = exe.display();
    format!(
        "[Unit]
Description=Avocado VM (headless, managed by avocado vm service)

[Service]
Type=forking
PIDFile={pid}
Environment=AVOCADO_VM_IDLE_HIBERNATE_SECS=0
Environment=PATH={path}
ExecStart={exe} vm start
ExecStop={exe} vm stop
TimeoutStartSec=600
TimeoutStopSec=120
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
",
        pid = pid_file.display(),
    )
}

pub(crate) fn systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl")?;
    if !st.success() {
        bail!("systemctl --user {} failed ({st})", args.join(" "));
    }
    Ok(())
}

pub(crate) fn service_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn linger_enabled() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

async fn install() -> Result<()> {
    // A VM started by hand lives outside the unit's cgroup; systemd would
    // refuse its pidfile. Make the user hand it over explicitly.
    if lifecycle::status().await?.running && !service_active() {
        bail!("the VM is running outside the service; stop it first with `avocado vm stop`");
    }
    let paths = VmPaths::resolve()?;
    let exe = std::env::current_exe().context("cannot locate the running avocado binary")?;
    let unit = unit_path()?;
    std::fs::create_dir_all(unit.parent().unwrap())?;
    std::fs::write(
        &unit,
        render_unit(
            &exe,
            &paths.pid_file(),
            &std::env::var("PATH").unwrap_or_default(),
        ),
    )
    .with_context(|| format!("failed to write {}", unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT_NAME])?;
    print_success(
        &format!("{UNIT_NAME} installed and started ({})", unit.display()),
        OutputLevel::Normal,
    );
    if !linger_enabled() {
        print_info(
            "User lingering is off, so the VM stops at logout and does not start at boot. \
             Enable it once with: sudo loginctl enable-linger $USER",
            OutputLevel::Normal,
        );
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let unit = unit_path()?;
    if !unit.exists() {
        print_info("service is not installed", OutputLevel::Normal);
        return Ok(());
    }
    systemctl(&["disable", "--now", UNIT_NAME])?;
    std::fs::remove_file(&unit)?;
    systemctl(&["daemon-reload"])?;
    print_success(&format!("{UNIT_NAME} removed"), OutputLevel::Normal);
    Ok(())
}

async fn status() -> Result<()> {
    if !unit_path()?.exists() {
        println!("service: not installed");
    } else {
        // systemctl status exits non-zero for inactive units; its output is
        // the status, so don't treat that as an error.
        let _ = Command::new("systemctl")
            .args(["--user", "status", "--no-pager", "--lines=0", UNIT_NAME])
            .status();
        println!("linger: {}", if linger_enabled() { "on" } else { "off" });
    }
    crate::commands::vm::status::StatusCommand.execute().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_the_installing_binary_with_hibernation_off() {
        let u = render_unit(
            Path::new("/opt/bin/avocado"),
            Path::new("/home/u/.avocado/vm/qemu.pid"),
            "/usr/bin:/bin",
        );
        assert!(u.contains("Type=forking\n"));
        assert!(u.contains("PIDFile=/home/u/.avocado/vm/qemu.pid\n"));
        assert!(u.contains("ExecStart=/opt/bin/avocado vm start\n"));
        assert!(u.contains("ExecStop=/opt/bin/avocado vm stop\n"));
        assert!(u.contains("Environment=AVOCADO_VM_IDLE_HIBERNATE_SECS=0\n"));
        assert!(u.contains("Environment=PATH=/usr/bin:/bin\n"));
        assert!(u.contains("WantedBy=default.target\n"));
    }
}
