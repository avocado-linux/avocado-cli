//! Reading files out of an extension's source tree.
//!
//! An extension's tree is reachable four different ways depending on how it was
//! sourced and where the CLI is running:
//!
//! 0. `source: { type: path }` — the user's working copy, on the host.
//! 1. Running *inside* the SDK container — `/opt/_avocado/<target>/includes/<ext>`.
//! 2. On the host with a Docker volume — the volume's mountpoint, when readable.
//! 3. Same volume, but via a throwaway container `cat` when the mountpoint isn't
//!    directly accessible (permissions), or a `.avocado/` dev-fallback directory.
//!
//! This used to be open-coded in [`crate::utils::config`] purely to fetch
//! `avocado.yaml`. It is factored out here because version resolution
//! (`extensions.<n>.version: { file, key }`) has to read a *second* file — the
//! one holding the version — out of the very same tree, by the very same route.
//! Anything that reads from an extension's tree should go through here.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::utils::config::ExtensionSource;
use crate::utils::ext_version_source::ExtFileReader;
use crate::utils::volume::VolumeState;

/// Config file names an extension may use, in preference order.
pub const EXT_CONFIG_NAMES: [&str; 2] = ["avocado.yaml", "avocado.yml"];

/// Where a directory-backed reader's root came from. Only used to make error
/// messages say something more useful than a bare path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirOrigin {
    /// The main config's own directory (an extension defined in-place).
    LocalConfig,
    /// A `source: { type: path }` working copy.
    SourcePath,
    /// The includes dir, visible because we're running inside the container.
    Container,
    /// The `<src_dir>/.avocado/<target>/includes/<ext>` development fallback.
    DevFallback,
}

impl DirOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::LocalConfig => "config directory",
            Self::SourcePath => "extension source path",
            Self::Container => "container includes directory",
            Self::DevFallback => "local includes directory",
        }
    }
}

/// A handle for reading files from one extension's source root.
#[derive(Debug, Clone)]
pub enum ExtSourceReader {
    /// A directory this process can read directly.
    Dir { root: PathBuf, origin: DirOrigin },
    /// A Docker volume. Prefers the host mountpoint and falls back to a
    /// throwaway container, because the mountpoint is often root-owned.
    Volume {
        state: VolumeState,
        target: String,
        ext_name: String,
    },
}

/// An extension's config, plus the reader that found it.
pub struct DiscoveredExt {
    pub reader: ExtSourceReader,
    /// The config file's real basename — `avocado.yaml` or `avocado.yml`.
    pub config_name: String,
    pub config_content: String,
}

impl ExtSourceReader {
    /// A reader rooted at a plain directory.
    pub fn dir(root: impl Into<PathBuf>, origin: DirOrigin) -> Self {
        Self::Dir {
            root: root.into(),
            origin,
        }
    }

    /// Locate a remote extension's tree and read its config in one pass.
    ///
    /// Returns `None` when no route to the extension can be found — typically
    /// because it hasn't been fetched yet. Callers skip such extensions rather
    /// than failing; the missing definition surfaces later with a better
    /// diagnostic than anything available here.
    pub fn discover(
        ext_name: &str,
        source: &ExtensionSource,
        src_dir: &Path,
        target: &str,
        volume_state: Option<&VolumeState>,
        verbose: bool,
    ) -> Option<DiscoveredExt> {
        for candidate in Self::candidates(ext_name, source, src_dir, target, volume_state) {
            if verbose {
                eprintln!(
                    "[DEBUG] Extension '{ext_name}': trying {}",
                    candidate.describe()
                );
            }
            match candidate.read_ext_config() {
                Ok((config_name, config_content)) => {
                    if verbose {
                        eprintln!(
                            "[DEBUG]   Read {} bytes of {config_name} from {}",
                            config_content.len(),
                            candidate.describe()
                        );
                    }
                    return Some(DiscoveredExt {
                        reader: candidate,
                        config_name,
                        config_content,
                    });
                }
                Err(e) => {
                    if verbose {
                        eprintln!("[DEBUG]   {e:#}");
                    }
                }
            }
        }

        if verbose {
            eprintln!("[DEBUG] No config found for '{ext_name}', skipping");
        }
        None
    }

