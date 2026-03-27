use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::commands::connect::client::{
    self, ArtifactParam, ArtifactUploadSpec, BlobParts, CompleteRuntimeRequest, CompletedPart,
    ConnectClient, ContainerDiscoveryResult, ContainerUploadArtifact, ContainerUploadPart,
    ContainerUploadResult, ContainerUploadSpec, CreateRuntimeRequest,
    RuntimeParams, UploadPartError,
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

        let (runtime, num_artifacts) = self.create_runtime_api(
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
                })
                .collect::<Vec<_>>(),
            delegation.as_ref().map(|d| (&d.delegated_targets_json, &d.content_key_hex, &d.content_keyid)),
        ).await?;

        if runtime.status == "draft" {
            self.handle_draft_status(connect, &runtime, num_artifacts).await?;
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

        self.complete_and_finalize(connect, &runtime, completed_parts, num_artifacts).await
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
            Some((&d.delegated_targets_json, &d.content_key_hex, &d.content_keyid))
        } else {
            None
        };

        // Phase B: Create runtime via API
        let (runtime, num_artifacts) = self.create_runtime_api(
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
                })
                .collect::<Vec<_>>(),
            delegation_refs,
        ).await?;

        if runtime.status == "draft" {
            self.handle_draft_status(connect, &runtime, num_artifacts).await?;
            return Ok(());
        }

        // Phase C: Upload artifacts in parallel inside the container
        let completed_parts = self
            .upload_in_container(project_config, &runtime.artifacts, &discovery)
            .await?;

        // Phase D: Complete and finalize
        self.complete_and_finalize(connect, &runtime, completed_parts, num_artifacts).await
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
            self.handle_draft_status(connect, runtime, num_artifacts).await?;
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
    async fn get_artifacts_dir(
        &self,
    ) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
        let file_or_dir = self.file.as_ref()
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
    async fn discover_in_container(
        &self,
        config: &Config,
    ) -> Result<ContainerDiscoveryResult> {
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

        let result: ContainerDiscoveryResult = serde_json::from_str(&stdout)
            .with_context(|| {
                format!(
                    "Failed to parse discovery output as JSON (first 500 chars): {}",
                    &stdout[..stdout.len().min(500)]
                )
            })?;

        for info in &result.artifacts {
            print_info(
                &format!("  {} ({})", info.image_id, format_bytes(info.size_bytes)),
                OutputLevel::Normal,
            );
        }

        Ok(result)
    }

    /// Run the upload script inside the container, uploading artifact parts in
    /// parallel directly from the Docker volume to S3 via presigned URLs.
    async fn upload_in_container(
        &self,
        config: &Config,
        upload_specs: &[ArtifactUploadSpec],
        discovery: &ContainerDiscoveryResult,
    ) -> Result<Vec<BlobParts>> {
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

        // Build the upload spec JSON, mapping API upload specs to container paths
        let spec = ContainerUploadSpec {
            concurrency: 8,
            part_size: PART_SIZE,
            artifacts: to_upload
                .iter()
                .map(|api_spec| {
                    let container_path = discovery
                        .artifacts
                        .iter()
                        .find(|a| a.image_id == api_spec.image_id)
                        .map(|a| a.container_path.clone())
                        .with_context(|| {
                            format!(
                                "API returned image_id '{}' not found in discovery",
                                api_spec.image_id
                            )
                        })?;
                    let size_bytes = discovery
                        .artifacts
                        .iter()
                        .find(|a| a.image_id == api_spec.image_id)
                        .map(|a| a.size_bytes)
                        .unwrap_or(0);
                    Ok(ContainerUploadArtifact {
                        image_id: api_spec.image_id.clone(),
                        container_path,
                        size_bytes,
                        parts: api_spec
                            .parts
                            .iter()
                            .map(|p| ContainerUploadPart {
                                part_number: p.part_number,
                                upload_url: p.upload_url.clone(),
                            })
                            .collect(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };

        // Write spec to host-visible bind mount
        let config_dir = PathBuf::from(&self.config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let config_dir = if config_dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            config_dir
        };
        let spec_path = config_dir.join(".connect-upload-spec.json");
        let result_path = config_dir.join(".connect-upload-result.json");

        // Cleanup helper: remove temp files when done (on success or error)
        struct Cleanup {
            paths: Vec<PathBuf>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for p in &self.paths {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        let _cleanup = Cleanup {
            paths: vec![spec_path.clone(), result_path.clone()],
        };

        let spec_json = serde_json::to_string(&spec)
            .context("Failed to serialize upload spec")?;
        std::fs::write(&spec_path, &spec_json)
            .with_context(|| format!("Failed to write upload spec to {}", spec_path.display()))?;

        // Run upload script inside container (streaming mode for progress)
        let target = resolve_target_required(self.target.as_deref(), config)?;
        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;
        let container = SdkContainer::from_config(&self.config_path, config)?;

        let upload_script = generate_upload_script();
        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: upload_script,
            source_environment: true,
            no_bootstrap: true,
            ..Default::default()
        };

        let success = container
            .run_in_container(run_config)
            .await
            .context("Failed to run upload script in container")?;

        if !success {
            anyhow::bail!(
                "Upload failed inside container. Run with --verbose for details."
            );
        }

        // Read and parse the result JSON
        let result_json = std::fs::read_to_string(&result_path)
            .with_context(|| {
                format!(
                    "Upload script succeeded but result file not found at {}",
                    result_path.display()
                )
            })?;

        let result: ContainerUploadResult = serde_json::from_str(&result_json)
            .context("Failed to parse upload result JSON")?;

        // Convert to BlobParts for the complete API
        Ok(result
            .completed
            .into_iter()
            .map(|a| BlobParts {
                image_id: a.image_id,
                parts: a.parts,
            })
            .collect())
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

                match connect.upload_part(&upload_url, buf.clone()).await {
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
IMAGE_IDS=$(echo "$MANIFEST" | jq -r '
  [(.extensions // [])[] .image_id, (.os_bundle // empty) .image_id]
  | .[]')

ARTIFACTS="[]"
for IMAGE_ID in $IMAGE_IDS; do
  RAW_PATH="${{IMAGES}}/${{IMAGE_ID}}.raw"
  if [ ! -f "$RAW_PATH" ]; then
    echo "Artifact not found: $RAW_PATH (image_id: $IMAGE_ID)" >&2
    exit 1
  fi
  SIZE=$(stat -c%s "$RAW_PATH")
  SHA=$(sha256sum "$RAW_PATH" | awk '{{print $1}}')
  ARTIFACTS=$(echo "$ARTIFACTS" | jq \
    --arg id "$IMAGE_ID" \
    --arg sz "$SIZE" \
    --arg sha "$SHA" \
    --arg p "$RAW_PATH" \
    '. + [{{"image_id":$id,"size_bytes":($sz|tonumber),"sha256":$sha,"container_path":$p}}]')
done

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

/// Generate a bash script that uploads artifact parts in parallel from inside
/// the container using curl, reading the upload spec from a JSON file.
fn generate_upload_script() -> String {
    r#"set -euo pipefail

SPEC=$(cat /opt/src/.connect-upload-spec.json)
CONCURRENCY=$(echo "$SPEC" | jq -r '.concurrency')
PART_SIZE=$(echo "$SPEC" | jq -r '.part_size')
RESULT_DIR=$(mktemp -d)
trap 'rm -rf "$RESULT_DIR"' EXIT
RUNNING=0
FAILURES=0
DONE_PARTS=0

# ── Helpers ──────────────────────────────────────────────────────────────────

format_bytes() {
  echo "$1" | awk '{
    if ($1 >= 1073741824) printf "%.1f GB", $1/1073741824
    else if ($1 >= 1048576) printf "%.1f MB", $1/1048576
    else if ($1 >= 1024) printf "%.1f KB", $1/1024
    else printf "%d B", $1
  }'
}

show_progress() {
  local done_bytes=$(( DONE_PARTS * PART_SIZE ))
  [ "$done_bytes" -gt "$TOTAL_BYTES" ] && done_bytes=$TOTAL_BYTES
  local pct=0
  [ "$TOTAL_BYTES" -gt 0 ] && pct=$(( done_bytes * 100 / TOTAL_BYTES ))
  local bar_w=30
  local filled=$(( pct * bar_w / 100 ))
  local empty=$(( bar_w - filled ))
  local bar_fill=$(printf '%*s' "$filled" '' | tr ' ' '#')
  local bar_empty=$(printf '%*s' "$empty" '' | tr ' ' '-')
  local done_str=$(format_bytes $done_bytes)
  local total_str=$(format_bytes $TOTAL_BYTES)
  printf "\r  [%s] %s / %s (%d%%)" "$bar_fill$bar_empty" "$done_str" "$total_str" "$pct" >&2
}

# ── Upload worker ────────────────────────────────────────────────────────────

upload_part() {
  local image_id=$1 raw_path=$2 file_size=$3
  local part_number=$4 upload_url=$5
  local result_file="${RESULT_DIR}/${image_id}_${part_number}.json"

  local offset=$(( (part_number - 1) * PART_SIZE ))
  local remaining=$(( file_size - offset ))
  local chunk=$(( remaining < PART_SIZE ? remaining : PART_SIZE ))

  local etag="" attempt
  for attempt in 1 2 3; do
    local headers_file
    headers_file=$(mktemp)
    local http_code
    http_code=$(dd if="$raw_path" bs=4096 skip=$((offset / 4096)) \
                   count=$(( (chunk + 4095) / 4096 )) 2>/dev/null \
      | head -c "$chunk" \
      | curl -s -X PUT "$upload_url" \
          -H "Content-Type: application/octet-stream" \
          --data-binary @- \
          -D "$headers_file" \
          -o /dev/null \
          -w '%{http_code}')

    if [ "$http_code" -ge 200 ] && [ "$http_code" -lt 300 ]; then
      etag=$(grep -i '^etag:' "$headers_file" | tr -d '\r' | awk '{print $2}')
      rm -f "$headers_file"
      break
    fi
    rm -f "$headers_file"
    [ "$attempt" -lt 3 ] && sleep $((attempt * 2))
  done

  if [ -z "$etag" ]; then
    echo '{"error":true}' > "$result_file"
    return 1
  fi

  jq -n --argjson pn "$part_number" --arg et "$etag" \
    '{"part_number":$pn,"etag":$et}' > "$result_file"
}

# ── Pre-calculate totals ────────────────────────────────────────────────────

ARTIFACT_COUNT=$(echo "$SPEC" | jq '.artifacts | length')
TOTAL_PARTS=0
TOTAL_BYTES=0
for i in $(seq 0 $((ARTIFACT_COUNT - 1))); do
  FILE_SIZE=$(echo "$SPEC" | jq -r ".artifacts[$i].size_bytes")
  PART_COUNT=$(echo "$SPEC" | jq ".artifacts[$i].parts | length")
  TOTAL_PARTS=$((TOTAL_PARTS + PART_COUNT))
  TOTAL_BYTES=$((TOTAL_BYTES + FILE_SIZE))
done

show_progress

# ── Dispatch all parts with flat concurrency pool ───────────────────────────

for i in $(seq 0 $((ARTIFACT_COUNT - 1))); do
  IMAGE_ID=$(echo "$SPEC" | jq -r ".artifacts[$i].image_id")
  RAW_PATH=$(echo "$SPEC" | jq -r ".artifacts[$i].container_path")
  FILE_SIZE=$(echo "$SPEC" | jq -r ".artifacts[$i].size_bytes")
  PART_COUNT=$(echo "$SPEC" | jq ".artifacts[$i].parts | length")

  for j in $(seq 0 $((PART_COUNT - 1))); do
    PART_NUM=$(echo "$SPEC" | jq -r ".artifacts[$i].parts[$j].part_number")
    URL=$(echo "$SPEC" | jq -r ".artifacts[$i].parts[$j].upload_url")

    upload_part "$IMAGE_ID" "$RAW_PATH" "$FILE_SIZE" "$PART_NUM" "$URL" &
    RUNNING=$((RUNNING + 1))

    if [ "$RUNNING" -ge "$CONCURRENCY" ]; then
      wait -n || FAILURES=$((FAILURES + 1))
      RUNNING=$((RUNNING - 1))
      DONE_PARTS=$((DONE_PARTS + 1))
      show_progress
    fi
  done
done

# Drain remaining background jobs
while [ "$RUNNING" -gt 0 ]; do
  wait -n || FAILURES=$((FAILURES + 1))
  RUNNING=$((RUNNING - 1))
  DONE_PARTS=$((DONE_PARTS + 1))
  show_progress
done

# Final newline after progress bar
printf "\n" >&2

if [ "$FAILURES" -gt 0 ]; then
  echo "ERROR: ${FAILURES} part upload(s) failed" >&2
  exit 1
fi

# ── Collect results ─────────────────────────────────────────────────────────

RESULT='{"completed":[]}'
for i in $(seq 0 $((ARTIFACT_COUNT - 1))); do
  IMAGE_ID=$(echo "$SPEC" | jq -r ".artifacts[$i].image_id")
  PART_COUNT=$(echo "$SPEC" | jq ".artifacts[$i].parts | length")
  PARTS="[]"
  for j in $(seq 0 $((PART_COUNT - 1))); do
    PART_NUM=$(echo "$SPEC" | jq -r ".artifacts[$i].parts[$j].part_number")
    PART_JSON=$(cat "${RESULT_DIR}/${IMAGE_ID}_${PART_NUM}.json")
    PARTS=$(echo "$PARTS" | jq --argjson p "$PART_JSON" '. + [$p]')
  done
  RESULT=$(echo "$RESULT" | jq --arg id "$IMAGE_ID" --argjson p "$PARTS" \
    '.completed += [{"image_id":$id,"parts":$p}]')
done

echo "$RESULT" > /opt/src/.connect-upload-result.json
"#
    .to_string()
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
