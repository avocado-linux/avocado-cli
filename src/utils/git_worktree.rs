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
    /// The repository sets `extensions.relativeWorktrees` (written by
    /// `worktree.useRelativePaths`). git before 2.48 refuses such a
    /// repository; the 2024 SDK ships 2.44, the 2026 SDK 2.53.
    pub relative_worktrees: bool,
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
///   repository in the container, or the dir it names is not this
///   worktree's git dir. The caller warns and mounts nothing.
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
    // git reads only the first line.
    let gitdir = raw
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{} has no `gitdir:` line", dot_git.display()))?;
    let gitdir = Path::new(gitdir);

    // On the host, resolve through the file system as git does, so that `..`
    // after a symlinked project dir leaves the symlink's target. In the
    // container nothing on the path is a symlink, so `..` is lexical.
    let host_gitdir = project_dir.join(gitdir).canonicalize().map_err(|_| {
        format!(
            "{} points at {}, which does not exist",
            dot_git.display(),
            normalize(&project_dir.join(gitdir)).display()
        )
    })?;
    let container_gitdir = normalize(&container_src.join(gitdir));

    // A submodule's git dir sets core.worktree to a path relative to itself.
    // Inside the container that path does not lead back to the source mount,
    // so git would still fail after a mount.
    if has_core_worktree(&host_gitdir.join("config")) {
        return Err(format!(
            "{} is a submodule git dir (it sets core.worktree)",
            host_gitdir.display()
        ));
    }

    let commondir = std::fs::read_to_string(host_gitdir.join("commondir")).ok();
    let (host_common, container_common) = match &commondir {
        Some(c) => {
            let c = Path::new(c.trim());
            let host = host_gitdir.join(c);
            (
                host.canonicalize()
                    .map_err(|e| format!("could not resolve {}: {e}", host.display()))?,
                normalize(&container_gitdir.join(c)),
            )
        }
        None => (host_gitdir.clone(), container_gitdir.clone()),
    };

    // The container sees the git dir through the common dir mount, so it must
    // sit at the same place under the common dir on both sides. A symlink in
    // the gitdir path can make the host and container paths differ.
    let host_rel = host_gitdir.strip_prefix(&host_common).ok();
    let container_rel = container_gitdir.strip_prefix(&container_common).ok();
    if host_rel.is_none() || host_rel != container_rel {
        return Err(format!(
            "git dir {} is not inside its common dir {} at the same place on the \
             host and in the container",
            host_gitdir.display(),
            host_common.display()
        ));
    }
    // Already in the container when the common dir is inside the project and
    // git looks for it at the same place under the source mount.
    let project = project_dir
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", project_dir.display()))?;
    if let Ok(rel) = host_common.strip_prefix(&project) {
        if container_common == container_src.join(rel) {
            return Ok(None);
        }
    }
    // From here on the common dir is mounted read-write into a root
    // container, so it must be this worktree's repository. A `.git` file from
    // a tarball or a copied tree can name any dir, `/` or `$HOME` included.
    // git writes a linked worktree's git dir with a `commondir` file and a
    // `gitdir` file that points back at `<worktree>/.git`.
    if commondir.is_none() {
        return Err(format!(
            "{} is not a linked worktree git dir (it has no commondir file)",
            host_gitdir.display()
        ));
    }
    if !points_back_at(&host_gitdir, &dot_git) {
        return Err(format!(
            "{} belongs to another worktree or to none: its gitdir file does not point at {}",
            host_gitdir.display(),
            dot_git.display()
        ));
    }
    if !looks_like_git_dir(&host_common) {
        return Err(format!(
            "{} is not a git dir (it needs HEAD, objects and refs)",
            host_common.display()
        ));
    }
    if container_common.starts_with(container_src) {
        return Err(format!(
            "git looks for the common dir {} at {}, which is inside the project mount",
            host_common.display(),
            container_common.display()
        ));
    }
    if !container_common.has_root() {
        return Err(format!(
            "{} cannot be a container path",
            container_common.display()
        ));
    }
    // Submodule links in a linked worktree are relative paths between the
    // worktree and the common dir. In the container the worktree is at the
    // source mount and the common dir is somewhere else, so git cannot follow
    // links made on the host, and links that `git submodule update --init`
    // makes in the container are wrong on the host.
    if project_dir.join(".gitmodules").is_file() {
        return Err(
            "the worktree has submodules, and their links do not resolve in the \
             container. Build from the main checkout to use git there"
                .to_string(),
        );
    }

    let relative_worktrees = config_value(
        &host_common.join("config"),
        "extensions",
        "relativeworktrees",
    )
    .is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1" | ""
        )
    });
    Ok(Some(GitCommonMount {
        host: host_common,
        container: container_common,
        relative_worktrees,
    }))
}

