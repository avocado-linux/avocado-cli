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
/// Guest mount options for the share.
///
/// `cache=mmap` lets reads and writes use the guest page cache. Without it
/// the client sends one 4 KiB request per page: 256 MiB took about 60 s to
/// read and 100 s to write, against under 0.2 s with it. It does not cache
/// metadata, so edits, deletes and renames made on the host show up at once.
/// The metadata cache modes (`loose`, or any value with bit 0x2) are much
/// faster for small files but serve stale data after host edits, and some
/// guest readers (`sdk run -i` shells, the in-VM agent) never get a chance
/// to drop caches. `msize=512000` is the largest value the virtio transport
/// accepts; the kernel caps larger values to it.
pub const MOUNT_OPTS: &str = "trans=virtio,version=9p2000.L,access=any,msize=512000,cache=mmap";
/// Filename under `~/.avocado/vm/` that records the workspace root.
const WORKSPACE_FILE: &str = "workspace";

/// Resolve the workspace root the avocado-vm should expose.
///
/// Order: explicit override → `$AVOCADO_VM_WORKSPACE` → the default root.
///
/// The default is `$HOME`, except on Windows where it is
/// `%USERPROFILE%\Avocado` (created if missing). The Windows share is an
/// SMB export that needs an access-control entry on the shared folder for
/// its service account, and doing that to a whole user profile is far more
/// invasive than the 9p export of `$HOME` elsewhere. Projects kept
/// somewhere else need `AVOCADO_VM_WORKSPACE` pointed at a folder that
/// contains them, once.
///
/// Known gap on Windows: the CLI also bind-mounts files of its own that
/// live outside that folder (the signing keys under `%APPDATA%`, helper
/// scripts under `%TEMP%`). Those mounts do not resolve in the VM unless
/// the workspace root is widened to contain them.
pub fn resolve_workspace(override_value: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_value {
        return canonicalize_existing(p);
    }
    if let Ok(env) = std::env::var("AVOCADO_VM_WORKSPACE") {
        return canonicalize_existing(Path::new(&env));
    }
    let dirs =
        directories::BaseDirs::new().context("could not determine $HOME for default workspace")?;
    if cfg!(windows) {
        let root = dirs.home_dir().join("Avocado");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating default workspace {}", root.display()))?;
        return canonicalize_existing(&root);
    }
    Ok(dirs.home_dir().to_path_buf())
}

fn canonicalize_existing(p: &Path) -> Result<PathBuf> {
    if !p.exists() {
        bail!("workspace path {} does not exist", p.display());
    }
    p.canonicalize()
        .map(|c| without_verbatim_prefix(&c))
        .with_context(|| format!("canonicalizing {}", p.display()))
}

/// Drop the `\\?\` prefix Windows' `canonicalize` puts on a drive path, so
/// `\\?\C:\Users\me` and `C:\Users\me` compare and print as the same
/// thing. Anything else (Unix paths, `\\?\UNC\…`) is returned unchanged.
fn without_verbatim_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
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

/// Whether this host's QEMU can export a directory over virtio-9p.
/// Windows builds of QEMU are compiled without `-fsdev`, so there is no
/// workspace share there yet.
pub fn host_supports_9p() -> bool {
    !cfg!(windows)
}

