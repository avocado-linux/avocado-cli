//! Integration tests for source-tree extension versions.
//!
//! The regression these guard: an extension whose `version` came from
//! `{{ env.AVOCADO_EXT_VERSION }}` resolved at package time but interpolated to
//! `""` for anyone consuming it via `source: { type: path | git }`, so switching
//! an extension to a local checkout for testing failed semver validation. A
//! `version: { file, key }` provider reads from the extension's own tree, which
//! is present in every consumption mode, so the composed version is identical
//! no matter which source kind is used.

use avocado_cli::utils::config::Config;
use std::fs;
use std::path::{Path, PathBuf};

/// A scratch directory that cleans itself up.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("avocado_ext_version_source_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Write a minimal extension whose version lives in `Cargo.toml`, exactly like
/// the in-source program extensions (avocado-cli, avocado-conn, avocado-rat).
fn write_cargo_extension(dir: &TestDir, rel: &str, ext_name: &str, version: &str) {
    dir.write(
        &format!("{rel}/Cargo.toml"),
        &format!("[package]\nname = \"prog\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    );
    dir.write(
        &format!("{rel}/avocado.yaml"),
        &format!(
            "supported_targets: '*'\n\
             extensions:\n  \
               {ext_name}:\n    \
                 version:\n      \
                   file: Cargo.toml\n      \
                   key: package.version\n    \
                 types:\n      \
                   - sysext\n"
        ),
    );
}

fn composed_ext_version(config_path: &Path, ext_name: &str) -> Option<String> {
    let composed = Config::load_composed(config_path.to_str().unwrap(), Some("qemux86-64"))
        .expect("config should compose");
    composed
        .merged_value
        .get("extensions")?
        .get(ext_name)?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// The reported bug: an extension consumed via `source: { type: path }` must
/// compose to a real version rather than failing validation.
#[test]
fn path_sourced_extension_resolves_version_from_its_own_tree() {
    let dir = TestDir::new("path_source");
    write_cargo_extension(&dir, "my-ext", "avocado-ext-prog", "1.0.0-rc.1");

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("1.0.0-rc.1")
    );
}

/// The version tracks the program it wraps with no edit to avocado.yaml — the
/// whole point of the provider.
#[test]
fn path_sourced_version_tracks_the_program_version() {
    let dir = TestDir::new("tracks");
    write_cargo_extension(&dir, "my-ext", "avocado-ext-prog", "1.0.0-rc.1");

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("1.0.0-rc.1")
    );

    // Bump only Cargo.toml.
    dir.write(
        "my-ext/Cargo.toml",
        "[package]\nname = \"prog\"\nversion = \"2.4.0\"\nedition = \"2021\"\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("2.4.0")
    );
}

/// A bare `VERSION` file, with no `key`, is read whole and trimmed.
#[test]
fn path_sourced_extension_resolves_a_bare_version_file() {
    let dir = TestDir::new("bare_file");
    dir.write("my-ext/VERSION", "  0.4.2\n\n");
    dir.write(
        "my-ext/avocado.yaml",
        "supported_targets: '*'\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             version:\n      \
               file: VERSION\n",
    );

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("0.4.2")
    );
}

