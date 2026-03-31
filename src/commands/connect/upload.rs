use anyhow::{Context, Result};
use base64::prelude::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::commands::connect::client::{
    self, ArtifactParam, ArtifactUploadSpec, BlobParts, CompleteRuntimeRequest, CompletedPart,
    ConnectClient, ContainerDiscoveryResult, CreateRuntimeRequest, RuntimeParams, UploadPartError,
};
use crate::utils::config::{load_config, Config};
use crate::utils::container::{RunConfig, SdkContainer};
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
    pub publish: bool,
    pub deploy_cohort: Option<String>,
    pub deploy_name: Option<String>,
    pub deploy_tags: Vec<String>,
    pub deploy_activate: bool,
}

struct ArtifactInfo {
    image_id: String,
    path: PathBuf,
    size_bytes: u64,
    sha256: String,
    part_checksums: Vec<String>,
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
        // 0. Validate deploy-after-upload flags
        super::deploy::validate_deploy_flags(
            &self.deploy_cohort,
            &self.deploy_name,
            &self.deploy_tags,
            self.deploy_activate,
        )?;

        // 1. Load config and resolve profile
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let connect = ConnectClient::from_profile(profile)?;

        // 2. Load project config (needed for prerequisite checks and content key detection)
        let project_config =
            load_config(&self.config_path).context("Failed to load avocado.yaml")?;

        if self.file.is_some() {
            // --file path: host-side artifacts (unchanged legacy flow)
            return self.execute_host_path(&connect, &project_config).await;
        }

