//! Workspace 9p share + per-project path translation.
//!
//! Approach: a single 9p share is declared at VM-start time, pointing at the
//! user's workspace root (default `$HOME`, override via `AVOCADO_VM_WORKSPACE`).
//! After boot, the CLI SSH-mounts it at `/mnt/workspace` once per VM lifetime.
//! Every project the CLI operates on must live under this workspace root; we
//! translate `<workspace_root>/foo/bar` (on the host) to `/mnt/workspace/foo/bar`
//! (in the VM) and substitute that into `docker run -v` args.
//!
//! This trades dynamic per-project hot-plug for a much simpler model that
//! doesn't depend on runtime `fsdev_add` (which has been unreliable across
//! QEMU versions). If a user needs a workspace outside `$HOME`, they set
//! `AVOCADO_VM_WORKSPACE` before `avocado vm start`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::ssh::SshTarget;
use super::state::VmPaths;

/// In-VM mount point for the workspace 9p share. `/run` is tmpfs on
/// systemd-based systems (including Avocado), so it's always writable and
/// `mkdir` works without any pre-boot setup. We don't use `/mnt` because
/// that's read-only in the Avocado rootfs — making it writable requires a
/// systemd-unit tmpfs overlay that has to be started before the confext
/// providing it is merged, which is hard to time correctly.
pub const VM_MOUNT_POINT: &str = "/run/workspace";
/// 9p `mount_tag` matched at QEMU launch.
pub const SHARE_TAG: &str = "workspace";
/// Filename under `~/.avocado/vm/` that records the workspace root.
const WORKSPACE_FILE: &str = "workspace";

/// Resolve the workspace root the avocado-vm should expose.
///
/// Order: explicit override → `$AVOCADO_VM_WORKSPACE` → `$HOME`.
pub fn resolve_workspace(override_value: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_value {
        return canonicalize_existing(p);
    }
    if let Ok(env) = std::env::var("AVOCADO_VM_WORKSPACE") {
        return canonicalize_existing(Path::new(&env));
    }
    let dirs =
        directories::BaseDirs::new().context("could not determine $HOME for default workspace")?;
    Ok(dirs.home_dir().to_path_buf())
}

fn canonicalize_existing(p: &Path) -> Result<PathBuf> {
    if !p.exists() {
        bail!("workspace path {} does not exist", p.display());
    }
    p.canonicalize()
        .with_context(|| format!("canonicalizing {}", p.display()))
}

/// Record the workspace root that the running VM is exposing. Stored next
/// to the rest of the VM state so other CLI invocations can read it.
pub fn record_workspace(paths: &VmPaths, root: &Path) -> Result<()> {
    paths.ensure()?;
    let target = paths.root.join(WORKSPACE_FILE);
    std::fs::write(&target, root.display().to_string())
        .with_context(|| format!("recording workspace root at {}", target.display()))?;
    Ok(())
}

/// Read the previously-recorded workspace root, if any.
pub fn read_recorded_workspace(paths: &VmPaths) -> Result<Option<PathBuf>> {
    let target = paths.root.join(WORKSPACE_FILE);
    if !target.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&target)
        .with_context(|| format!("reading {}", target.display()))?;
    Ok(Some(PathBuf::from(raw.trim())))
}

/// QEMU arg fragments declaring the workspace 9p export.
/// Append these to the rest of the `-fsdev` / `-device` lines.
///
/// Unused on macOS — the spawn step is delegated to Avocado.app, which
/// has its own copy of the arg-building logic in `VMSupervisor.swift`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn qemu_args_for(workspace: &Path) -> Vec<String> {
    vec![
        "-fsdev".to_string(),
        format!(
            "local,id={SHARE_TAG},path={},security_model=mapped-xattr",
            workspace.display()
        ),
        "-device".to_string(),
        format!("virtio-9p-pci,fsdev={SHARE_TAG},mount_tag={SHARE_TAG}"),
    ]
}

/// Mount `/mnt/workspace` inside the guest if not already mounted.
/// Idempotent: re-running is cheap and safe.
pub async fn ensure_mounted_in_guest(target: &SshTarget) -> Result<()> {
    let check = target
        .exec(&format!(
            "if mountpoint -q {VM_MOUNT_POINT}; then echo MOUNTED; else echo MISSING; fi"
        ))
        .await?;
    if check.0.trim() == "MOUNTED" {
        return Ok(());
    }
    let cmd = format!(
        "mkdir -p {VM_MOUNT_POINT} && \
         mount -t 9p -o trans=virtio,version=9p2000.L,access=any {SHARE_TAG} {VM_MOUNT_POINT}"
    );
    target
        .exec(&cmd)
        .await
        .with_context(|| format!("failed to mount {SHARE_TAG} at {VM_MOUNT_POINT} in VM"))?;
    Ok(())
}

/// Translate a host path into its in-VM equivalent under
/// `/mnt/workspace/`. Returns an error if the host path isn't under the
/// workspace root.
pub fn translate_to_vm(host_path: &Path, workspace_root: &Path) -> Result<PathBuf> {
    let canon = host_path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", host_path.display()))?;
    let rel = canon.strip_prefix(workspace_root).with_context(|| {
        format!(
            "{} is not under workspace root {}; set AVOCADO_VM_WORKSPACE to a directory that contains it, \
             then `avocado vm stop && avocado vm start`",
            canon.display(),
            workspace_root.display(),
        )
    })?;
    Ok(Path::new(VM_MOUNT_POINT).join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_paths_under_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let sub = workspace.join("project-a");
        std::fs::create_dir_all(&sub).unwrap();

        let translated = translate_to_vm(&sub, &workspace).unwrap();
        assert_eq!(translated, Path::new("/run/workspace/project-a"));
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let err = translate_to_vm(tmp_b.path(), tmp_a.path()).unwrap_err();
        assert!(format!("{err:#}").contains("not under workspace root"));
    }

    #[test]
    fn qemu_args_carry_tag_and_path() {
        let args = qemu_args_for(Path::new("/Users/foo"));
        let joined = args.join(" ");
        assert!(joined.contains("mount_tag=workspace"));
        assert!(joined.contains("path=/Users/foo"));
        assert!(joined.contains("security_model=mapped-xattr"));
    }

    #[test]
    fn workspace_record_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = VmPaths::at(tmp.path());
        assert!(read_recorded_workspace(&paths).unwrap().is_none());
        record_workspace(&paths, Path::new("/home/foo/src")).unwrap();
        assert_eq!(
            read_recorded_workspace(&paths).unwrap().as_deref(),
            Some(Path::new("/home/foo/src"))
        );
    }

    #[test]
    fn resolve_workspace_prefers_explicit_then_env_then_home() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_workspace(Some(tmp.path())).unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());

        // env path
        std::env::remove_var("AVOCADO_VM_WORKSPACE");
        std::env::set_var("AVOCADO_VM_WORKSPACE", tmp.path());
        let resolved = resolve_workspace(None).unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
        std::env::remove_var("AVOCADO_VM_WORKSPACE");
    }
}