/// A `package`-sourced extension is read out of the installed payload. The RPM
/// ships the provider rather than a baked literal, so the payload must resolve
/// itself — and must land on the same version a `path` source would.
///
/// The dev-fallback root (`<src_dir>/.avocado/<target>/includes/<ext>`) stands
/// in for the installed payload; it is the same directory layout DNF produces.
#[test]
fn package_sourced_payload_resolves_to_the_same_version_as_a_path_source() {
    let dir = TestDir::new("package_parity");
    write_cargo_extension(&dir, "my-ext", "avocado-ext-prog", "3.1.4");

    // What `avocado ext package` publishes and DNF installs: the extension's
    // own tree, provider intact, alongside the file the provider names.
    write_cargo_extension(
        &dir,
        "project/.avocado/qemux86-64/includes/avocado-ext-prog",
        "avocado-ext-prog",
        "3.1.4",
    );

    let via_path = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );
    let path_version = composed_ext_version(&via_path, "avocado-ext-prog");

    let via_package = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: package\n      \
               version: '*'\n",
    );
    let package_version = composed_ext_version(&via_package, "avocado-ext-prog");

    assert_eq!(path_version.as_deref(), Some("3.1.4"));
    assert_eq!(
        path_version, package_version,
        "switching source kind changed the composed version"
    );

    // Prove the package leg really read the installed payload rather than
    // happening to agree with the sibling checkout: bump only the payload.
    dir.write(
        "project/.avocado/qemux86-64/includes/avocado-ext-prog/Cargo.toml",
        "[package]\nname = \"prog\"\nversion = \"3.2.0\"\nedition = \"2021\"\n",
    );
    assert_eq!(
        composed_ext_version(&via_package, "avocado-ext-prog").as_deref(),
        Some("3.2.0"),
        "package source did not resolve from the installed payload"
    );
}

/// An extension defined in the project's own config resolves against that
/// config's directory. This is the path `avocado ext package` takes for the
/// in-source program extensions.
#[test]
fn local_extension_resolves_against_its_own_config_directory() {
    let dir = TestDir::new("local");
    dir.write(
        "Cargo.toml",
        "[package]\nname = \"prog\"\nversion = \"1.0.0-rc.1\"\nedition = \"2021\"\n",
    );
    let project = dir.write(
        "avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             version:\n      \
               file: Cargo.toml\n      \
               key: package.version\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("1.0.0-rc.1")
    );

    // `ext package` reads a local extension through `get_merged_ext_config`,
    // which re-reads the file rather than going through `load_composed`. It has
    // to resolve the provider too, or in-source extensions package unresolved.
    let config = Config::load(&project).unwrap();
    let merged = config
        .get_merged_ext_config("avocado-ext-prog", "qemux86-64", project.to_str().unwrap())
        .unwrap()
        .expect("extension section");
    assert_eq!(
        merged.get("version").and_then(|v| v.as_str()),
        Some("1.0.0-rc.1")
    );
}

/// A provider that can't be resolved must say which file it wanted and where it
/// looked, rather than surfacing as an empty or bogus version downstream.
#[test]
fn missing_version_file_reports_the_file_and_the_extension() {
    let dir = TestDir::new("missing_file");
    dir.write(
        "my-ext/avocado.yaml",
        "supported_targets: '*'\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             version:\n      \
               file: Cargo.toml\n      \
               key: package.version\n",
    );

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    let err = Config::load_composed(project.to_str().unwrap(), Some("qemux86-64"))
        .expect_err("a missing version file must fail composition");
    let msg = format!("{err:#}");
    assert!(msg.contains("avocado-ext-prog"), "{msg}");
    assert!(msg.contains("Cargo.toml"), "{msg}");
}

/// The consumer keeps the last word: a literal `version` in the project config
/// wins over the extension's provider, same as any other field.
#[test]
fn consumer_can_still_override_the_version() {
    let dir = TestDir::new("override");
    write_cargo_extension(&dir, "my-ext", "avocado-ext-prog", "1.0.0-rc.1");

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             version: '9.9.9'\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("9.9.9")
    );
}

/// Plain string versions are untouched — the mapping form is purely additive.
#[test]
fn literal_versions_are_unaffected() {
    let dir = TestDir::new("literal");
    dir.write(
        "my-ext/avocado.yaml",
        "supported_targets: '*'\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             version: '0.1.0'\n",
    );

    let project = dir.write(
        "project/avocado.yaml",
        "default_target: qemux86-64\n\
         extensions:\n  \
           avocado-ext-prog:\n    \
             source:\n      \
               type: path\n      \
               path: ../my-ext\n",
    );

    assert_eq!(
        composed_ext_version(&project, "avocado-ext-prog").as_deref(),
        Some("0.1.0")
    );
}
