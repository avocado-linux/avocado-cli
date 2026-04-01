//! Save command implementation for archiving avocado build state.

use anyhow::{Context, Result};
use chrono::Utc;
use gzp::deflate::Gzip;
use gzp::par::compress::{ParCompress, ParCompressBuilder};
use gzp::ZWriter;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::utils::config::Config;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;
use crate::utils::volume::VolumeState;

/// Prefix for all paths inside the archive
const ARCHIVE_PREFIX: &str = "avocado-state";

/// Current archive manifest version
const MANIFEST_VERSION: u32 = 1;

/// Metadata stored in the archive describing its contents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    /// Archive format version
    pub version: u32,
    /// ISO 8601 timestamp of archive creation
    pub created_at: String,
    /// CLI version that created the archive
    pub cli_version: String,
    /// Target architecture
    pub target: String,
    /// Container tool used (docker/podman)
    pub container_tool: String,
    /// Relative path within archive to volume contents
    pub volume_contents_path: String,
    /// Whether the archive includes the source directory
    #[serde(default)]
    pub include_src: bool,
}

/// Save the current build state to a compressed archive.
pub struct SaveCommand {
    output: String,
    config_path: String,
    target: Option<String>,
    verbose: bool,
    container_tool: String,
    include_src: bool,
}

impl SaveCommand {
    pub fn new(
        output: String,
        config_path: String,
        target: Option<String>,
        verbose: bool,
        container_tool: String,
        include_src: bool,
    ) -> Self {
        Self {
            output,
            config_path,
            target,
            verbose,
            container_tool,
            include_src,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        let config_dir = Path::new(&self.config_path)
            .parent()
            .unwrap_or(Path::new("."));
        let volume_state = VolumeState::load_from_dir(config_dir)?.ok_or_else(|| {
            anyhow::anyhow!(
                "No .avocado-state found. Run 'avocado install' or 'avocado build' first."
            )
        })?;

        if self.verbose {
            print_info(
                &format!(
                    "Saving volume '{}' for target '{}'",
                    volume_state.volume_name, target
                ),
                OutputLevel::Normal,
            );
        }

        let src_dir = if self.include_src {
            let resolved = config
                .get_resolved_src_dir(&self.config_path)
                .unwrap_or_else(|| {
                    let parent = Path::new(&self.config_path)
                        .parent()
                        .unwrap_or(Path::new("."));
                    if parent.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        parent.to_path_buf()
                    }
                });
            if !resolved.is_dir() {
                anyhow::bail!("src_dir '{}' does not exist", resolved.display());
            }
            if self.verbose {
                print_info(
                    &format!("Including src_dir: {}", resolved.display()),
                    OutputLevel::Normal,
                );
            }
            Some(resolved)
        } else {
            None
        };

        let output_path = PathBuf::from(&self.output);
        let config_path = PathBuf::from(&self.config_path);
        let volume_name = volume_state.volume_name.clone();
        let container_tool = self.container_tool.clone();
        let target_clone = target.clone();
        let include_src = self.include_src;

        tokio::task::spawn_blocking(move || {
            build_save_archive(
                &output_path,
                &config_path,
                &volume_name,
                &target_clone,
                &container_tool,
                include_src,
                src_dir.as_deref(),
            )
        })
        .await??;

        let metadata = fs::metadata(&self.output)?;
        let size_mb = metadata.len() as f64 / (1024.0 * 1024.0);
        print_success(
            &format!("Saved to {} ({:.1} MB)", self.output, size_mb),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

fn build_save_archive(
    output_path: &Path,
    config_path: &Path,
    volume_name: &str,
    target: &str,
    container_tool: &str,
    include_src: bool,
    src_dir: Option<&Path>,
) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("Failed to create {}", output_path.display()))?;
    let encoder: ParCompress<'static, Gzip, File> = ParCompressBuilder::new()
        .num_threads(num_cpus::get())
        .map_err(|e| anyhow::anyhow!("Failed to configure parallel compression: {e}"))?
        .from_writer(file);
    let mut tar = tar::Builder::new(encoder);

    // 1. Add manifest.json
    let manifest = ArchiveManifest {
        version: MANIFEST_VERSION,
        created_at: Utc::now().to_rfc3339(),
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        target: target.to_string(),
        container_tool: container_tool.to_string(),
        volume_contents_path: "volume".to_string(),
        include_src,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(
        &mut header,
        format!("{ARCHIVE_PREFIX}/manifest.json"),
        &manifest_bytes[..],
    )?;

    // 2. Add avocado.yaml
    if config_path.exists() {
        let config_name = config_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("avocado.yaml"));
        tar.append_path_with_name(
            config_path,
            format!("{ARCHIVE_PREFIX}/config/{}", config_name.to_string_lossy()),
        )?;
    }

    // 3. Add .avocado/ directory if present
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let avocado_dir = config_dir.join(".avocado");
    if avocado_dir.is_dir() {
        tar.append_dir_all(format!("{ARCHIVE_PREFIX}/config/.avocado"), &avocado_dir)?;
    }

    // 4. Add src_dir if requested
    if let Some(src_dir) = src_dir {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏-"),
        );
        pb.set_message("Packing src_dir...");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        tar.append_dir_all(format!("{ARCHIVE_PREFIX}/src"), src_dir)?;
        pb.finish_and_clear();
    }

