//! `avocado --version` reports the commit the binary was built from, in the
//! same shape rustc uses: `avocado <version> (<short-sha> <commit-date>)`.
//!
//! The version is the SECOND whitespace-separated field. Anything parsing this
//! line must read that field: the trailing `(<sha> <date>)` means the last
//! field is a date, not a version. `utils::remote::check_cli_version` parses it
//! that way, and avocado-desktop's `parse_cli_version` has to match.

use std::process::Command;

use semver::Version;

/// Read the `--version` line the way its consumers do.
fn version_line() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .arg("--version")
        .output()
        .expect("failed to run `avocado --version`");

    assert!(
        output.status.success(),
        "`avocado --version` exited with {:?}",
        output.status.code()
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Run a git command in the source tree, returning trimmed stdout on success.
/// `None` means this is not a git checkout (source tarball, vendored build).
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

#[test]
fn version_field_is_the_bare_crate_version() {
    let line = version_line();
    assert!(
        line.starts_with("avocado "),
        "version line should start with the binary name: {line:?}"
    );

    // Field 1 is the version, and it stays a bare semver so version
    // comparisons keep working: the commit rides in the parenthetical.
    let field = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no version field in {line:?}"));
    let parsed =
        Version::parse(field).unwrap_or_else(|e| panic!("{field:?} must parse as semver: {e}"));

    assert_eq!(
        parsed,
        Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver"),
        "reported version {field:?} must equal the crate version"
    );

    // Any build detail is a PARENTHETICAL. Without this the test also passes for
    // the older `avocado <version> <sha>` shape, so nothing here would catch a
    // revert to it - and under that shape the last field is a bare sha, which is
    // exactly what a `.last()` parser would report as the version.
    let detail = line[format!("avocado {field}").len()..].trim();
    assert!(
        detail.is_empty() || (detail.starts_with('(') && detail.ends_with(')')),
        "build detail must be parenthesized, got {detail:?} in {line:?}"
    );
}

#[test]
fn version_reports_the_commit_and_date_it_was_built_from() {
    let line = version_line();
    let bare = format!("avocado {}", env!("CARGO_PKG_VERSION"));

    let (Some(sha), Some(date)) = (
        git(&["rev-parse", "--short", "HEAD"]),
        git(&["show", "-s", "--format=%cs", "HEAD"]),
    ) else {
        // Not a git checkout, or git is absent. Assert the other branch rather
        // than returning: a bare `return` here made the whole format check a
        // no-op that reported success on a runner without git, which is the one
        // environment where a silent pass is most likely to go unnoticed.
        assert_eq!(
            line, bare,
            "with no git available the version line must be bare"
        );
        return;
    };

    assert_eq!(
        line,
        format!("{bare} ({sha} {date})"),
        "version line should carry the build commit and its date"
    );
}
