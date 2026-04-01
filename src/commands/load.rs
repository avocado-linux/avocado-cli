//! Load command implementation for restoring avocado build state from an archive.

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    archive
        .unpack(temp_path)
        .with_context(|| "Failed to extract archive (corrupted or not a valid tar.gz?)")?;
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

    // Restore config files
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
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

    // Restore src_dir if archive includes it
    let archive_src_dir = temp_path.join("avocado-state/src");
    if archive_src_dir.is_dir() {
        if verbose {
            print_info("Restoring src_dir...", OutputLevel::Normal);
        }
        copy_dir_recursive(&archive_src_dir, config_dir)
            .with_context(|| "Failed to restore src_dir")?;
    }

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
    let volume_dir = temp_path.join("avocado-state/volume");
    if !volume_dir.exists() {
        anyhow::bail!("Archive does not contain volume data");
    }

    let pb = spinner();
    pb.set_message("Importing volume");

    let mut child = Command::new(container_tool)
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{volume_name}:/data"),
            "-v",
            &format!("{}:/in:ro", volume_dir.display()),
            "busybox",
            "sh",
            "-c",
            "cd /in && tar cf - . | (cd /data && tar xf -)",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to start container for volume import")?;

    let status = child.wait()?;
    pb.finish_and_clear();

    if !status.success() {
        let stderr_text = if let Some(mut stderr) = child.stderr.take() {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok();
            buf
        } else {
            String::new()
        };
        // Clean up the volume on failure
        let _ = Command::new(container_tool)
            .args(["volume", "rm", &volume_name])
            .output();
        anyhow::bail!("Volume import failed: {}", stderr_text.trim());
    }

    Ok((manifest, volume_name))
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
