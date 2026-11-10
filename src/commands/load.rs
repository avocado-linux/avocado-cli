//! Load command implementation for restoring avocado build state from an archive.

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::utils::lockfile::LockFile;
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};
use crate::utils::volume::VolumeState;

use super::save::ArchiveManifest;

/// Load build state from a compressed archive.
pub struct LoadCommand {
    input: String,
    config_path: String,
    verbose: bool,
    container_tool: String,
    force: bool,
}

impl LoadCommand {
    pub fn new(
        input: String,
        config_path: String,
        verbose: bool,
        container_tool: String,
        force: bool,
    ) -> Self {
        Self {
            input,
            config_path,
            verbose,
            container_tool,
            force,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let input_path = Path::new(&self.input);
        if !input_path.exists() {
            anyhow::bail!("Input file not found: {}", self.input);
        }

        let config_dir = Path::new(&self.config_path)
            .parent()
            .unwrap_or(Path::new("."));

        // Check for existing state
        if !self.force {
            if let Some(existing) = VolumeState::load_from_dir(config_dir)? {
                anyhow::bail!(
                    "Volume '{}' already exists for this project. Use --force to overwrite.",
                    existing.volume_name
                );
            }
        }

        if self.verbose {
            print_info(
                &format!("Loading state from {}", self.input),
                OutputLevel::Normal,
            );
        }

        let input = self.input.clone();
        let config_path = PathBuf::from(&self.config_path);
        let container_tool = self.container_tool.clone();
        let force = self.force;
        let verbose = self.verbose;

        let (manifest, volume_name) = tokio::task::spawn_blocking(move || {
            import_archive(&input, &config_path, &container_tool, force, verbose)
        })
        .await??;

        // Write .avocado-state
        let config_dir_abs = if config_dir == Path::new(".") || config_dir == Path::new("") {
            std::env::current_dir()?
        } else if config_dir.is_absolute() {
            config_dir.to_path_buf()
        } else {
            std::env::current_dir()?.join(config_dir)
        };

        let volume_state = VolumeState {
            volume_name: volume_name.clone(),
            source_path: config_dir_abs.to_string_lossy().to_string(),
            container_tool: self.container_tool.clone(),
            stacks: false,
        };
        volume_state.save_to_dir(config_dir)?;

        print_success(
            &format!(
                "Loaded state for target '{}' into volume '{}'",
                manifest.target, volume_name
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

fn spinner() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg} ({bytes}, {elapsed})")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏-"),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

fn import_archive(
    input_path: &str,
    config_path: &Path,
    container_tool: &str,
    force: bool,
    verbose: bool,
) -> Result<(ArchiveManifest, String)> {
    // Extract to temp directory
    let temp_dir = tempfile::tempdir().with_context(|| "Failed to create temp directory")?;
    let temp_path = temp_dir.path();

    let pb = spinner();
    pb.set_message("Extracting archive");

    let file = File::open(input_path).with_context(|| format!("Failed to open {input_path}"))?;
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let pb_reader = pb.wrap_read(file);
    let decoder = MultiGzDecoder::new(pb_reader);
    let mut archive = tar::Archive::new(decoder);
    if file_size > 0 {
        pb.set_length(file_size);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
    }
    let has_volume = unpack_host_entries(&mut archive, temp_path)?;
    pb.finish_and_clear();

    // Read and validate manifest
    let manifest_path = temp_path.join("avocado-state/manifest.json");
    let manifest_content = fs::read_to_string(&manifest_path)
        .with_context(|| "Archive does not contain avocado-state/manifest.json")?;
    let manifest: ArchiveManifest =
        serde_json::from_str(&manifest_content).with_context(|| "Invalid manifest.json")?;

    if manifest.version != 1 {
        anyhow::bail!(
            "Unsupported archive version: {} (this CLI supports version 1)",
            manifest.version
        );
    }

    let current_version = env!("CARGO_PKG_VERSION");
    if manifest.cli_version != current_version {
        print_warning(
            &format!(
                "Archive was created with CLI v{}, current is v{}",
                manifest.cli_version, current_version
            ),
            OutputLevel::Normal,
        );
    }

    let config_dir = crate::utils::config::config_file_dir(config_path);

    // Create new Docker volume
    let volume_name = format!("avo-{}", uuid::Uuid::new_v4());

    if verbose {
        print_info(
            &format!("Creating volume '{}'", volume_name),
            OutputLevel::Normal,
        );
    }

    let output = Command::new(container_tool)
        .args([
            "volume",
            "create",
            "--label",
            &format!("avocado.source_path={}", config_dir.display()),
            &volume_name,
        ])
        .output()
        .with_context(|| "Failed to create Docker volume")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to create volume: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Import volume contents
    if !has_volume {
        anyhow::bail!("Archive does not contain volume data");
    }

    let pb = spinner();
    pb.set_message("Importing volume");

    // Over stdin rather than a bind mount: under the avocado-vm the daemon
    // cannot see a host temp directory, and would mount an empty one.
    // The CLI owns this stdin pipe, so `-i` without a TTY is the arrangement
    // whatever the terminal is.
    let mut child = Command::new(container_tool)
        .args(["run", "--rm"])
        .args(crate::utils::interactivity::ContainerStdio::StdinOnly.flags())
        .args([
            "-v",
            &format!("{volume_name}:/data"),
            "busybox",
            "tar",
            "xf",
            "-",
            "-C",
            "/data",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to start container for volume import")?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open container stdin"))?;
    // Drained on its own thread: tar prints a line per entry it cannot
    // create, and a full stderr pipe would stop it reading stdin while this
    // thread blocks writing it.
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok();
            buf
        })
    });
    // A write error usually means the container exited early; its stderr
    // says why.
    let streamed = stream_volume_entries(Path::new(input_path), stdin);
    let status = child.wait()?;
    let stderr_text = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    pb.finish_and_clear();

    if !status.success() {
        // Clean up the volume on failure
        let _ = Command::new(container_tool)
            .args(["volume", "rm", &volume_name])
            .output();
        anyhow::bail!("Volume import failed: {}", stderr_text.trim());
    }
    if let Err(e) = streamed {
        let _ = Command::new(container_tool)
            .args(["volume", "rm", &volume_name])
            .output();
        return Err(e.context("Volume import failed"));
    }

    // Restore config files. After the volume import, which reads the input
    // archive a second time: a restored src_dir could overwrite that file.
    let archive_config_dir = temp_path.join("avocado-state/config");

    // Restore avocado.yaml
    let config_filename = config_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("avocado.yaml"));
    let archive_yaml = archive_config_dir.join(config_filename);
    if archive_yaml.exists() {
        if config_path.exists() && !force {
            print_warning(
                &format!(
                    "{} already exists, skipping (use --force to overwrite)",
                    config_path.display()
                ),
                OutputLevel::Normal,
            );
        } else {
            fs::copy(&archive_yaml, config_path)
                .with_context(|| format!("Failed to restore {}", config_path.display()))?;
        }
    }

    // Restore .avocado/ directory
    let archive_avocado_dir = archive_config_dir.join(".avocado");
    if archive_avocado_dir.is_dir() {
        let dest_avocado_dir = config_dir.join(".avocado");
        copy_dir_recursive(&archive_avocado_dir, &dest_avocado_dir)
            .with_context(|| "Failed to restore .avocado/ directory")?;
    }

    // Restore the lock file, normalized to the top-level avocado.lock. Prefer the
    // archive's top-level avocado.lock; fall back to a legacy .avocado/lock.json
    // from older bundles so their pins aren't silently ignored. Don't clobber an
    // existing local lock without --force (mirrors the avocado.yaml handling
    // above), then drop any restored legacy copy so the two can't diverge.
    let dest_lockfile = LockFile::get_path(config_dir);
    let archive_lockfile = {
        let top = archive_config_dir.join("avocado.lock");
        let legacy = archive_config_dir.join(".avocado").join("lock.json");
        if top.is_file() {
            Some(top)
        } else if legacy.is_file() {
            Some(legacy)
        } else {
            None
        }
    };
    if let Some(src) = archive_lockfile {
        if dest_lockfile.exists() && !force {
            print_warning(
                &format!(
                    "{} already exists, skipping (use --force to overwrite)",
                    dest_lockfile.display()
                ),
                OutputLevel::Normal,
            );
        } else {
            fs::copy(&src, &dest_lockfile).with_context(|| "Failed to restore avocado.lock")?;
        }
    }
    // The canonical lock now lives at the top level; drop any legacy copy the
    // .avocado/ restore brought in so `load()` can't read a divergent file.
    let restored_legacy = LockFile::legacy_path(config_dir);
    if restored_legacy.exists() {
        let _ = fs::remove_file(&restored_legacy);
    }

    // Restore src_dir if archive includes it
    let archive_src_dir = temp_path.join("avocado-state/src");
    if archive_src_dir.is_dir() {
        if verbose {
            print_info("Restoring src_dir...", OutputLevel::Normal);
        }
        copy_dir_recursive(&archive_src_dir, config_dir)
            .with_context(|| "Failed to restore src_dir")?;
    }

    Ok((manifest, volume_name))
}