/// Whether the `gitdir` file in a linked worktree's git dir names `dot_git`.
/// git writes it as an absolute path, or relative to the git dir with
/// `worktree.useRelativePaths`.
fn points_back_at(gitdir: &Path, dot_git: &Path) -> bool {
    let Ok(back) = std::fs::read_to_string(gitdir.join("gitdir")) else {
        return false;
    };
    let back = gitdir.join(back.trim());
    match (back.canonicalize(), dot_git.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The files and dirs every git dir has.
fn looks_like_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
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
    config_value(config, "core", "worktree").is_some()
}

/// The value of `key` in `[section]` of a git config file, if it is set.
/// Section and key names are case-insensitive, as in git. A key with no
/// `=` (a bare boolean) gives an empty value.
fn config_value(config: &Path, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .eq_ignore_ascii_case(section);
            continue;
        }
        if in_section {
            let (k, v) = line.split_once('=').unwrap_or((line, ""));
            if k.trim().eq_ignore_ascii_case(key) {
                // Drop a trailing comment and surrounding quotes.
                let v = v.split(['#', ';']).next().unwrap_or("").trim();
                return Some(v.trim_matches('"').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const SRC: &str = "/opt/src";

    /// Run git in `dir` with none of the user's or the system's config, so
    /// a global `worktree.useRelativePaths` or `commit.gpgsign` cannot change
    /// what the fixtures write.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "worktree.useRelativePaths=false"])
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args([
                "-c",
                "protocol.file.allow=always",
                "-c",
                "commit.gpgsign=false",
            ])
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

    /// git resolves a relative gitdir through the file system, so `..`
    /// after a symlinked project dir leaves the symlink's target.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_project_dir_resolves_a_relative_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        std::fs::create_dir(&repos).unwrap();
        let main = repo(&repos);
        let wt = repos.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&wt, &alias).unwrap();

        let m = git_common_dir_mount(&alias, Path::new(SRC))
            .unwrap()
            .unwrap();
        assert_eq!(m.host, main.join(".git").canonicalize().unwrap());
        assert_eq!(m.container, PathBuf::from("/opt/main/.git"));
    }

    /// A common dir whose container path falls inside the source mount, but
    /// is not the same place on the host, cannot be mounted there: the
    /// container's mount holds the worktree, not the host dir of that name.
    #[test]
    fn a_common_dir_that_only_looks_inside_the_mount_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = repo(&root);
        let wt = root.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

        // As if the container mounted the worktree at the host dir that holds
        // the main checkout.
        let err = git_common_dir_mount(&wt, &root).unwrap_err();
        assert!(err.contains("inside the project mount"), "{err}");
    }

    /// git in the container cannot use the submodules of a linked worktree,
    /// and `git submodule update --init` there writes container paths into
    /// links that stay on the host. No mount keeps the host checkout safe.
    #[test]
    fn a_worktree_with_submodules_is_reported_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        std::fs::write(
            main.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.invalid/lib.git\n",
        )
        .unwrap();
        git(&main, &["add", ".gitmodules"]);
        git(&main, &["commit", "-qm", "submodule"]);
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

        let err = git_common_dir_mount(&wt, Path::new(SRC)).unwrap_err();
        assert!(err.contains("submodules"), "{err}");
        // The main checkout has a `.git` dir and needs nothing.
        assert_eq!(git_common_dir_mount(&main, Path::new(SRC)), Ok(None));
    }

    /// A `.git` file from a tarball or a copied tree can name any dir. Only a
    /// linked worktree's own git dir is mounted, never `/`.
    #[test]
    fn a_gitdir_of_root_is_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: /\n").unwrap();
        let err = git_common_dir_mount(tmp.path(), Path::new(SRC)).unwrap_err();
        assert!(err.contains("not a linked worktree"), "{err}");
    }

    #[test]
    fn a_gitdir_of_a_home_dir_is_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::fs::write(home.join(".profile"), "x\n").unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", home.display()),
        )
        .unwrap();
        let err = git_common_dir_mount(&project, Path::new(SRC)).unwrap_err();
        assert!(err.contains("not a linked worktree"), "{err}");

        // A commondir file alone does not make it a worktree git dir.
        std::fs::write(home.join("commondir"), "..\n").unwrap();
        let err = git_common_dir_mount(&project, Path::new(SRC)).unwrap_err();
        assert!(err.contains("belongs to"), "{err}");
    }

    /// The git dir of another worktree points back at that worktree, not at
    /// this project, so this project gets no mount.
    #[test]
    fn a_gitdir_of_another_worktree_is_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::copy(wt.join(".git"), other.join(".git")).unwrap();

        let err = git_common_dir_mount(&other, Path::new(SRC)).unwrap_err();
        assert!(err.contains("belongs to"), "{err}");
        // The real worktree still mounts.
        assert!(git_common_dir_mount(&wt, Path::new(SRC)).unwrap().is_some());
    }

    /// The common dir a worktree names must be a git dir.
    #[test]
    fn a_common_dir_that_is_not_a_git_dir_is_not_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let admin = main.join(".git/worktrees/wt");
        let fake = tmp.path().join("fake");
        std::fs::create_dir(&fake).unwrap();
        std::fs::write(admin.join("commondir"), format!("{}\n", fake.display())).unwrap();
        // Keep the gitdir inside its common dir so only the git dir check fails.
        let moved = fake.join("worktrees/wt");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&admin, &moved).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", moved.display())).unwrap();

        let err = git_common_dir_mount(&wt, Path::new(SRC)).unwrap_err();
        assert!(err.contains("not a git dir"), "{err}");
    }

    /// `worktree.useRelativePaths` also turns on `extensions.relativeWorktrees`
    /// in the common config, which git before 2.48 refuses. The mount still
    /// happens (newer SDKs read it) and the caller is told.
    #[test]
    fn the_relative_worktrees_extension_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let main = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let m = git_common_dir_mount(&wt, Path::new(SRC)).unwrap().unwrap();
        assert!(!m.relative_worktrees);

        // What git 2.48+ writes, set by hand so older host git can run this.
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        let config = main.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[extensions]\n\trelativeWorktrees = true\n");
        std::fs::write(&config, text).unwrap();

        let m = git_common_dir_mount(&wt, Path::new(SRC)).unwrap().unwrap();
        assert!(m.relative_worktrees);
        assert_eq!(m.container, PathBuf::from("/opt/main/.git"));
    }

    /// A symlink in an absolute gitdir path can put the git dir at a
    /// different place under the common dir on the host and in the
    /// container. Then the mount cannot serve it, so it is reported.
    #[cfg(unix)]
    #[test]
    fn a_gitdir_that_moves_under_a_symlink_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = repo(&root);
        let wt = root.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        std::fs::create_dir(root.join("a")).unwrap();
        std::os::unix::fs::symlink(main.join(".git/worktrees"), root.join("a/b")).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", root.join("a/b/wt").display()),
        )
        .unwrap();

        let err = git_common_dir_mount(&wt, Path::new(SRC)).unwrap_err();
        assert!(err.contains("at the same place"), "{err}");
    }

    /// git reads only the first line of `.git`.
    #[test]
    fn only_the_first_line_of_the_git_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "junk\ngitdir: /\n").unwrap();
        let err = git_common_dir_mount(tmp.path(), Path::new(SRC)).unwrap_err();
        assert!(err.contains("no `gitdir:` line"), "{err}");
    }

    #[test]
    fn config_values_drop_comments_and_quotes() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::write(
            &config,
            "[core]\n\tbare = false\n[Extensions]\n\trelativeWorktrees = \"true\" # set by git\n",
        )
        .unwrap();
        assert_eq!(
            config_value(&config, "extensions", "relativeworktrees").as_deref(),
            Some("true")
        );
        assert_eq!(config_value(&config, "core", "worktree"), None);
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