    /// Candidate roots, most authoritative first.
    ///
    /// A `type: path` extension is pinned to its declared directory — it never
    /// falls back to the container, because a stale copy in the volume would
    /// silently shadow the working copy the user is trying to test.
    fn candidates(
        ext_name: &str,
        source: &ExtensionSource,
        src_dir: &Path,
        target: &str,
        volume_state: Option<&VolumeState>,
    ) -> Vec<Self> {
        if let ExtensionSource::Path { path, .. } = source {
            let resolved = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                src_dir.join(path)
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            return vec![Self::dir(resolved, DirOrigin::SourcePath)];
        }

        let mut candidates = vec![Self::dir(
            format!("/opt/_avocado/{target}/includes/{ext_name}"),
            DirOrigin::Container,
        )];

        match volume_state {
            Some(state) => candidates.push(Self::Volume {
                state: state.clone(),
                target: target.to_string(),
                ext_name: ext_name.to_string(),
            }),
            None => candidates.push(Self::dir(
                src_dir
                    .join(".avocado")
                    .join(target)
                    .join("includes")
                    .join(ext_name),
                DirOrigin::DevFallback,
            )),
        }

        candidates
    }

    /// Read the extension's `avocado.yaml` (or `avocado.yml`).
    ///
    /// Both names are accepted from every source kind. Packaging stages the
    /// config under its real on-disk name, so a `.yml` extension shipped in an
    /// RPM would otherwise be invisible once installed.
    pub fn read_ext_config(&self) -> Result<(String, String)> {
        let mut last_err = None;
        for name in EXT_CONFIG_NAMES {
            match self.read(name) {
                Ok(content) if !content.trim().is_empty() => {
                    return Ok((name.to_string(), content))
                }
                Ok(_) => last_err = Some(anyhow::anyhow!("{name} is empty")),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no extension config found")))
            .with_context(|| format!("no readable avocado.yaml in {}", self.describe()))
    }

    /// Read a file relative to the extension root.
    pub fn read(&self, rel: &str) -> Result<String> {
        match self {
            Self::Dir { root, .. } => {
                let path = root.join(rel);

                // `version.file` already rejects absolute paths and `..`, but a
                // symlink *inside* the tree can still point out of it, and for a
                // `type: git` extension the tree is third-party content. Resolve
                // before reading and require the result to stay under the root.
                let real = path
                    .canonicalize()
                    .with_context(|| format!("Failed to read {}", path.display()))?;
                let real_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                if !real.starts_with(&real_root) {
                    anyhow::bail!(
                        "'{rel}' resolves to {}, outside the extension root {}",
                        real.display(),
                        real_root.display()
                    );
                }

                std::fs::read_to_string(&real)
                    .with_context(|| format!("Failed to read {}", path.display()))
            }
            Self::Volume {
                state,
                target,
                ext_name,
            } => {
                let in_volume = format!("{target}/includes/{ext_name}/{rel}");

                // The mountpoint is fast and needs no container, but it is
                // frequently root-owned, so a failure here is expected rather
                // than exceptional — fall through quietly.
                if let Ok(mountpoint) = volume_mountpoint(state) {
                    if let Ok(content) = std::fs::read_to_string(mountpoint.join(&in_volume)) {
                        return Ok(content);
                    }
                }

                read_via_container(state, &format!("/opt/_avocado/{in_volume}"))
            }
        }
    }

    /// Human-readable description of the root, for error messages.
    pub fn describe(&self) -> String {
        match self {
            Self::Dir { root, origin } => format!("{} {}", origin.label(), root.display()),
            Self::Volume {
                state,
                target,
                ext_name,
            } => format!(
                "SDK volume '{}' at /opt/_avocado/{target}/includes/{ext_name}",
                state.volume_name
            ),
        }
    }

    /// The host path of the extension's config file, when one exists.
    ///
    /// Only directory-backed readers have a meaningful host path; a volume
    /// reader reports the in-container path so messages still point somewhere.
    pub fn config_path(&self, config_name: &str) -> String {
        match self {
            Self::Dir { root, .. } => root.join(config_name).to_string_lossy().to_string(),
            Self::Volume {
                target, ext_name, ..
            } => format!("/opt/_avocado/{target}/includes/{ext_name}/{config_name}"),
        }
    }
}

impl ExtFileReader for ExtSourceReader {
    fn read_file(&self, rel: &str) -> Result<String> {
        self.read(rel)
    }

    fn describe(&self) -> String {
        ExtSourceReader::describe(self)
    }
}

/// Resolve a Docker volume's host mountpoint.
pub fn volume_mountpoint(state: &VolumeState) -> Result<PathBuf> {
    let output = std::process::Command::new(&state.container_tool)
        .args([
            "volume",
            "inspect",
            &state.volume_name,
            "--format",
            "{{.Mountpoint}}",
        ])
        .output()
        .with_context(|| format!("Failed to inspect volume '{}'", state.volume_name))?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get mountpoint for volume '{}': {}",
            state.volume_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mountpoint = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if mountpoint.is_empty() {
        anyhow::bail!("Volume '{}' has no mountpoint", state.volume_name);
    }

    Ok(PathBuf::from(mountpoint))
}

/// `cat` a path out of the SDK volume using a minimal throwaway container.
///
/// Used when the volume's host mountpoint isn't directly readable.
fn read_via_container(state: &VolumeState, container_path: &str) -> Result<String> {
    let images_to_try = [
        "busybox:latest",
        "alpine:latest",
        "docker.io/library/busybox:latest",
    ];

    for image in &images_to_try {
        let output = std::process::Command::new(&state.container_tool)
            .args([
                "run",
                "--rm",
                "-v",
                &format!("{}:/opt/_avocado:ro", state.volume_name),
                image,
                "cat",
                container_path,
            ])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let content = String::from_utf8_lossy(&out.stdout).to_string();
                if content.is_empty() {
                    anyhow::bail!("{container_path} is empty");
                }
                return Ok(content);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                // A missing file is conclusive — trying another image won't help.
                if stderr.contains("No such file") || stderr.contains("not found") {
                    anyhow::bail!("{container_path} not found in volume");
                }
            }
            // Image unavailable or the tool failed; try the next image.
            Err(_) => {}
        }
    }