/// Unpack every entry except the volume's into `temp_path`, and say whether the
/// archive has volume entries. Those are not unpacked here: they stream into the
/// volume from the archive itself (`stream_volume_entries`), which keeps their
/// owners, hardlinks and special files as `save` wrote them.
fn unpack_host_entries<R: Read>(archive: &mut tar::Archive<R>, temp_path: &Path) -> Result<bool> {
    let mut has_volume = false;
    let mut dir_modes = Vec::new();
    for entry in archive
        .entries()
        .with_context(|| "Failed to extract archive (corrupted or not a valid tar.gz?)")?
    {
        let mut entry = entry
            .with_context(|| "Failed to extract archive (corrupted or not a valid tar.gz?)")?;
        let path = entry.path()?.into_owned();
        if path.starts_with(VOLUME_PREFIX) {
            has_volume = true;
            continue;
        }
        // As `Archive::unpack` does, a directory gets its mode only after its
        // children are written, so a read-only one does not refuse them.
        if entry.header().entry_type().is_dir() {
            if !path
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            {
                anyhow::bail!("Archive entry escapes the archive root: {}", path.display());
            }
            let dir = temp_path.join(&path);
            fs::create_dir_all(&dir)?;
            if let Ok(mode) = entry.header().mode() {
                dir_modes.push((dir, mode));
            }
            continue;
        }
        entry
            .unpack_in(temp_path)
            .with_context(|| "Failed to extract archive (corrupted or not a valid tar.gz?)")?;
    }
    #[cfg(unix)]
    for (dir, mode) in dir_modes.iter().rev() {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(*mode))?;
    }
    #[cfg(not(unix))]
    let _ = dir_modes;
    Ok(has_volume)
}