    // 5. Query volume size for progress bar
    let volume_size = Command::new(container_tool)
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{volume_name}:/data:ro"),
            "busybox",
            "du",
            "-sb",
            "/data",
        ])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let out = String::from_utf8_lossy(&o.stdout);
                out.split_whitespace().next()?.parse::<u64>().ok()
            } else {
                None
            }
        });

    // 6. Stream entire volume contents from container
    let pb = if let Some(total) = volume_size {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} Packing volume [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} Packing volume ({bytes}, {elapsed})")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏-"),
        );
        pb
    };
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    let mut child = Command::new(container_tool)
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{volume_name}:/data:ro"),
            "busybox",
            "tar",
            "cf",
            "-",
            "-C",
            "/data",
            ".",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to start container for volume export")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture container stdout"))?;
    let stderr_handle = child.stderr.take();

    let mut inner_archive = tar::Archive::new(stdout);
    let mut bytes_written: u64 = 0;
    for entry_result in inner_archive.entries()? {
        let mut entry =
            entry_result.with_context(|| "Failed to read entry from volume tar stream")?;
        let original_path = entry.path()?.to_path_buf();
        let new_path = PathBuf::from(format!("{ARCHIVE_PREFIX}/volume")).join(&original_path);
        let entry_size = entry.header().size().unwrap_or(0);

        let mut new_header = entry.header().clone();

        let entry_type = entry.header().entry_type();
        // Re-prefix hardlink targets so they resolve correctly in the archive
        if entry_type.is_hard_link() {
            if let Ok(Some(link_name)) = entry.header().link_name() {
                let new_link = PathBuf::from(format!("{ARCHIVE_PREFIX}/volume")).join(link_name);
                new_header.set_link_name(&new_link)?;
            }
        }

        if entry_type.is_dir() || entry_type.is_symlink() || entry_type.is_hard_link() {
            tar.append_data(&mut new_header, &new_path, std::io::empty())?;
        } else {
            tar.append_data(&mut new_header, &new_path, &mut entry)?;
        }

        bytes_written += entry_size;
        pb.set_position(bytes_written);
    }

    let status = child.wait()?;
    pb.finish_and_clear();

    if !status.success() {
        let stderr_text = if let Some(mut stderr) = stderr_handle {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok();
            buf
        } else {
            String::new()
        };
        anyhow::bail!("Volume export failed: {}", stderr_text.trim());
    }

    let mut encoder = tar.into_inner()?;
    encoder
        .finish()
        .map_err(|e| anyhow::anyhow!("Failed to finish compression: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_manifest_serialization() {
        let manifest = ArchiveManifest {
            version: 1,
            created_at: "2026-04-01T12:00:00Z".to_string(),
            cli_version: "0.33.0".to_string(),
            target: "aarch64".to_string(),
            container_tool: "docker".to_string(),
            volume_contents_path: "volume".to_string(),
            include_src: false,
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: ArchiveManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.target, "aarch64");
        assert_eq!(parsed.container_tool, "docker");
        assert!(!parsed.include_src);
    }

    #[test]
    fn test_archive_manifest_round_trip() {
        let manifest = ArchiveManifest {
            version: MANIFEST_VERSION,
            created_at: Utc::now().to_rfc3339(),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            target: "x86_64".to_string(),
            container_tool: "podman".to_string(),
            volume_contents_path: "volume".to_string(),
            include_src: true,
        };

        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let parsed: ArchiveManifest = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(parsed.version, manifest.version);
        assert_eq!(parsed.cli_version, manifest.cli_version);
        assert_eq!(parsed.target, manifest.target);
    }
}