    anyhow::bail!("Failed to read {container_path} via container")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avocado_ext_reader_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dir_reader_reads_relative_files() {
        let dir = tmpdir("read");
        fs::write(dir.join("VERSION"), "1.2.3\n").unwrap();

        let reader = ExtSourceReader::dir(&dir, DirOrigin::SourcePath);
        assert_eq!(reader.read("VERSION").unwrap(), "1.2.3\n");
        assert!(reader.read("nope").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn dir_reader_rejects_symlink_out_of_the_root() {
        let dir = tmpdir("escape");
        let outside = dir.join("outside");
        let root = dir.join("ext");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, "sekrit\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("VERSION")).unwrap();

        let reader = ExtSourceReader::dir(&root, DirOrigin::SourcePath);
        let err = reader.read("VERSION").unwrap_err();
        assert!(
            err.to_string().contains("outside the extension root"),
            "{err:#}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_reader_reads_nested_paths() {
        let dir = tmpdir("nested");
        fs::create_dir_all(dir.join("crates/app")).unwrap();
        fs::write(dir.join("crates/app/Cargo.toml"), "[package]\n").unwrap();

        let reader = ExtSourceReader::dir(&dir, DirOrigin::SourcePath);
        assert_eq!(reader.read("crates/app/Cargo.toml").unwrap(), "[package]\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_ext_config_prefers_yaml() {
        let dir = tmpdir("prefers_yaml");
        fs::write(dir.join("avocado.yaml"), "version: '1.0.0'\n").unwrap();
        fs::write(dir.join("avocado.yml"), "version: '2.0.0'\n").unwrap();

        let reader = ExtSourceReader::dir(&dir, DirOrigin::SourcePath);
        let (name, content) = reader.read_ext_config().unwrap();
        assert_eq!(name, "avocado.yaml");
        assert!(content.contains("1.0.0"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_ext_config_falls_back_to_yml() {
        let dir = tmpdir("yml");
        fs::write(dir.join("avocado.yml"), "version: '2.0.0'\n").unwrap();

        let reader = ExtSourceReader::dir(&dir, DirOrigin::SourcePath);
        let (name, content) = reader.read_ext_config().unwrap();
        assert_eq!(name, "avocado.yml");
        assert!(content.contains("2.0.0"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_ext_config_error_names_the_root() {
        let dir = tmpdir("empty");
        let reader = ExtSourceReader::dir(&dir, DirOrigin::SourcePath);
        let err = reader.read_ext_config().unwrap_err();
        assert!(
            format!("{err:#}").contains("extension source path"),
            "{err:#}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `type: path` extension must never fall back to a container copy — a
    /// stale fetch would shadow the working copy under test.
    #[test]
    fn path_source_has_exactly_one_candidate() {
        let source = ExtensionSource::Path {
            path: "../my-ext".to_string(),
            include: None,
        };
        let candidates = ExtSourceReader::candidates(
            "my-ext",
            &source,
            Path::new("/projects/app"),
            "qemux86-64",
            None,
        );
        assert_eq!(candidates.len(), 1);
        match &candidates[0] {
            ExtSourceReader::Dir { root, origin } => {
                assert_eq!(*origin, DirOrigin::SourcePath);
                assert!(root.ends_with("my-ext"), "{}", root.display());
            }
            other => panic!("expected a Dir candidate, got {other:?}"),
        }
    }

    #[test]
    fn package_source_tries_container_then_dev_fallback() {
        let source = ExtensionSource::Package {
            version: "*".to_string(),
            package: None,
            repo_name: None,
            include: None,
        };
        let candidates = ExtSourceReader::candidates(
            "my-ext",
            &source,
            Path::new("/projects/app"),
            "qemux86-64",
            None,
        );
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0]
            .describe()
            .contains("/opt/_avocado/qemux86-64/includes/my-ext"));
        assert!(candidates[1]
            .describe()
            .contains("/projects/app/.avocado/qemux86-64/includes/my-ext"));
    }

    #[test]
    fn git_source_with_a_volume_tries_container_then_volume() {
        let source = ExtensionSource::Git {
            url: "https://example.invalid/x".to_string(),
            git_ref: None,
            sparse_checkout: None,
            include: None,
        };
        let state = VolumeState {
            volume_name: "avo-test".to_string(),
            source_path: "/projects/app".to_string(),
            container_tool: "docker".to_string(),
        };
        let candidates = ExtSourceReader::candidates(
            "my-ext",
            &source,
            Path::new("/projects/app"),
            "qemux86-64",
            Some(&state),
        );
        assert_eq!(candidates.len(), 2);
        assert!(matches!(candidates[0], ExtSourceReader::Dir { .. }));
        assert!(matches!(candidates[1], ExtSourceReader::Volume { .. }));
        assert!(candidates[1].describe().contains("avo-test"));
    }

    #[test]
    fn config_path_reports_a_usable_location() {
        let reader = ExtSourceReader::dir("/projects/my-ext", DirOrigin::SourcePath);
        assert_eq!(
            reader.config_path("avocado.yml"),
            "/projects/my-ext/avocado.yml"
        );

        let volume = ExtSourceReader::Volume {
            state: VolumeState {
                volume_name: "avo-test".to_string(),
                source_path: "/projects/app".to_string(),
                container_tool: "docker".to_string(),
            },
            target: "qemux86-64".to_string(),
            ext_name: "my-ext".to_string(),
        };
        assert_eq!(
            volume.config_path("avocado.yaml"),
            "/opt/_avocado/qemux86-64/includes/my-ext/avocado.yaml"
        );
    }
}
