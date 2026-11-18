//! `avocado feed index <dir>`: write `<dir>/repodata/` for a directory of RPMs
//! by running the host `createrepo_c`. It regenerates the metadata on every
//! run (no `--update`), so a stale `repodata/` is rewritten, never merged.

use anyhow::{bail, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const TOOL: &str = "createrepo_c";

/// The argv (without the program name) that indexes `dir`. The directory is a
/// single argument, never joined into a string, so a path with spaces or shell
/// metacharacters reaches the tool unchanged. A relative path starting with `-`
/// gets a `./` prefix: createrepo_c reads it as an option, and `--` does not help.
///
/// `--revision` plus `--set-timestamp-to-revision` pin every timestamp in the
/// metadata to `revision`, so re-indexing an unchanged directory writes
/// byte-identical `repomd.xml`. A consumer that fingerprints the feed from that
/// file would otherwise see a new value after every re-index. `--quiet` is
/// absent so createrepo_c names the RPM it could not read, and
/// `--error-exit-val` makes older versions fail on such an RPM as newer ones do.
fn index_argv(dir: &Path, revision: u64) -> Vec<OsString> {
    let arg = if dir.is_relative() && dir.as_os_str().as_encoded_bytes().starts_with(b"-") {
        Path::new(".").join(dir).into_os_string()
    } else {
        dir.as_os_str().to_os_string()
    };
    vec![
        OsString::from("--error-exit-val"),
        OsString::from(format!("--revision={revision}")),
        OsString::from("--set-timestamp-to-revision"),
        arg,
    ]
}

/// Newest modification time, in seconds since the epoch, of any `.rpm` under
/// `dir`. Unchanged RPMs give the same revision, so the metadata stays stable.
/// The value is not monotonic: an RPM added with an older mtime (`cp -p`) leaves
/// it alone, and removing the newest one lowers it. `repomd.xml` still changes
/// through the data checksums, which is what a client compares. 0 when the
/// directory holds no RPM. Symlinked directories are not followed, so a link
/// back to an ancestor cannot loop this walk. createrepo_c does follow them,
/// which is why [`run_with`] refuses a [`symlink_cycle`] before spawning it.
fn newest_rpm_mtime(dir: &Path) -> u64 {
    let mut newest = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rpm") {
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let secs = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                newest = newest.max(secs);
            }
        }
    }
    newest
}

/// The first symlinked directory under `dir` that leads back to a directory on
/// its own path: the feed directory, one of its ancestors, or a directory
/// entered through an earlier link. createrepo_c follows symlinked directories
/// and walks such a link forever (its `--skip-symlinks` hangs too), so the
/// cycle has to be found before it runs. A link that leads anywhere else is
/// walked here as createrepo_c would walk it, so a cycle behind it is found
/// too; a link to the same directory twice is a repeat, not a cycle.
fn symlink_cycle(dir: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(dir).ok()?;
    // Every ancestor is on the path: a link to one re-enters the feed.
    let mut chain: Vec<PathBuf> = root.ancestors().map(Path::to_path_buf).collect();
    cycle_under(dir, &root, &mut chain)
}