/// Where `save` puts the volume's contents in the archive.
const VOLUME_PREFIX: &str = "avocado-state/volume";

/// Re-write the archive's volume entries, relative to the volume root, as a
/// tar stream into `out`. The inverse of `save`'s re-prefixing: headers are
/// kept, and hardlink targets lose the prefix too.
fn stream_volume_entries(input_path: &Path, out: impl Write) -> Result<()> {
    let file = File::open(input_path)
        .with_context(|| format!("Failed to open {}", input_path.display()))?;
    let mut archive = tar::Archive::new(MultiGzDecoder::new(file));
    let mut tar = tar::Builder::new(out);
    for entry in archive.entries()? {
        let mut entry = entry.with_context(|| "Failed to read entry from archive")?;
        let path = entry.path()?.into_owned();
        let Ok(rel) = path.strip_prefix(VOLUME_PREFIX) else {
            continue;
        };
        let rel = if rel.as_os_str().is_empty() {
            Path::new(".").to_path_buf()
        } else {
            rel.to_path_buf()
        };
        let mut header = entry.header().clone();
        let entry_type = header.entry_type();
        if entry_type.is_hard_link() || entry_type.is_symlink() {
            let link = entry
                .link_name()?
                .ok_or_else(|| anyhow::anyhow!("link entry missing link target"))?
                .into_owned();
            // Symlink targets are filesystem paths and stay as they are.
            let link = if entry_type.is_hard_link() {
                link.strip_prefix(VOLUME_PREFIX)
                    .map(Path::to_path_buf)
                    .unwrap_or(link)
            } else {
                link
            };
            tar.append_link(&mut header, &rel, &link)?;
        } else if entry_type.is_dir() {
            tar.append_data(&mut header, &rel, std::io::empty())?;
        } else {
            tar.append_data(&mut header, &rel, &mut entry)?;
        }
    }
    tar.into_inner()?.flush()?;
    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The import streams the archive's volume entries instead of
    /// bind-mounting a host directory the avocado-vm cannot see. The stream
    /// has to be the volume as `save` wrote it: prefix gone (hardlink targets
    /// too), symlink targets untouched, owners and modes kept, other entries
    /// left out.
    /// The import must not deadlock when the container floods stderr before it
    /// has read all of stdin (tar prints a line per entry it cannot create).
    /// The fake tool writes 1 MiB to stderr before reading stdin, against a
    /// 1 MiB volume: with stderr read only after `wait`, both sides block.
    #[cfg(unix)]
    #[test]
    fn import_survives_a_container_that_floods_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let tool = temp.path().join("fake-docker");
        fs::write(
            &tool,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             run) head -c 1048576 /dev/zero >&2; cat >/dev/null; exit 0 ;;\n\
             *) exit 0 ;;\n\
             esac\n",
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

        let archive = temp.path().join("state.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut add = |path: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                tar.append_data(&mut h, path, data).unwrap();
            };
            let manifest = format!(
                r#"{{"version":1,"created_at":"x","cli_version":"{}","target":"t","container_tool":"docker","volume_contents_path":"volume","include_src":false}}"#,
                env!("CARGO_PKG_VERSION")
            );
            add("avocado-state/manifest.json", manifest.as_bytes());
            add("avocado-state/volume/big", &vec![7u8; 1 << 20]);
            tar.into_inner().unwrap().finish().unwrap();
        }
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config = project.join("avocado.yaml");

        let (tx, rx) = std::sync::mpsc::channel();
        let (archive, tool) = (archive.display().to_string(), tool.display().to_string());
        std::thread::spawn(move || {
            let _ = tx.send(import_archive(&archive, &config, &tool, false, false).map(|_| ()));
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("load hung with the container blocked on stderr")
            .unwrap();
    }

    /// A read-only directory in an included src_dir gets its mode only after
    /// its children are written, as `Archive::unpack` does; unpacking entry by
    /// entry applied 0555 first and the child failed with Permission denied.
    /// Volume entries are reported, not unpacked.
    #[cfg(unix)]
    #[test]
    fn host_entries_unpack_under_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let mut buf = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut buf);
            let mut add = |kind: tar::EntryType, path: &str, mode: u32, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(kind);
                h.set_size(data.len() as u64);
                h.set_mode(mode);
                tar.append_data(&mut h, path, data).unwrap();
            };
            add(
                tar::EntryType::Directory,
                "avocado-state/src/ro",
                0o555,
                b"",
            );
            add(
                tar::EntryType::Regular,
                "avocado-state/src/ro/file",
                0o644,
                b"x",
            );
            add(
                tar::EntryType::Regular,
                "avocado-state/volume/sysroot/f",
                0o644,
                b"v",
            );
            tar.finish().unwrap();
        }
        let temp = TempDir::new().unwrap();
        let has_volume =
            unpack_host_entries(&mut tar::Archive::new(&buf[..]), temp.path()).unwrap();

        assert!(has_volume);
        let ro = temp.path().join("avocado-state/src/ro");
        assert_eq!(fs::read_to_string(ro.join("file")).unwrap(), "x");
        assert_eq!(
            fs::metadata(&ro).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert!(!temp.path().join("avocado-state/volume").exists());
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn volume_entries_stream_relative_to_the_volume_root() {
        let temp = TempDir::new().unwrap();
        let archive_path = temp.path().join("state.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                File::create(&archive_path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut add = |kind: tar::EntryType, path: &str, link: Option<&str>, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(kind);
                h.set_size(data.len() as u64);
                h.set_mode(0o4755);
                h.set_uid(0);
                h.set_gid(0);
                match link {
                    Some(l) => tar.append_link(&mut h, path, l).unwrap(),
                    None => tar.append_data(&mut h, path, data).unwrap(),
                }
            };
            add(
                tar::EntryType::Regular,
                "avocado-state/manifest.json",
                None,
                b"{}",
            );
            add(
                tar::EntryType::Directory,
                "avocado-state/volume/.",
                None,
                b"",
            );
            add(
                tar::EntryType::Directory,
                "avocado-state/volume/sysroot/bin",
                None,
                b"",
            );
            add(
                tar::EntryType::Regular,
                "avocado-state/volume/sysroot/bin/su",
                None,
                b"elf",
            );
            add(
                tar::EntryType::Link,
                "avocado-state/volume/sysroot/bin/su2",
                Some("avocado-state/volume/sysroot/bin/su"),
                b"",
            );
            add(
                tar::EntryType::Symlink,
                "avocado-state/volume/sysroot/sh",
                Some("/bin/su"),
                b"",
            );
            add(
                tar::EntryType::Regular,
                "avocado-state/config/avocado.yaml",
                None,
                b"x",
            );
            tar.into_inner().unwrap().finish().unwrap();
        }

        let mut out = Vec::new();
        stream_volume_entries(&archive_path, &mut out).unwrap();

        let mut got = Vec::new();
        for e in tar::Archive::new(&out[..]).entries().unwrap() {
            let mut e = e.unwrap();
            let h = e.header().clone();
            let link = e.link_name().unwrap().map(|l| l.display().to_string());
            let mut body = String::new();
            e.read_to_string(&mut body).unwrap();
            assert_eq!((h.uid().unwrap(), h.mode().unwrap()), (0, 0o4755));
            got.push((e.path().unwrap().display().to_string(), link, body));
        }
        let s = |v: &str| v.to_string();
        assert_eq!(
            got,
            vec![
                (s("."), None, s("")),
                (s("sysroot/bin"), None, s("")),
                (s("sysroot/bin/su"), None, s("elf")),
                (s("sysroot/bin/su2"), Some(s("sysroot/bin/su")), s("")),
                (s("sysroot/sh"), Some(s("/bin/su")), s("")),
            ]
        );
    }

    #[test]
    fn test_copy_dir_recursive() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let dst_path = dst_dir.path().join("output");

        // Create source structure
        fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        fs::create_dir(src_dir.path().join("subdir")).unwrap();
        fs::write(src_dir.path().join("subdir/file2.txt"), "world").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("subdir/file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(dst_path.join("subdir/file2.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn test_load_nonexistent_file() {
        let cmd = LoadCommand::new(
            "/nonexistent/file.tar.gz".to_string(),
            "avocado.yaml".to_string(),
            false,
            "docker".to_string(),
            false,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
