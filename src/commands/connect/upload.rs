use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::commands::connect::client::{
    self, ArtifactParam, ArtifactUploadSpec, BlobParts, CompleteRuntimeRequest, CompletedPart,
    ConnectClient, CreateRuntimeRequest, RuntimeParams,
};
use crate::utils::config::{load_config, Config};
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::prerequisites::{check_prerequisites, TaskPrerequisites};
use crate::utils::stamps::StampRequirement;
use crate::utils::target::resolve_target_required;

const PART_SIZE: u64 = 52_428_800; // 50 MiB, matching API's @part_size

pub struct ConnectUploadCommand {
    pub org: String,
    pub project: String,
    pub runtime: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub config_path: String,
    pub target: Option<String>,
    pub file: Option<String>,
    pub profile: Option<String>,
}

struct ArtifactInfo {
    image_id: String,
    path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

impl TaskPrerequisites for ConnectUploadCommand {
    fn required_stamps(&self) -> Vec<StampRequirement> {
        vec![StampRequirement::runtime_build(&self.runtime)]
    }

    fn task_description(&self) -> String {
        format!("Cannot upload runtime '{}'", self.runtime)
    }
}

impl ConnectUploadCommand {
    pub async fn execute(&self) -> Result<()> {
        // 1. Load config and resolve profile
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let connect = ConnectClient::from_profile(profile)?;

        // 2. Load project config (needed for prerequisite checks and content key detection)
        let project_config =
            load_config(&self.config_path).context("Failed to load avocado.yaml")?;

        // 3. When exporting from Docker, validate that the runtime has been built
        //    before attempting anything. Skip when --file is provided (user manages state).
        if self.file.is_none() {
            let target = resolve_target_required(self.target.as_deref(), &project_config)?;
            let container_image = project_config
                .get_sdk_image()
                .context("No SDK container image specified in configuration")?;
            let container = SdkContainer::from_config(&self.config_path, &project_config)?;
            check_prerequisites(self, &target, &container, container_image).await?;
        }

        // 3. Get artifacts on disk (export from Docker or use --file)
        let (artifacts_dir, _tmp_dir) = self.get_artifacts_dir(&project_config).await?;

        // 4. Read manifest and derive version + build_id
        let manifest = read_manifest(&artifacts_dir)?;
        let version = self
            .version
            .clone()
            .unwrap_or_else(|| format_version_from_manifest(&manifest));
        let build_id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // 5. Discover artifacts from manifest (no hashing needed — image_id is the dedup key)
        print_info("Discovering artifacts...", OutputLevel::Normal);
        let artifact_infos = discover_artifacts(&artifacts_dir, &manifest)?;

        for info in &artifact_infos {
            print_info(
                &format!("  {} ({})", info.image_id, format_bytes(info.size_bytes),),
                OutputLevel::Normal,
            );
        }

        // 6. Read delegation info from build volume.
        // Only send delegation fields if the runtime has an explicit content_key configured
        // (Level 1+). At Level 0 (server-managed), the server handles targets signing and
        // expects these fields to be absent.
        let has_content_key = project_config
            .get_runtime_content_key_name(&self.runtime)
            .is_some();

        let delegation = if has_content_key {
            let d = read_delegation_info(&artifacts_dir)
                .context("No TUF delegation files found in build volume. A content_key is configured — run 'avocado build' first.")?;
            Some(d)
        } else {
            // Level 0: delegation files may exist (for sideload) but the server doesn't need them
            None
        };

        // 7. Create runtime (Step 1)
        print_info(
            &format!("Creating runtime {version}..."),
            OutputLevel::Normal,
        );
        let create_req = CreateRuntimeRequest {
            runtime: RuntimeParams {
                version: version.clone(),
                build_id,
                description: self.description.clone(),
                manifest: Some(manifest),
                artifacts: artifact_infos
                    .iter()
                    .map(|a| ArtifactParam {
                        image_id: a.image_id.clone(),
                        size_bytes: a.size_bytes,
                        sha256: a.sha256.clone(),
                    })
                    .collect(),
                delegated_targets_json: delegation
                    .as_ref()
                    .map(|d| d.delegated_targets_json.clone()),
                content_key_hex: delegation.as_ref().map(|d| d.content_key_hex.clone()),
                content_keyid: delegation.as_ref().map(|d| d.content_keyid.clone()),
            },
        };
        let runtime = connect
            .create_runtime(&self.org, &self.project, &create_req)
            .await?;

        // 7. If runtime is already draft (full dedup or idempotent return), we're done
        if runtime.status == "draft" {
            print_success(
                &format!(
                    "Runtime {} already up to date (status: draft, {} artifact(s))",
                    runtime.version,
                    artifact_infos.len()
                ),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // 8. Upload artifacts (Step 2)
        let completed_parts =
            upload_artifacts(&connect, &runtime.artifacts, &artifact_infos).await?;

        // 9. Complete runtime (Step 3)
        print_info("Completing upload...", OutputLevel::Normal);
        let result = connect
            .complete_runtime(
                &self.org,
                &self.project,
                &runtime.id,
                &CompleteRuntimeRequest {
                    parts: completed_parts,
                },
            )
            .await?;

        print_success(
            &format!(
                "Runtime {} uploaded (status: {}, {} artifact(s))",
                result.version,
                result.status,
                artifact_infos.len()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Get artifact files on disk, either from --file or by exporting from Docker.
    async fn get_artifacts_dir(
        &self,
        config: &Config,
    ) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
        if let Some(ref file_or_dir) = self.file {
            let path = PathBuf::from(file_or_dir);
            if path.is_dir() {
                return Ok((path, None));
            }
            if path.is_file() {
                let tmp = tempfile::tempdir()?;
                extract_tarball(&path, tmp.path())?;
                return Ok((tmp.path().to_path_buf(), Some(tmp)));
            }
            anyhow::bail!("Path not found: {}", path.display());
        }

        // Export tarball from Docker volume, then extract
        print_info(
            "Exporting runtime artifacts from build volume...",
            OutputLevel::Normal,
        );
        let tarball = self.export_tarball(config).await?;
        let tmp = tempfile::tempdir()?;
        extract_tarball(&tarball, tmp.path())?;
        let _ = std::fs::remove_file(&tarball);
        Ok((tmp.path().to_path_buf(), Some(tmp)))
    }

    /// Export the runtime tarball from the Docker volume using `avocado sdk run`.
    async fn export_tarball(&self, config: &Config) -> Result<PathBuf> {
        let output_file = ".connect-upload.tar.gz";
        let parent = PathBuf::from(&self.config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let tarball_path = if parent.as_os_str().is_empty() {
            PathBuf::from(output_file)
        } else {
            parent.join(output_file)
        };

        // Clean up any stale file
        let _ = std::fs::remove_file(&tarball_path);

        // Resolve target
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Write inner script to cwd (mounted as /opt/src in container)
        let config_parent = PathBuf::from(&self.config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let config_dir = if config_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            config_parent
        };

        let inner_script = format!(
            r#"set -euo pipefail
STAGING="${{AVOCADO_PREFIX}}/runtimes/{runtime}/var-staging"
tar czf /opt/src/{output} -C "${{STAGING}}" lib/avocado
"#,
            runtime = self.runtime,
            output = output_file,
        );

        let script_path = config_dir.join(".connect-upload-inner.sh");
        std::fs::write(&script_path, &inner_script)?;

        // Run via avocado sdk run (use argv[0] so avocado-dev works in dev)
        let exe = std::env::args()
            .next()
            .unwrap_or_else(|| "avocado".to_string());
        let status = tokio::process::Command::new(&exe)
            .args([
                "--target",
                &target,
                "sdk",
                "run",
                "--",
                "bash",
                "/opt/src/.connect-upload-inner.sh",
            ])
            .current_dir(&config_dir)
            .status()
            .await
            .context("Failed to run 'avocado sdk run'")?;

        // Clean up inner script
        let _ = std::fs::remove_file(&script_path);

        if !status.success() {
            anyhow::bail!(
                "Failed to export runtime artifacts (exit code {:?})",
                status.code()
            );
        }

        if !tarball_path.exists() {
            anyhow::bail!(
                "Export completed but tarball not found at {}",
                tarball_path.display()
            );
        }

        let size = std::fs::metadata(&tarball_path)?.len();
        print_info(
            &format!(
                "Exported {} ({})",
                tarball_path.display(),
                format_bytes(size)
            ),
            OutputLevel::Normal,
        );

        Ok(tarball_path)
    }
}

// ---------------------------------------------------------------------------
// Tarball extraction
// ---------------------------------------------------------------------------

fn extract_tarball(tarball: &Path, dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(tarball)
        .with_context(|| format!("Failed to open {}", tarball.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(dest)
        .with_context(|| format!("Failed to extract {}", tarball.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest reading
// ---------------------------------------------------------------------------

fn read_manifest(dir: &Path) -> Result<serde_json::Value> {
    // Try the active symlink first, then walk runtimes/*/manifest.json
    let active = dir.join("lib/avocado/active/manifest.json");
    if active.exists() {
        let content = std::fs::read_to_string(&active)?;
        return serde_json::from_str(&content).context("Failed to parse manifest.json");
    }

    let runtimes_dir = dir.join("lib/avocado/runtimes");
    if runtimes_dir.exists() {
        for entry in std::fs::read_dir(&runtimes_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let manifest_path = entry.path().join("manifest.json");
                if manifest_path.exists() {
                    let content = std::fs::read_to_string(&manifest_path)?;
                    return serde_json::from_str(&content).context("Failed to parse manifest.json");
                }
            }
        }
    }

    anyhow::bail!(
        "manifest.json not found in {}. Have you run 'avocado build'?",
        dir.display()
    )
}

fn format_version_from_manifest(manifest: &serde_json::Value) -> String {
    if let Some(runtime) = manifest.get("runtime") {
        if let (Some(name), Some(ver)) = (
            runtime.get("name").and_then(|v| v.as_str()),
            runtime.get("version").and_then(|v| v.as_str()),
        ) {
            return format!("{name}-{ver}");
        }
    }
    manifest
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// Artifact discovery (manifest-driven, with SHA256 hashing for TUF)
// ---------------------------------------------------------------------------

/// Discover artifacts from the manifest's extensions list and os_bundle.
/// Each extension has an `image_id` that maps to a `{image_id}.raw` file on disk.
/// The os_bundle (if present) is also included as an artifact.
/// Computes SHA256 of each artifact for TUF target metadata.
fn discover_artifacts(dir: &Path, manifest: &serde_json::Value) -> Result<Vec<ArtifactInfo>> {
    let images_dir = dir.join("lib/avocado/images");

    let extensions = manifest
        .get("extensions")
        .and_then(|v| v.as_array())
        .context("Manifest missing 'extensions' array")?;

    let mut artifacts = Vec::new();

    for ext in extensions {
        let image_id = ext
            .get("image_id")
            .and_then(|v| v.as_str())
            .context("Extension missing 'image_id' field")?;

        let path = images_dir.join(format!("{image_id}.raw"));
        if !path.exists() {
            anyhow::bail!(
                "Artifact file not found: {} (image_id: {})",
                path.display(),
                image_id,
            );
        }

        let size_bytes = std::fs::metadata(&path)
            .with_context(|| format!("Failed to stat {}", path.display()))?
            .len();

        let sha256 =
            compute_sha256(&path).with_context(|| format!("Failed to hash {}", path.display()))?;

        artifacts.push(ArtifactInfo {
            image_id: image_id.to_string(),
            path,
            size_bytes,
            sha256,
        });
    }

    // Include OS bundle artifact if present in manifest
    if let Some(os_bundle) = manifest.get("os_bundle") {
        let image_id = os_bundle
            .get("image_id")
            .and_then(|v| v.as_str())
            .context("os_bundle missing 'image_id' field")?;

        let path = images_dir.join(format!("{image_id}.raw"));
        if !path.exists() {
            anyhow::bail!(
                "OS bundle artifact not found: {} (image_id: {})",
                path.display(),
                image_id,
            );
        }

        let size_bytes = std::fs::metadata(&path)
            .with_context(|| format!("Failed to stat {}", path.display()))?
            .len();

        let sha256 =
            compute_sha256(&path).with_context(|| format!("Failed to hash {}", path.display()))?;

        artifacts.push(ArtifactInfo {
            image_id: image_id.to_string(),
            path,
            size_bytes,
            sha256,
        });
    }

    if artifacts.is_empty() {
        anyhow::bail!("No artifacts found in manifest. Have you run 'avocado build'?");
    }

    Ok(artifacts)
}

fn compute_sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Chunked upload with progress bars
// ---------------------------------------------------------------------------

async fn upload_artifacts(
    connect: &ConnectClient,
    upload_specs: &[ArtifactUploadSpec],
    artifact_infos: &[ArtifactInfo],
) -> Result<Vec<BlobParts>> {
    let mut all_completed = Vec::new();
    let mut to_upload = Vec::new();
    let mut skipped = 0;

    for spec in upload_specs {
        if spec.parts.is_empty() {
            print_info(
                &format!("  {} (already uploaded, skipping)", spec.image_id),
                OutputLevel::Normal,
            );
            skipped += 1;
        } else {
            to_upload.push(spec);
        }
    }

    if to_upload.is_empty() {
        if skipped > 0 {
            print_info(
                &format!("All {skipped} artifact(s) already uploaded."),
                OutputLevel::Normal,
            );
        }
        return Ok(all_completed);
    }

    print_info(
        &format!(
            "Uploading {} artifact(s){}...",
            to_upload.len(),
            if skipped > 0 {
                format!(" ({skipped} already uploaded)")
            } else {
                String::new()
            }
        ),
        OutputLevel::Normal,
    );

    let multi = MultiProgress::new();

    for spec in &to_upload {
        let artifact = artifact_infos
            .iter()
            .find(|a| a.image_id == spec.image_id)
            .with_context(|| format!("API returned unknown image_id '{}'", spec.image_id))?;

        let pb = multi.add(ProgressBar::new(artifact.size_bytes));
        pb.set_style(
            ProgressStyle::with_template(
                "  {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
            )?
            .progress_chars("#>-"),
        );
        pb.set_message(format!("{}...", &spec.image_id[..8]));

        let mut file = tokio::fs::File::open(&artifact.path).await?;
        let mut completed_parts = Vec::new();

        for part in &spec.parts {
            let offset = (part.part_number - 1) * PART_SIZE;
            let remaining = artifact.size_bytes.saturating_sub(offset);
            let chunk_size = std::cmp::min(remaining, PART_SIZE) as usize;

            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; chunk_size];
            file.read_exact(&mut buf).await?;

            // Retry up to 3 times
            let mut etag = None;
            for attempt in 0..3u32 {
                match connect.upload_part(&part.upload_url, buf.clone()).await {
                    Ok(e) => {
                        etag = Some(e);
                        break;
                    }
                    Err(e) if attempt < 2 => {
                        let delay =
                            std::time::Duration::from_millis(500 * (u64::from(attempt) + 1));
                        tokio::time::sleep(delay).await;
                        eprintln!(
                            "  Retrying part {} of {} (attempt {}/3): {}",
                            part.part_number,
                            spec.image_id,
                            attempt + 2,
                            e
                        );
                    }
                    Err(e) => {
                        anyhow::bail!(
                            "Failed to upload part {} of '{}' after 3 attempts: {}",
                            part.part_number,
                            spec.image_id,
                            e
                        );
                    }
                }
            }

            completed_parts.push(CompletedPart {
                part_number: part.part_number,
                etag: etag.unwrap(),
            });

            pb.set_position(std::cmp::min(offset + PART_SIZE, artifact.size_bytes));
        }

        pb.finish_with_message(format!("{}... (done)", &spec.image_id[..8]));

        all_completed.push(BlobParts {
            image_id: spec.image_id.clone(),
            parts: completed_parts,
        });
    }

    Ok(all_completed)
}

// ---------------------------------------------------------------------------
// Delegation info
// ---------------------------------------------------------------------------

struct DelegationInfo {
    delegated_targets_json: String,
    content_key_hex: String,
    content_keyid: String,
}

/// Read TUF delegation info from `lib/avocado/tuf-staging/` inside the exported artifacts dir.
/// Returns `None` if the staging directory or required files are absent.
fn read_delegation_info(artifacts_dir: &Path) -> Option<DelegationInfo> {
    let staging = artifacts_dir.join("lib/avocado/tuf-staging");

    // Parse targets.json to extract content key and role name
    let targets_path = staging.join("targets.json");
    let targets_raw = std::fs::read_to_string(&targets_path).ok()?;
    let targets: serde_json::Value = serde_json::from_str(&targets_raw).ok()?;

    let delegations = targets.pointer("/signed/delegations")?;
    let roles = delegations.get("roles")?.as_array()?;
    let role = roles.first()?;

    let role_name = role.get("name")?.as_str()?;
    let content_keyid = role
        .get("keyids")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();

    let content_key_hex = delegations
        .pointer(&format!("/keys/{content_keyid}/keyval/public"))?
        .as_str()?
        .to_string();

    // Derive runtime UUID from role name ("runtime-<uuid>")
    let runtime_uuid = role_name.strip_prefix("runtime-")?;

    // Read the delegation file as raw JSON
    let delegation_path = staging
        .join("delegations")
        .join(format!("runtime-{runtime_uuid}.json"));
    let delegated_targets_json = std::fs::read_to_string(&delegation_path).ok()?;

    Some(DelegationInfo {
        delegated_targets_json,
        content_key_hex,
        content_keyid,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1} {}", size, UNITS[i])
    }
}