/// [`symlink_cycle`] below `real`, naming entries under `shown`. `chain` holds
/// the canonical directories on the path from the root to `real`. It
/// terminates because every link target entered is pushed onto `chain` and
/// there are finitely many directories to push.
fn cycle_under(shown: &Path, real: &Path, chain: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(real) else {
        return None;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let shown_child = shown.join(entry.file_name());
        let target = if kind.is_symlink() {
            match std::fs::canonicalize(entry.path()) {
                Ok(t) if t.is_dir() => t,
                _ => continue,
            }
        } else if kind.is_dir() {
            entry.path()
        } else {
            continue;
        };
        if chain.contains(&target) {
            return Some(shown_child);
        }
        chain.push(target.clone());
        let found = cycle_under(&shown_child, &target, chain);
        chain.pop();
        if found.is_some() {
            return found;
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn which_on_path(bin: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let full = dir.join(bin);
        if is_executable_file(&full) {
            return Some(full);
        }
    }
    None
}

/// Core of `run` with PATH and the spawner injected. The directory and cycle
/// checks come first so a bad path fails before the tool is resolved or
/// anything is spawned. `spawn` returns whether the tool exited successfully.
fn run_with<F>(dir: &Path, path: Option<&OsStr>, spawn: F) -> Result<()>
where
    F: FnOnce(&Path, &[OsString]) -> std::io::Result<bool>,
{
    if !dir.is_dir() {
        bail!(
            "cannot index '{}': it does not exist or is not a directory",
            dir.display()
        );
    }
    if let Some(link) = symlink_cycle(dir) {
        bail!(
            "cannot index '{}': '{}' is a symlink back into its own path, and {TOOL} would follow it forever; remove it or point it outside the directory",
            dir.display(),
            link.display()
        );
    }
    let Some(tool) = which_on_path(TOOL, path) else {
        bail!(
            "{TOOL} not found in $PATH; install it (e.g. `pacman -S createrepo_c`, `apt install createrepo-c` or `dnf install createrepo_c`)"
        );
    };
    let argv = index_argv(dir, newest_rpm_mtime(dir));
    match spawn(&tool, &argv) {
        Ok(true) => Ok(()),
        Ok(false) => bail!("{TOOL} failed to index '{}'", dir.display()),
        Err(e) => bail!("could not run {TOOL} ({e})"),
    }
}

pub fn run(dir: &Path) -> Result<()> {
    run_with(dir, std::env::var_os("PATH").as_deref(), |tool, argv| {
        std::process::Command::new(tool)
            .args(argv)
            .status()
            .map(|s| s.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn index_argv_lists_directory() {
        let dir = Path::new("/tmp/my feed; rm -rf");
        let argv = index_argv(dir, 0);
        assert_eq!(argv.last().unwrap(), dir.as_os_str());
        assert_eq!(argv.iter().filter(|a| *a == dir.as_os_str()).count(), 1);
    }

    #[test]
    fn dash_leading_directory_gets_dot_slash_prefix() {
        // createrepo_c reads `-rpms` as an option and `--` does not help.
        let argv = index_argv(Path::new("-rpms"), 0);
        assert_eq!(argv.last().unwrap(), OsStr::new("./-rpms"));
        let argv = index_argv(Path::new("-rpms/sub"), 0);
        assert_eq!(argv.last().unwrap(), OsStr::new("./-rpms/sub"));
        // Absolute and ordinary relative paths are passed through unchanged.
        let argv = index_argv(Path::new("/tmp/-rpms"), 0);
        assert_eq!(argv.last().unwrap(), OsStr::new("/tmp/-rpms"));
        let argv = index_argv(Path::new("rpms"), 0);
        assert_eq!(argv.last().unwrap(), OsStr::new("rpms"));
    }

    #[test]
    fn argv_pins_timestamps_and_keeps_warnings() {
        let argv = index_argv(Path::new("rpms"), 1234);
        let has = |s: &str| argv.iter().any(|a| a == OsStr::new(s));
        assert!(has("--revision=1234"), "{argv:?}");
        assert!(has("--set-timestamp-to-revision"), "{argv:?}");
        assert!(has("--error-exit-val"), "{argv:?}");
        assert!(!has("--quiet") && !has("-q"), "{argv:?}");
    }

    #[test]
    fn revision_is_newest_rpm_mtime_and_ignores_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(newest_rpm_mtime(tmp.path()), 0);
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let old = sub.join("a.rpm");
        let new = tmp.path().join("b.rpm");
        let other = tmp.path().join("notes.txt");
        for p in [&old, &new, &other] {
            std::fs::write(p, "x").unwrap();
        }
        let set = |p: &Path, secs: u64| {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set(&old, 1_000);
        set(&new, 2_000);
        set(&other, 9_000);
        assert_eq!(newest_rpm_mtime(tmp.path()), 2_000);
        set(&old, 3_000);
        assert_eq!(newest_rpm_mtime(tmp.path()), 3_000);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_does_not_hang_the_revision_walk() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(".", tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(".", tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("x.rpm"), "x").unwrap();
        assert!(newest_rpm_mtime(tmp.path()) > 0);
    }

    #[cfg(unix)]
    #[test]
    fn run_passes_the_newest_rpm_mtime_as_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        write_tool(bin.path(), 0o755);
        let rpm = tmp.path().join("a.rpm");
        std::fs::write(&rpm, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&rpm)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(5_000))
            .unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        run_with(tmp.path(), Some(bin.path().as_os_str()), |_, argv| {
            seen.borrow_mut().extend_from_slice(argv);
            Ok(true)
        })
        .unwrap();
        assert!(
            seen.borrow()
                .iter()
                .any(|a| a == OsStr::new("--revision=5000")),
            "{:?}",
            seen.borrow()
        );
    }

    #[cfg(unix)]
    fn write_tool(dir: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(TOOL);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_tool_is_skipped_for_a_later_executable_one() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_tool(first.path(), 0o644);
        write_tool(second.path(), 0o755);
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(
            which_on_path(TOOL, Some(&path)),
            Some(second.path().join(TOOL))
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn empty_path_element_is_skipped() {
        // An empty element would resolve to the bare name, i.e. the cwd, so
        // the cwd holds an executable tool that only the skip keeps out.
        let cwd = tempfile::tempdir().unwrap();
        write_tool(cwd.path(), 0o755);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let found = which_on_path(TOOL, Some(&OsString::from(":")));
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(found, None);
    }

    #[test]
    fn missing_tool_names_createrepo_c() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let spawned = Cell::new(false);
        let err = run_with(tmp.path(), Some(empty.path().as_os_str()), |_, _| {
            spawned.set(true);
            Ok(true)
        })
        .unwrap_err();
        assert!(err.to_string().contains("createrepo_c"), "{err}");
        assert!(!spawned.get());
        assert!(!tmp.path().join("repodata").exists());
    }

    #[cfg(unix)]
    #[test]
    fn missing_directory_runs_no_subprocess() {
        let tmp = tempfile::tempdir().unwrap();
        // A PATH that does hold the tool: only the directory check can fail.
        write_tool(tmp.path(), 0o755);
        let missing = tmp.path().join("no-such-dir");
        let spawned = Cell::new(false);
        let err = run_with(&missing, Some(tmp.path().as_os_str()), |_, _| {
            spawned.set(true);
            Ok(true)
        })
        .unwrap_err();
        assert!(err.to_string().contains("no-such-dir"), "{err}");
        assert!(!spawned.get());
    }

    /// Runs `run_with` over `dir` with an executable tool on PATH and reports
    /// the result and whether the tool was spawned.
    #[cfg(unix)]
    fn run_with_tool(dir: &Path) -> (Result<()>, bool) {
        let bin = tempfile::tempdir().unwrap();
        write_tool(bin.path(), 0o755);
        let spawned = Cell::new(false);
        let res = run_with(dir, Some(bin.path().as_os_str()), |_, _| {
            spawned.set(true);
            Ok(true)
        });
        (res, spawned.get())
    }

    #[cfg(unix)]
    #[test]
    fn link_to_the_feed_directory_fails_naming_it_without_spawning() {
        // createrepo_c follows symlinked directories, so `a -> .` never ends.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let link = tmp.path().join("sub").join("a");
        std::os::unix::fs::symlink("..", &link).unwrap();
        let (res, spawned) = run_with_tool(tmp.path());
        let err = res.expect_err("a cycle must be refused").to_string();
        assert!(err.contains(&link.display().to_string()), "{err}");
        assert!(!spawned, "the tool was spawned over a cycle");
    }

    #[cfg(unix)]
    #[test]
    fn link_to_an_ancestor_fails_naming_it_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let feed = tmp.path().join("feed");
        std::fs::create_dir(&feed).unwrap();
        let link = feed.join("up");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
        let (res, spawned) = run_with_tool(&feed);
        let err = res.expect_err("a cycle must be refused").to_string();
        assert!(err.contains(&link.display().to_string()), "{err}");
        assert!(!spawned, "the tool was spawned over a cycle");
    }

    #[cfg(unix)]
    #[test]
    fn link_to_a_directory_outside_the_feed_still_indexes() {
        let feed = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("x.rpm"), "x").unwrap();
        std::os::unix::fs::symlink(other.path(), feed.path().join("ext")).unwrap();
        // A second link to the same directory is a repeat, not a cycle.
        std::os::unix::fs::symlink(other.path(), feed.path().join("ext2")).unwrap();
        let (res, spawned) = run_with_tool(feed.path());
        res.expect("a link that leads out of the feed is not a cycle");
        assert!(spawned, "the tool was not spawned");
    }
}
