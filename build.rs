use std::env;
use std::path::Path;
use std::process::Command;

/// Emit the version string reported by `avocado --version`, plus the target
/// triple `avocado upgrade` uses to pick its release asset.
///
/// The build detail follows rustc's shape, `1.2.3 (abc1234 2026-03-05)`, so the
/// version stays a bare semver in its own field and the commit and its date sit
/// in a trailing parenthetical. Anything parsing this line has to read the
/// SECOND whitespace-separated field: the last one is now a date.
/// `utils::remote::check_cli_version` does, and avocado-desktop's
/// `parse_cli_version` has to match.
///
/// The date is the commit date, not the build date, so the string stays
/// reproducible across rebuilds of the same commit.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_rerun_hints();

    let version = match (
        git(&["rev-parse", "--short", "HEAD"]),
        git(&["show", "-s", "--format=%cs", "HEAD"]),
    ) {
        (Some(sha), Some(date)) if in_own_repository() => {
            format!("{} ({sha} {date})", env!("CARGO_PKG_VERSION"))
        }
        // Building outside a git checkout (source tarball, vendored crate) is
        // legitimate, so report the bare crate version rather than failing the
        // build the way an unwrap here would. `in_own_repository` routes the
        // more dangerous case here too: git succeeded, but against somebody
        // else's repository.
        _ => env!("CARGO_PKG_VERSION").to_string(),
    };
    println!("cargo:rustc-env=AVOCADO_CLI_VERSION={version}");

    println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
}

/// Whether the repository git just answered from is this crate's own.
///
/// `git` runs with the build script's cwd and will happily answer from whatever
/// repository encloses it. A crate unpacked inside an unrelated one - a vendored
/// copy in a monorepo, a source tree under a Yocto `WORKDIR` that is itself
/// version-controlled, a docker build context inside a repo - would otherwise
/// embed that repository's sha, and the string looks authoritative enough that a
/// bug report citing an unrelated commit goes unquestioned. Reporting the bare
/// version, as if git were absent, is the honest answer there.
///
/// This crate lives at its repository root, so an exact match is the check;
/// anything else means git found a different tree.
fn in_own_repository() -> bool {
    let (Some(toplevel), Ok(manifest_dir)) = (
        git(&["rev-parse", "--show-toplevel"]),
        env::var("CARGO_MANIFEST_DIR"),
    ) else {
        return false;
    };
    // Compare canonicalized: either side can arrive with a symlinked path.
    match (
        std::fs::canonicalize(&toplevel),
        std::fs::canonicalize(&manifest_dir),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => toplevel == manifest_dir,
    }
}

/// Rebuild when HEAD moves. Without these, cargo caches the build-script
/// output and the reported commit goes stale after the next commit or checkout.
fn emit_git_rerun_hints() {
    let mut watched = Vec::new();
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        watched.push(head);
    }
    // The reflog is the only one of these that changes on EVERY commit and is
    // never packed away. `.git/HEAD` is a symref whose contents do not change
    // when you commit, and the loose branch ref that does is deleted by
    // `git gc` / `pack-refs` / `maintenance` - after which nothing watched here
    // would move again and the embedded sha would stay stale indefinitely,
    // without self-healing. `packed-refs` is no substitute: its mtime does not
    // change on commit either.
    if let Some(reflog) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
        watched.push(reflog);
    }
    // Follow the branch ref too, for the case the reflog is disabled
    // (`core.logAllRefUpdates=false`, the default in a bare repository).
    // `--symbolic-full-name` prints `HEAD` on a detached checkout, which
    // resolves right back to `.git/HEAD`; a detached checkout needs no extra
    // watch anyway, since `.git/HEAD` then holds the sha directly and changes
    // with it. Dedup below drops the repeat.
    if let Some(branch) = git(&["rev-parse", "--symbolic-full-name", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &branch]) {
            watched.push(path);
        }
    }

    watched.sort();
    watched.dedup();
    for path in watched {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Run a git command, returning its trimmed stdout when it succeeds with
/// output. `None` covers git being absent as well as a failed invocation.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}
