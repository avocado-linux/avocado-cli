//! The extra mount a project needs when it is a linked git worktree.
//!
//! In a linked worktree (`git worktree add`), `<project>/.git` is a file
//! with one line, `gitdir: <path>`. The path points into the main checkout,
//! at `<common>/worktrees/<id>`, and that directory has a `commondir` file
//! that points back at `<common>`. The SDK container mounts only the project
//! directory, so git in the container cannot open either path and every
//! `git` command fails with "not a git repository".
//!
//! [`git_common_dir_mount`] reads the same files git reads, without running
//! git on the host, and returns the host common dir and the container path
//! that git in the container will look for. For an absolute `gitdir` (the
//! default) the container path is the host path. For a relative `gitdir`
//! (`worktree.useRelativePaths`) it is resolved against the container's
//! source mount, as git in the container will resolve it.

use std::path::{Component, Path, PathBuf};

/// A host git common dir and where the container must see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommonMount {
    /// The common dir on the host (canonical).
    pub host: PathBuf,
    /// The absolute path git in the container resolves the common dir to.
    pub container: PathBuf,
}

/// The common dir mount `project_dir` needs, when its `.git` is a file that
/// points outside the project.
///
/// `container_src` is where the container mounts `project_dir` (`/opt/src`).
///
/// - `Ok(None)`: nothing to mount. There is no `.git`, `.git` is a
///   directory, or the git dir is already inside the project.
/// - `Ok(Some(_))`: mount `host` read-write at `container`.
/// - `Err(reason)`: `.git` is a file, but a mount would not give a working
///   repository in the container. The caller warns and mounts nothing.
pub fn git_common_dir_mount(
    project_dir: &Path,
    container_src: &Path,
) -> Result<Option<GitCommonMount>, String> {
    let dot_git = project_dir.join(".git");
    if !dot_git.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&dot_git)
        .map_err(|e| format!("could not read {}: {e}", dot_git.display()))?;
    let gitdir = raw
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{} has no `gitdir:` line", dot_git.display()))?;
    let gitdir = Path::new(gitdir);

    let host_gitdir = normalize(&project_dir.join(gitdir));
    let container_gitdir = normalize(&container_src.join(gitdir));
    if !host_gitdir.is_dir() {
        return Err(format!(
            "{} points at {}, which does not exist",
            dot_git.display(),
            host_gitdir.display()
        ));
    }

    // A submodule's git dir sets core.worktree to a path relative to itself.
    // Inside the container that path does not lead back to the source mount,
    // so git would still fail after a mount.
    if has_core_worktree(&host_gitdir.join("config")) {
        return Err(format!(
            "{} is a submodule git dir (it sets core.worktree)",
            host_gitdir.display()
        ));
    }

    let (host_common, container_common) =
        match std::fs::read_to_string(host_gitdir.join("commondir")) {
            Ok(c) => {
                let c = Path::new(c.trim());
                (
                    normalize(&host_gitdir.join(c)),
                    normalize(&container_gitdir.join(c)),
                )
            }
            Err(_) => (host_gitdir.clone(), container_gitdir.clone()),
        };

    if !container_gitdir.starts_with(&container_common) {
        return Err(format!(
            "git dir {} is not inside its common dir {}",
            host_gitdir.display(),
            host_common.display()
        ));
    }
    if container_common.starts_with(container_src) {
        return Ok(None);
    }
    if !container_common.has_root() {
        return Err(format!(
            "{} cannot be a container path",
            container_common.display()
        ));
    }

    let host = host_common
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", host_common.display()))?;
    Ok(Some(GitCommonMount {
        host,
        container: container_common,
    }))
}

/// Remove `.` and `..` without touching the file system, as git does when it
/// joins a relative `gitdir` or `commondir`. `..` at the root stays at the
/// root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    continue;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether a git config file sets `worktree` in its `[core]` section.
fn has_core_worktree(config: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(config) else {
        return false;
    };
    let mut in_core = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_core = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .eq_ignore_ascii_case("core");
            continue;
        }
        if in_core {
            let key = line.split('=').next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("worktree") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const SRC: &str = "/opt/src";

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit at `<root>/main`.
    fn repo(root: &Path) -> PathBuf {
        let main = root.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("avocado.yaml"), "x: 1\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "init"]);
        main
    }

    #[test]
    fn a_linked_worktree_mounts_the_common_dir_at_its_host_path() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

        let m = git_common_dir_mount(&wt, Path::new(SRC)).unwrap().unwrap();
        let common = main.join(".git").canonicalize().unwrap();
        assert_eq!(m.host, common);
        // git writes the absolute path it saw, so compare against the file.
        let line = std::fs::read_to_string(wt.join(".git")).unwrap();
        let gitdir = Path::new(line.trim().strip_prefix("gitdir: ").unwrap());
        assert_eq!(m.container, normalize(&gitdir.join("../..")));
        assert!(m.container.has_root());
    }

    #[test]
    fn a_relative_gitdir_resolves_against_the_container_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        // What `worktree.useRelativePaths` writes, without needing git 2.48.
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();

        let m = git_common_dir_mount(&wt, Path::new(SRC)).unwrap().unwrap();
        assert_eq!(m.host, main.join(".git").canonicalize().unwrap());
        assert_eq!(m.container, PathBuf::from("/opt/main/.git"));
    }

    #[test]
    fn a_normal_repo_needs_no_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        assert_eq!(git_common_dir_mount(&main, Path::new(SRC)), Ok(None));
    }

    #[test]
    fn a_dir_without_git_needs_no_mount() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(git_common_dir_mount(tmp.path(), Path::new(SRC)), Ok(None));
    }

    #[test]
    fn a_git_dir_inside_the_project_needs_no_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        std::fs::rename(main.join(".git"), main.join(".git-real")).unwrap();
        std::fs::write(main.join(".git"), "gitdir: .git-real\n").unwrap();
        assert_eq!(git_common_dir_mount(&main, Path::new(SRC)), Ok(None));
    }

    #[test]
    fn a_submodule_is_reported_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = repo(tmp.path());
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        git(&parent, &["init", "-q"]);
        git(
            &parent,
            &["submodule", "add", "-q", lib.to_str().unwrap(), "proj"],
        );

        let err = git_common_dir_mount(&parent.join("proj"), Path::new(SRC)).unwrap_err();
        assert!(err.contains("submodule"), "{err}");
    }

    #[test]
    fn a_dangling_gitdir_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: /nonexistent/.git/x\n").unwrap();
        let err = git_common_dir_mount(tmp.path(), Path::new(SRC)).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn normalize_handles_dot_dot_and_the_root() {
        assert_eq!(
            normalize(Path::new("/opt/src/../main/./.git/worktrees/wt/../..")),
            PathBuf::from("/opt/main/.git")
        );
        assert_eq!(normalize(Path::new("/opt/../../x")), PathBuf::from("/x"));
    }
}