        // In-container path: zero-copy discovery + parallel upload
        self.execute_container_path(&connect, &project_config).await
    }

    /// Upload from host-side artifacts (--file flag). This is the legacy flow
    /// used when the user provides a pre-built tarball or directory.
    async fn execute_host_path(
        &self,
        connect: &ConnectClient,
        project_config: &Config,
    ) -> Result<()> {
        let (artifacts_dir, _tmp_dir) = self.get_artifacts_dir().await?;

        let manifest = read_manifest(&artifacts_dir)?;
        let version = self
            .version
            .clone()
            .unwrap_or_else(|| format_version_from_manifest(&manifest));
        let build_id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        print_info("Discovering artifacts...", OutputLevel::Normal);
        let artifact_infos = discover_artifacts(&artifacts_dir, &manifest)?;

        for info in &artifact_infos {
            print_info(
                &format!("  {} ({})", info.image_id, format_bytes(info.size_bytes)),
                OutputLevel::Normal,
            );
        }

        let has_content_key = project_config
            .get_runtime_content_key_name(&self.runtime)
            .is_some();

        let delegation = if has_content_key {
            let d = read_delegation_info(&artifacts_dir)
                .context("No TUF delegation files found in build volume. A content_key is configured — run 'avocado build' first.")?;
            Some(d)
        } else {
            None
        };

        let (runtime, num_artifacts) = self
            .create_runtime_api(
                connect,
                version,
                build_id,
                &manifest,
                &artifact_infos
                    .iter()
                    .map(|a| ArtifactParam {
                        image_id: a.image_id.clone(),
                        size_bytes: a.size_bytes,
                        sha256: a.sha256.clone(),
                        part_size: PART_SIZE,
                        part_checksums: a.part_checksums.clone(),
                    })
                    .collect::<Vec<_>>(),
                delegation.as_ref().map(|d| {
                    (
                        &d.delegated_targets_json,
                        &d.content_key_hex,
                        &d.content_keyid,
                    )
                }),
            )
            .await?;

        if runtime.status == "draft" {
            self.handle_draft_status(connect, &runtime, num_artifacts)
                .await?;
            return Ok(());
        }

        // Upload artifacts from host
        let completed_parts = upload_artifacts(
            connect,
            &self.org,
            &self.project,
            &runtime.id,
            &runtime.artifacts,
            &artifact_infos,
        )
        .await?;

        self.complete_and_finalize(connect, &runtime, completed_parts, num_artifacts)
            .await
    }

    /// Upload directly from the Docker volume with zero-copy discovery and
    /// parallel in-container uploads.
    async fn execute_container_path(
        &self,
        connect: &ConnectClient,
        project_config: &Config,
    ) -> Result<()> {
        // Validate prerequisites (runtime has been built)
        let target = resolve_target_required(self.target.as_deref(), project_config)?;
        let container_image = project_config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let container = SdkContainer::from_config(&self.config_path, project_config)?;
        check_prerequisites(self, &target, &container, container_image).await?;

        // Phase A: Discover artifacts inside the container
        let discovery = self.discover_in_container(project_config).await?;

        let manifest = &discovery.manifest;
        let version = self
            .version
            .clone()
            .unwrap_or_else(|| format_version_from_manifest(manifest));
        let build_id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let has_content_key = project_config
            .get_runtime_content_key_name(&self.runtime)
            .is_some();

        // Use delegation from container discovery if content_key is configured
        let delegation_refs = if has_content_key {
            let d = discovery.delegation.as_ref()
                .context("No TUF delegation files found in build volume. A content_key is configured — run 'avocado build' first.")?;
            Some((
                &d.delegated_targets_json,
                &d.content_key_hex,
                &d.content_keyid,
            ))
        } else {
            None
        };

        // Phase B: Create runtime via API
        let (runtime, num_artifacts) = self
            .create_runtime_api(
                connect,
                version,
                build_id,
                manifest,
                &discovery
                    .artifacts
                    .iter()
                    .map(|a| ArtifactParam {
                        image_id: a.image_id.clone(),
                        size_bytes: a.size_bytes,
                        sha256: a.sha256.clone(),
                        part_size: PART_SIZE,
                        part_checksums: a.part_checksums.clone(),
                    })
                    .collect::<Vec<_>>(),
                delegation_refs,
            )
            .await?;

        if runtime.status == "draft" {
            self.handle_draft_status(connect, &runtime, num_artifacts)
                .await?;
            return Ok(());
        }

        // Phase C: Stream artifacts from Docker volume and upload with progress
        let completed_parts = self
            .upload_in_container(project_config, connect, &runtime.artifacts, &discovery)
            .await?;

        // Phase D: Complete and finalize
        self.complete_and_finalize(connect, &runtime, completed_parts, num_artifacts)
            .await
    }

    /// Create a runtime via the Connect API. Returns the runtime data and
    /// artifact count.
    async fn create_runtime_api(
        &self,
        connect: &ConnectClient,
        version: String,
        build_id: Option<String>,
        manifest: &serde_json::Value,
        artifacts: &[ArtifactParam],
        delegation: Option<(&String, &String, &String)>,
    ) -> Result<(client::RuntimeCreateData, usize)> {
        print_info(
            &format!("Creating runtime {version}..."),
            OutputLevel::Normal,
        );
        let num_artifacts = artifacts.len();
        let create_req = CreateRuntimeRequest {
            runtime: RuntimeParams {
                version,
                build_id,
                description: self.description.clone(),
                manifest: Some(manifest.clone()),
                artifacts: artifacts.to_vec(),
                delegated_targets_json: delegation.map(|(d, _, _)| d.clone()),
                content_key_hex: delegation.map(|(_, k, _)| k.clone()),
                content_keyid: delegation.map(|(_, _, kid)| kid.clone()),
            },
        };
        let runtime = connect
            .create_runtime(&self.org, &self.project, &create_req)
            .await?;
        Ok((runtime, num_artifacts))
    }

    /// Handle the case where the runtime is already in draft status (full dedup).
    async fn handle_draft_status(
        &self,
        connect: &ConnectClient,
        runtime: &client::RuntimeCreateData,
        num_artifacts: usize,
    ) -> Result<()> {
        print_success(
            &format!(
                "Runtime {} already up to date (status: draft, {} artifact(s))",
                runtime.version, num_artifacts
            ),
            OutputLevel::Normal,
        );

        if self.publish {
            print_info("Publishing runtime...", OutputLevel::Normal);
            let published = connect
                .publish_runtime(&self.org, &self.project, &runtime.id)
                .await?;
            print_success(
                &format!(
                    "Runtime {} published (status: {})",
                    published.version, published.status
                ),
                OutputLevel::Normal,
            );
        }

        if let Some(ref cohort_id) = self.deploy_cohort {
            super::deploy::deploy_after_upload(&super::deploy::DeployAfterUploadParams {
                client: connect,
                org: &self.org,
                project: &self.project,
                runtime_id: &runtime.id,
                runtime_version: &runtime.version,
                cohort_id,
                name: self.deploy_name.as_deref(),
                description: self.description.as_deref(),
                tags: &self.deploy_tags,
                activate: self.deploy_activate,
            })
            .await?;
        }

        Ok(())
    }

    /// Complete the upload and optionally publish/deploy.
    async fn complete_and_finalize(
        &self,
        connect: &ConnectClient,
        runtime: &client::RuntimeCreateData,
        completed_parts: Vec<BlobParts>,
        num_artifacts: usize,
    ) -> Result<()> {
        // If all blobs were already uploaded (e.g. retrying after a prior partial
        // upload that completed the blobs but not the runtime transition), skip
        // the complete call — there are no multipart uploads to finalize.
        if completed_parts.is_empty() {
            self.handle_draft_status(connect, runtime, num_artifacts)
                .await?;
            return Ok(());
        }

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
                result.version, result.status, num_artifacts
            ),
            OutputLevel::Normal,
        );

        if self.publish {
            print_info("Publishing runtime...", OutputLevel::Normal);
            let published = connect
                .publish_runtime(&self.org, &self.project, &runtime.id)
                .await?;
            print_success(
                &format!(
                    "Runtime {} published (status: {})",
                    published.version, published.status
                ),
                OutputLevel::Normal,
            );
        }

        if let Some(ref cohort_id) = self.deploy_cohort {
            super::deploy::deploy_after_upload(&super::deploy::DeployAfterUploadParams {
                client: connect,
                org: &self.org,
                project: &self.project,
                runtime_id: &runtime.id,
                runtime_version: &result.version,
                cohort_id,
                name: self.deploy_name.as_deref(),
                description: self.description.as_deref(),
                tags: &self.deploy_tags,
                activate: self.deploy_activate,
            })
            .await?;
        }

        Ok(())
    }

    /// Get artifact files on disk, either from --file or by exporting from Docker.
    /// Resolve --file to a directory of artifacts (used by the host path only).
    async fn get_artifacts_dir(&self) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
        let file_or_dir = self
            .file
            .as_ref()
            .context("get_artifacts_dir called without --file")?;
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

    /// Run a discovery script inside the container to read the manifest, compute
    /// artifact hashes, and collect TUF delegation info — all without copying
    /// files out of the Docker volume.
    async fn discover_in_container(&self, config: &Config) -> Result<ContainerDiscoveryResult> {
        let target = resolve_target_required(self.target.as_deref(), config)?;
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let container = SdkContainer::from_config(&self.config_path, config)?;

        print_info(
            "Discovering artifacts in build volume...",
            OutputLevel::Normal,
        );

        let script = generate_discovery_script(&self.runtime);
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: script,
            source_environment: true,
            no_bootstrap: true,
            ..Default::default()
        };

        let stdout = container
            .run_in_container_with_output(run_config)
            .await
            .context("Failed to run discovery script in container")?
            .context("Discovery script failed inside container. Run with --verbose for details.")?;

        let result: ContainerDiscoveryResult =
            serde_json::from_str(&stdout).with_context(|| {
                format!(
                    "Failed to parse discovery output as JSON (first 500 chars): {}",
                    &stdout[..stdout.len().min(500)]
                )
            })?;

        for info in &result.artifacts {
            print_info(
                &format!("  {} ({})", info.name, format_bytes(info.size_bytes)),
                OutputLevel::Normal,
            );
        }

        Ok(result)
    }

    /// Run the upload script inside the container, uploading artifact parts in
    /// parallel directly from the Docker volume to S3 via presigned URLs.
    /// Upload artifacts by streaming file bytes from the Docker volume through
    /// lightweight `cat` containers, with real-time indicatif progress bars.
    async fn upload_in_container(
        &self,
        config: &Config,
        connect: &ConnectClient,
        upload_specs: &[ArtifactUploadSpec],
        discovery: &ContainerDiscoveryResult,
    ) -> Result<Vec<BlobParts>> {
        use crate::utils::volume::VolumeManager;
        use std::process::Stdio;

        // Filter to artifacts that actually need uploading (non-empty parts)
        let to_upload: Vec<&ArtifactUploadSpec> = upload_specs
            .iter()
            .filter(|s| !s.parts.is_empty())
            .collect();

        if to_upload.is_empty() {
            print_info("All artifact(s) already uploaded.", OutputLevel::Normal);
            return Ok(Vec::new());
        }

        let skipped = upload_specs.len() - to_upload.len();
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

        // Resolve container infrastructure for streaming files
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?
            .to_string();
        let container = SdkContainer::from_config(&self.config_path, config)?;
        let volume_manager = VolumeManager::new(container.container_tool.clone(), false);
        let volume_state = volume_manager.get_or_create_volume(&container.cwd).await?;

        let multi = MultiProgress::new();
        let mut all_completed = Vec::new();

        for spec in &to_upload {
            let artifact = discovery
                .artifacts
                .iter()
                .find(|a| a.image_id == spec.image_id)
                .with_context(|| {
                    format!(
                        "API returned image_id '{}' not found in discovery",
                        spec.image_id
                    )
                })?;

            let pb = multi.add(ProgressBar::new(artifact.size_bytes));
            pb.set_style(
                ProgressStyle::with_template(
                    "  {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
                )?
                .progress_chars("#>-"),
            );
            pb.set_message(artifact.name.clone());

            // Convert container path (/opt/_avocado/...) to volume-relative path
            let volume_path = artifact
                .container_path
                .strip_prefix("/opt/_avocado/")
                .unwrap_or(&artifact.container_path);

            // Spawn a lightweight named container to stream the file via stdout
            let cat_container_name = format!("avocado-upload-{}", uuid::Uuid::new_v4());
            let mut cmd = tokio::process::Command::new(&container.container_tool);
            cmd.args([
                "run",
                "--rm",
                "--name",
                &cat_container_name,
                "-v",
                &format!("{}:/data:ro", volume_state.volume_name),
                &container_image,
                "cat",
                &format!("/data/{volume_path}"),
            ]);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::null());
            // Isolate from terminal SIGINT so Ctrl-C doesn't kill the docker
            // client before --rm cleanup can run.
            #[cfg(unix)]
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }

            let mut child = cmd
                .spawn()
                .context("Failed to spawn container for file streaming")?;
            let mut stdout = child
                .stdout
                .take()
                .context("Failed to capture container stdout")?;

            // Upload all parts, stopping the container on Ctrl-C or error.
            let container_tool = container.container_tool.clone();
            let container_name_ref = cat_container_name.clone();
            let upload_result: Result<Vec<CompletedPart>> = tokio::select! {
                result = async {
                let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
                let mut upload_handles: Vec<tokio::task::JoinHandle<Result<CompletedPart>>> =
                    Vec::new();

                for part in &spec.parts {
                    let offset = (part.part_number - 1) * PART_SIZE;
                    let remaining = artifact.size_bytes.saturating_sub(offset);
                    let chunk_size = std::cmp::min(remaining, PART_SIZE) as usize;

                    let mut buf = vec![0u8; chunk_size];
                    stdout.read_exact(&mut buf).await.with_context(|| {
                        format!(
                            "Failed to read part {} of '{}' from container",
                            part.part_number, artifact.name
                        )
                    })?;

                    let checksum = BASE64_STANDARD.encode(Sha256::digest(&buf));

                    // Verify against pre-computed checksum from discovery
                    if let Some(expected) = artifact.part_checksums.get((part.part_number - 1) as usize) {
                        if checksum != *expected {
                            anyhow::bail!(
                                "Part {} of '{}' checksum mismatch (data changed since discovery)",
                                part.part_number, artifact.name
                            );
                        }
                    }

                    let permit = sem
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?;

                    let url = part.upload_url.clone();
                    let part_num = part.part_number;
                    let name = artifact.name.clone();
                    let pb = pb.clone();
                    let connect = connect.clone();

                    let handle = tokio::spawn(async move {
                        let _permit = permit;
                        let cs = chunk_size as u64;

                        let mut etag = None;
                        for attempt in 0..3u32 {
                            if attempt > 0 {
                                pb.set_position(pb.position().saturating_sub(cs));
                            }
                            match connect
                                .upload_part_with_progress(&url, buf.clone(), &checksum, Some(&pb))
                                .await
                            {
                                Ok(e) => {
                                    etag = Some(e);
                                    break;
                                }
                                Err(_) if attempt < 2 => {
                                    let delay = std::time::Duration::from_millis(
                                        500 * (u64::from(attempt) + 1),
                                    );
                                    tokio::time::sleep(delay).await;
                                }
                                Err(e) => {
                                    anyhow::bail!(
                                        "Failed to upload part {} of '{}' after 3 attempts: {}",
                                        part_num,
                                        name,
                                        e
                                    );
                                }
                            }
                        }

                        Ok(CompletedPart {
                            part_number: part_num,
                            etag: etag.unwrap(),
                            checksum_sha256: checksum,
                        })
                    });
                    upload_handles.push(handle);
                }

                let mut completed_parts = Vec::new();
                for handle in upload_handles {
                    let part = handle.await.context("Upload task panicked")??;
                    completed_parts.push(part);
                }
                Ok(completed_parts)
                } => result,
                _ = tokio::signal::ctrl_c() => {
                    Err(anyhow::anyhow!("Interrupted by user"))
                }
            };

            // On error or interrupt, stop the container so --rm can clean it up
            let completed_parts = match upload_result {
                Ok(parts) => parts,
                Err(e) => {
                    let _ = tokio::process::Command::new(&container_tool)
                        .args(["stop", "-t", "2", &container_name_ref])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                    let _ = child.wait().await;
                    return Err(e);
                }
            };

            pb.finish_with_message(format!("{} (done)", artifact.name));

            // Ensure container exits cleanly
            let status = child.wait().await?;
            if !status.success() {
                anyhow::bail!(
                    "Container streaming failed for '{}' (exit code {:?})",
                    artifact.name,
                    status.code()
                );
            }

            all_completed.push(BlobParts {
                image_id: spec.image_id.clone(),
                parts: completed_parts,
            });
        }

        Ok(all_completed)
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

        let (sha256, part_checksums) = compute_sha256_with_parts(&path)
            .with_context(|| format!("Failed to hash {}", path.display()))?;

        artifacts.push(ArtifactInfo {
            image_id: image_id.to_string(),
            path,
            size_bytes,
            sha256,
            part_checksums,
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

        let (sha256, part_checksums) = compute_sha256_with_parts(&path)
            .with_context(|| format!("Failed to hash {}", path.display()))?;

        artifacts.push(ArtifactInfo {
            image_id: image_id.to_string(),
            path,
            size_bytes,
            sha256,
            part_checksums,
        });
    }

    if artifacts.is_empty() {
        anyhow::bail!("No artifacts found in manifest. Have you run 'avocado build'?");
    }

    Ok(artifacts)
}