/// QEMU arg fragments declaring the workspace 9p export.
/// Append these to the rest of the `-fsdev` / `-device` lines.
///
/// `mapped-xattr` makes QEMU create host files as its own user and keep the
/// guest's owner in an xattr. The plain `/opt/src` mount under the VM
/// (`container::src_mount_uid_gid`) depends on that; change both together.
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
///
/// Over virtio-9p where the host's QEMU can export a directory; on Windows
/// over SMB from the host's own file server (see [`smb`]).
pub async fn ensure_mounted_in_guest(target: &SshTarget) -> Result<()> {
    let check = target
        .exec(&format!(
            "if mountpoint -q {VM_MOUNT_POINT}; then echo MOUNTED; else echo MISSING; fi"
        ))
        .await?;
    if check.0.trim() == "MOUNTED" {
        return Ok(());
    }
    if !host_supports_9p() {
        return smb::mount_in_guest(target).await;
    }
    let cmd = format!(
        "mkdir -p {VM_MOUNT_POINT} && \
         mount -t 9p -o {MOUNT_OPTS} {SHARE_TAG} {VM_MOUNT_POINT}"
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
        .map(|c| without_verbatim_prefix(&c))
        .with_context(|| format!("canonicalizing {}", host_path.display()))?;
    let workspace_root = &without_verbatim_prefix(workspace_root);
    let rel = canon.strip_prefix(workspace_root).with_context(|| {
        format!(
            "{} is not under workspace root {}; set AVOCADO_VM_WORKSPACE to a directory that contains it, \
             then `avocado vm stop && avocado vm start`",
            canon.display(),
            workspace_root.display(),
        )
    })?;
    // A guest path: always `/`-separated, whatever the host's separator.
    let mut vm = String::from(VM_MOUNT_POINT);
    for part in rel.components() {
        vm.push('/');
        vm.push_str(&part.as_os_str().to_string_lossy());
    }
    Ok(PathBuf::from(vm))
}

/// The Windows workspace share: SMB from the host's file server.
///
/// Windows builds of QEMU have no virtio-9p, so there the guest mounts the
/// workspace with the CIFS client built into its kernel, from the host's
/// own SMB server, which the VM's user-mode network reaches at 10.0.2.2.
/// One share covers the whole workspace root, so no project needs sharing
/// on its own.
///
/// Host setup is one-time and needs elevation (Windows asks once): a local
/// account used only for this, Modify rights for it on the workspace
/// folder, and a share open to that account alone. See
/// `smb_share_setup.ps1`. The account's random password is kept in
/// `share.cred` in the VM state dir, DPAPI-encrypted for the current user.
pub mod smb {
    use super::*;

    pub const SHARE: &str = "avocado-workspace";
    pub const USER: &str = "avocado-vm";
    /// Guest mount options. `uid`/`gid`/modes present every file as
    /// root-owned 0755 (the share carries no Unix ownership), `mfsymlinks`
    /// emulates symlinks in plain files, `nobrl` keeps byte-range locks in
    /// the guest so tools that lock a file do not fail on the server.
    pub const MOUNT_OPTS: &str =
        "vers=3.1.1,uid=0,gid=0,file_mode=0755,dir_mode=0755,noperm,mfsymlinks,nobrl,cache=strict,actimeo=1";
    const SETUP_SCRIPT: &str = include_str!("smb_share_setup.ps1");

    fn credential_file(paths: &VmPaths) -> PathBuf {
        paths.root.join("share.cred")
    }

    /// The guest-side script that mounts the share. Carries the password,
    /// so it goes to the guest over stdin, never on a command line.
    pub fn guest_mount_script(computer: &str, password: &str) -> String {
        format!(
            "set -e\n\
             mkdir -p {VM_MOUNT_POINT}\n\
             mountpoint -q {VM_MOUNT_POINT} && exit 0\n\
             mount -t cifs //10.0.2.2/{SHARE} {VM_MOUNT_POINT} \
             -o username={USER},password={password},domain={computer},{MOUNT_OPTS}\n"
        )
    }

    /// The folder `net share <name>` reports for the share, if it exists.
    pub fn parse_net_share_path(output: &str) -> Option<PathBuf> {
        output.lines().find_map(|line| {
            let rest = line.strip_prefix("Path")?;
            let value = rest.trim();
            (!value.is_empty() && rest.starts_with(char::is_whitespace))
                .then(|| PathBuf::from(value))
        })
    }

    async fn powershell(args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
        ]);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::null());
        cmd.output().await.context("failed to run powershell")
    }

    /// Whether the host share exists and points at `workspace`.
    async fn share_is_current(workspace: &Path) -> bool {
        let out = tokio::process::Command::new("net")
            .args(["share", SHARE])
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        let Ok(out) = out else { return false };
        if !out.status.success() {
            return false;
        }
        let shared = parse_net_share_path(&String::from_utf8_lossy(&out.stdout));
        shared.is_some_and(|p| {
            p.to_string_lossy()
                .eq_ignore_ascii_case(&workspace.to_string_lossy())
        })
    }

    /// Single-quote `s` for a PowerShell command string.
    fn ps_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// Run the elevated one-time setup. Windows shows its approval prompt
    /// unless the CLI is already running elevated.
    async fn run_setup(paths: &VmPaths, workspace: &Path) -> Result<()> {
        crate::utils::output::print_info_stderr(&format!(
            "Setting up the workspace share for {} (one time; Windows will ask for \
             administrator approval)…",
            workspace.display()
        ));
        let script = paths.root.join("smb_share_setup.ps1");
        std::fs::write(&script, SETUP_SCRIPT)
            .with_context(|| format!("writing {}", script.display()))?;
        let result = paths.root.join("share-setup.result");
        let _ = std::fs::remove_file(&result);

        // The elevated child's own argument list: every value double-quoted.
        let child_args = [
            "-NoProfile".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            format!("\"{}\"", script.display()),
            "-Workspace".to_string(),
            format!("\"{}\"", workspace.display()),
            "-CredentialFile".to_string(),
            format!("\"{}\"", credential_file(paths).display()),
            "-ResultFile".to_string(),
            format!("\"{}\"", result.display()),
        ]
        .join(" ");
        let command = format!(
            "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList {}",
            ps_quote(&child_args)
        );
        let out = powershell(&["-Command", &command]).await?;
        let status = std::fs::read_to_string(&result).unwrap_or_default();
        let _ = std::fs::remove_file(&result);
        let _ = std::fs::remove_file(&script);
        if status.trim() == "ok" {
            return Ok(());
        }
        if status.trim().is_empty() {
            bail!(
                "workspace share setup did not run (was the administrator prompt declined?): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        bail!("workspace share setup failed: {}", status.trim());
    }

    /// Read the share account's password back out of the DPAPI-protected
    /// credential file.
    async fn read_password(paths: &VmPaths) -> Result<String> {
        let command = format!(
            "$s = Get-Content -LiteralPath {} | ConvertTo-SecureString; \
             [Net.NetworkCredential]::new('', $s).Password",
            ps_quote(&credential_file(paths).to_string_lossy())
        );
        let out = powershell(&["-Command", &command]).await?;
        let password = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || password.is_empty() {
            bail!("could not read the workspace share credential");
        }
        Ok(password)
    }

    /// Make sure the host share exists for the recorded workspace, then
    /// mount it in the guest. If the mount is refused (a stale password),
    /// the host setup is redone once and the mount retried.
    pub async fn mount_in_guest(target: &SshTarget) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let workspace =
            read_recorded_workspace(&paths)?.context("no workspace recorded for the running VM")?;
        let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| ".".to_string());

        let mut fresh_setup = false;
        if !credential_file(&paths).is_file() || !share_is_current(&workspace).await {
            run_setup(&paths, &workspace).await?;
            fresh_setup = true;
        }
        let password = read_password(&paths).await?;
        let first = target
            .exec_script(&guest_mount_script(&computer, &password))
            .await;
        match first {
            Ok(_) => Ok(()),
            Err(e) if fresh_setup => {
                Err(e).context(format!("mounting the workspace share at {VM_MOUNT_POINT}"))
            }
            Err(_) => {
                run_setup(&paths, &workspace).await?;
                let password = read_password(&paths).await?;
                target
                    .exec_script(&guest_mount_script(&computer, &password))
                    .await
                    .map(|_| ())
                    .with_context(|| format!("mounting the workspace share at {VM_MOUNT_POINT}"))
            }
        }
    }
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

    /// A Windows host path becomes a `/`-separated guest path, and the
    /// `\\?\` form `canonicalize` produces matches a plainly recorded root.
    #[cfg(windows)]
    #[test]
    fn translates_windows_paths_to_guest_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = canonicalize_existing(tmp.path()).unwrap();
        assert!(!workspace.to_string_lossy().starts_with(r"\\?\"));
        let sub = workspace.join("Projects").join("demo");
        std::fs::create_dir_all(&sub).unwrap();

        let translated = translate_to_vm(&sub, &workspace).unwrap();
        assert_eq!(translated.to_string_lossy(), "/run/workspace/Projects/demo");
        // A root recorded in verbatim form (older state) still matches.
        let verbatim = PathBuf::from(format!(r"\\?\{}", workspace.display()));
        assert_eq!(
            translate_to_vm(&sub, &verbatim).unwrap().to_string_lossy(),
            "/run/workspace/Projects/demo"
        );
    }

    #[test]
    fn verbatim_prefix_is_dropped_only_from_drive_paths() {
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\C:\Users\me")),
            PathBuf::from(r"C:\Users\me")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\?\UNC\server\share")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new("/home/me")),
            PathBuf::from("/home/me")
        );
    }

    #[test]
    fn smb_share_path_is_read_from_net_share_output() {
        let out = "Share name        avocado-workspace\r\n\
                   Path              C:\\Users\\me\\Avocado\r\n\
                   Remark            Avocado VM workspace\r\n\
                   Maximum users     No limit\r\n";
        assert_eq!(
            smb::parse_net_share_path(out),
            Some(PathBuf::from(r"C:\Users\me\Avocado"))
        );
        assert_eq!(
            smb::parse_net_share_path("This shared resource does not exist."),
            None
        );
        // A value line that merely starts with "Path" is not the field.
        assert_eq!(smb::parse_net_share_path("Pathological  x"), None);
    }

    #[test]
    fn smb_guest_script_mounts_the_one_workspace_share() {
        let script = smb::guest_mount_script("MY-PC", "s3cr3t");
        assert!(script.contains("mount -t cifs //10.0.2.2/avocado-workspace /run/workspace"));
        assert!(script.contains("username=avocado-vm,password=s3cr3t,domain=MY-PC,"));
        assert!(script.contains("mfsymlinks"));
        // Idempotent: a second run with the share mounted does nothing.
        assert!(script.contains("mountpoint -q /run/workspace && exit 0"));
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
    fn mount_opts_use_page_cache_without_metadata_cache() {
        assert!(MOUNT_OPTS.contains("cache=mmap"));
        assert!(MOUNT_OPTS.contains("msize=512000"));
        assert!(!MOUNT_OPTS.contains("loose"));
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
        assert_eq!(resolved, canonicalize_existing(tmp.path()).unwrap());

        // env path
        std::env::remove_var("AVOCADO_VM_WORKSPACE");
        std::env::set_var("AVOCADO_VM_WORKSPACE", tmp.path());
        let resolved = resolve_workspace(None).unwrap();
        assert_eq!(resolved, canonicalize_existing(tmp.path()).unwrap());
        std::env::remove_var("AVOCADO_VM_WORKSPACE");
    }
}