/// Compute whole-file SHA-256 (hex) and per-part SHA-256 checksums (base64) in one pass.
/// Parts are PART_SIZE (50 MiB) aligned, matching the API's multipart part size.
fn compute_sha256_with_parts(path: &Path) -> Result<(String, Vec<String>)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut whole_hasher = Sha256::new();
    let mut part_checksums = Vec::new();
    let part_size = PART_SIZE as usize;

    loop {
        let mut buf = vec![0u8; part_size];
        let mut read = 0;
        while read < part_size {
            let n = file.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        if read == 0 {
            break;
        }
        let chunk = &buf[..read];
        whole_hasher.update(chunk);
        part_checksums.push(BASE64_STANDARD.encode(Sha256::digest(chunk)));
    }
    Ok((format!("{:x}", whole_hasher.finalize()), part_checksums))
}

// ---------------------------------------------------------------------------
// Chunked upload with progress bars
// ---------------------------------------------------------------------------

async fn upload_artifacts(
    connect: &ConnectClient,
    org: &str,
    project_id: &str,
    runtime_id: &str,
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

        // Current presigned URLs for this artifact — may be refreshed on expiry.
        let mut current_parts = spec.parts.clone();

        for part in &spec.parts {
            let offset = (part.part_number - 1) * PART_SIZE;
            let remaining = artifact.size_bytes.saturating_sub(offset);
            let chunk_size = std::cmp::min(remaining, PART_SIZE) as usize;

            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; chunk_size];
            file.read_exact(&mut buf).await?;

            let checksum = BASE64_STANDARD.encode(Sha256::digest(&buf));

            // Verify against pre-computed checksum from discovery
            if let Some(expected) = artifact.part_checksums.get((part.part_number - 1) as usize) {
                if checksum != *expected {
                    anyhow::bail!(
                        "Part {} of '{}' checksum mismatch (data changed since discovery)",
                        part.part_number,
                        spec.image_id
                    );
                }
            }

            // Retry up to 3 times for transient failures.
            // On URL expiry, refresh presigned URLs and retry immediately — no cap on refreshes
            // since fetching a new URL is safe (the S3 multipart upload_id does not expire).
            let mut attempt = 0u32;
            let etag: String = loop {
                // Resolve the current upload URL for this part number (may have been refreshed).
                let upload_url = current_parts
                    .iter()
                    .find(|p| p.part_number == part.part_number)
                    .map(|p| p.upload_url.clone())
                    .with_context(|| {
                        format!(
                            "Part {} missing from upload spec for {}",
                            part.part_number, spec.image_id
                        )
                    })?;

                match connect
                    .upload_part(&upload_url, buf.clone(), &checksum)
                    .await
                {
                    Ok(e) => break e,
                    Err(UploadPartError::UrlExpired { .. }) => {
                        eprintln!(
                            "  Presigned URL expired for part {} of {} — refreshing URLs...",
                            part.part_number, spec.image_id
                        );
                        current_parts = connect
                            .get_upload_urls(org, project_id, runtime_id, &spec.image_id)
                            .await
                            .with_context(|| {
                                format!(
                                    "Failed to refresh upload URLs for artifact '{}'",
                                    spec.image_id
                                )
                            })?;
                        // Don't increment attempt — expiry is not a transient failure.
                    }
                    Err(e) if attempt < 2 => {
                        attempt += 1;
                        let delay = std::time::Duration::from_millis(500 * u64::from(attempt));
                        tokio::time::sleep(delay).await;
                        eprintln!(
                            "  Retrying part {} of {} (attempt {}/3): {}",
                            part.part_number,
                            spec.image_id,
                            attempt + 1,
                            e
                        );
                    }
                    Err(e) => {
                        anyhow::bail!(
                            "Failed to upload part {} of '{}' after {} attempt(s): {}",
                            part.part_number,
                            spec.image_id,
                            attempt + 1,
                            e
                        );
                    }
                }
            };

            completed_parts.push(CompletedPart {
                part_number: part.part_number,
                etag,
                checksum_sha256: checksum,
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
// In-container discovery & upload (zero-copy path)
// ---------------------------------------------------------------------------

/// Generate a bash script that discovers artifacts inside the container and
/// outputs a JSON object (`ContainerDiscoveryResult`) to stdout.
fn generate_discovery_script(runtime: &str) -> String {
    format!(
        r#"set -euo pipefail

STAGING="${{AVOCADO_PREFIX}}/runtimes/{runtime}/var-staging/lib/avocado"
IMAGES="${{STAGING}}/images"

# Locate manifest.json
MANIFEST_PATH=""
if [ -f "${{STAGING}}/active/manifest.json" ]; then
  MANIFEST_PATH="${{STAGING}}/active/manifest.json"
else
  for d in "${{STAGING}}/runtimes"/*/; do
    if [ -f "${{d}}manifest.json" ]; then
      MANIFEST_PATH="${{d}}manifest.json"
      break
    fi
  done
fi

if [ -z "$MANIFEST_PATH" ]; then
  echo "manifest.json not found in ${{STAGING}}. Have you run 'avocado build'?" >&2
  exit 1
fi

MANIFEST=$(cat "$MANIFEST_PATH")

# Discover artifacts from manifest (extensions + optional os_bundle)
# Output tab-separated: name\timage_id
ARTIFACT_ENTRIES=$(echo "$MANIFEST" | jq -r '
  [(.extensions // [])[] | {{name: .name, image_id: .image_id}}]
  + (if .os_bundle then [{{name: "os-bundle", image_id: .os_bundle.image_id}}] else [] end)
  | .[] | "\(.name)\t\(.image_id)"')

ARTIFACTS="[]"
while IFS=$'\t' read -r NAME IMAGE_ID; do
  [ -z "$IMAGE_ID" ] && continue
  RAW_PATH="${{IMAGES}}/${{IMAGE_ID}}.raw"
  if [ ! -f "$RAW_PATH" ]; then
    echo "Artifact not found: $RAW_PATH ($NAME, image_id: $IMAGE_ID)" >&2
    exit 1
  fi
  SIZE=$(stat -c%s "$RAW_PATH")
  SHA=$(sha256sum "$RAW_PATH" | awk '{{print $1}}')

  # Compute per-part SHA-256 checksums (base64-encoded, 50 MiB parts)
  PART_SIZE=52428800
  PART_CHECKSUMS="[]"
  PART_INDEX=0
  while [ "$((PART_INDEX * PART_SIZE))" -lt "$SIZE" ]; do
    CHUNK_CS=$(dd if="$RAW_PATH" bs="$PART_SIZE" count=1 skip="$PART_INDEX" 2>/dev/null \
      | openssl dgst -sha256 -binary | openssl base64 -A)
    PART_CHECKSUMS=$(echo "$PART_CHECKSUMS" | jq --arg cs "$CHUNK_CS" '. + [$cs]')
    PART_INDEX=$((PART_INDEX + 1))
  done

  ARTIFACTS=$(echo "$ARTIFACTS" | jq \
    --arg id "$IMAGE_ID" \
    --arg name "$NAME" \
    --arg sz "$SIZE" \
    --arg sha "$SHA" \
    --arg p "$RAW_PATH" \
    --argjson pcs "$PART_CHECKSUMS" \
    '. + [{{"image_id":$id,"name":$name,"size_bytes":($sz|tonumber),"sha256":$sha,"part_checksums":$pcs,"container_path":$p}}]')
done <<< "$ARTIFACT_ENTRIES"

if [ "$(echo "$ARTIFACTS" | jq 'length')" -eq 0 ]; then
  echo "No artifacts found in manifest. Have you run 'avocado build'?" >&2
  exit 1
fi

# Read TUF delegation info (optional)
DELEGATION="null"
TARGETS_PATH="${{STAGING}}/tuf-staging/targets.json"
if [ -f "$TARGETS_PATH" ]; then
  ROLE_NAME=$(jq -r '.signed.delegations.roles[0].name' "$TARGETS_PATH")
  KEYID=$(jq -r '.signed.delegations.roles[0].keyids[0]' "$TARGETS_PATH")
  PUBKEY=$(jq -r ".signed.delegations.keys[\"$KEYID\"].keyval.public" "$TARGETS_PATH")
  DELEG_PATH="${{STAGING}}/tuf-staging/delegations/${{ROLE_NAME}}.json"
  if [ -f "$DELEG_PATH" ]; then
    DELEG_JSON=$(cat "$DELEG_PATH")
    DELEGATION=$(jq -n \
      --arg d "$DELEG_JSON" \
      --arg k "$PUBKEY" \
      --arg kid "$KEYID" \
      '{{"delegated_targets_json":$d,"content_key_hex":$k,"content_keyid":$kid}}')
  fi
fi

# Output the discovery result as a single JSON object
jq -n \
  --argjson manifest "$MANIFEST" \
  --argjson artifacts "$ARTIFACTS" \
  --argjson delegation "$DELEGATION" \
  '{{"manifest":$manifest,"artifacts":$artifacts,"delegation":$delegation}}'
"#,
        runtime = runtime,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_compute_sha256_with_parts_single_part() {
        let mut f = NamedTempFile::new().unwrap();
        let data = vec![0xABu8; 1024];
        f.write_all(&data).unwrap();
        f.flush().unwrap();

        let (hex_hash, parts) = compute_sha256_with_parts(f.path()).unwrap();

        // Single part for data smaller than PART_SIZE
        assert_eq!(parts.len(), 1);
        // Hex hash should be 64 chars
        assert_eq!(hex_hash.len(), 64);
        // Part checksum should be base64-encoded (44 chars for SHA-256)
        assert_eq!(parts[0].len(), 44);
    }

    #[test]
    fn test_compute_sha256_with_parts_multiple_parts() {
        let mut f = NamedTempFile::new().unwrap();
        // Write slightly more than one part (PART_SIZE + 1 byte)
        let data = vec![0xCDu8; PART_SIZE as usize + 1];
        f.write_all(&data).unwrap();
        f.flush().unwrap();

        let (hex_hash, parts) = compute_sha256_with_parts(f.path()).unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(hex_hash.len(), 64);
        // The two parts should have different checksums (different data lengths)
        assert_ne!(parts[0], parts[1]);
    }

    #[test]
    fn test_compute_sha256_with_parts_whole_file_hash_matches() {
        let mut f = NamedTempFile::new().unwrap();
        let data = b"hello world checksum test";
        f.write_all(data).unwrap();
        f.flush().unwrap();

        let (hex_hash, _parts) = compute_sha256_with_parts(f.path()).unwrap();

        // Verify against independently computed hash
        let expected = format!("{:x}", Sha256::digest(data));
        assert_eq!(hex_hash, expected);
    }

    #[test]
    fn test_compute_sha256_with_parts_part_checksum_matches_independent() {
        let mut f = NamedTempFile::new().unwrap();
        let data = vec![0x42u8; 512];
        f.write_all(&data).unwrap();
        f.flush().unwrap();

        let (_hex_hash, parts) = compute_sha256_with_parts(f.path()).unwrap();

        // Verify the part checksum matches an independently computed base64 SHA-256
        let expected = BASE64_STANDARD.encode(Sha256::digest(&data));
        assert_eq!(parts[0], expected);
    }
}
