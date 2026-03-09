use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const CONFIG_FILE: &str = "credentials.json";

// ---------------------------------------------------------------------------
// Config + Profile types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectConfig {
    pub default_profile: String,
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub api_url: String,
    pub token: String,
    pub user: ProfileUser,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileUser {
    pub email: String,
    pub name: String,
}

impl ConnectConfig {
    /// Create a fresh config with a single profile set as default.
    pub fn new_with_profile(name: &str, profile: Profile) -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(name.to_string(), profile);
        Self {
            default_profile: name.to_string(),
            profiles,
        }
    }

    /// Resolve a profile by explicit name or fall back to the default.
    pub fn resolve_profile<'a>(&'a self, name: Option<&'a str>) -> Result<(&'a str, &'a Profile)> {
        let profile_name = name.unwrap_or(&self.default_profile);
        match self.profiles.get(profile_name) {
            Some(p) => Ok((profile_name, p)),
            None => {
                let available: Vec<&str> = self.profiles.keys().map(|s| s.as_str()).collect();
                if available.is_empty() {
                    anyhow::bail!(
                        "No profiles configured. Run 'avocado connect auth login' to authenticate."
                    );
                }
                anyhow::bail!(
                    "Profile '{}' not found. Available profiles: {}",
                    profile_name,
                    available.join(", ")
                );
            }
        }
    }

    /// Insert or update a profile.
    pub fn upsert_profile(&mut self, name: &str, profile: Profile) {
        self.profiles.insert(name.to_string(), profile);
    }

    /// Remove a profile. Returns true if it existed.
    pub fn remove_profile(&mut self, name: &str) -> bool {
        self.profiles.remove(name).is_some()
    }
}

// ---------------------------------------------------------------------------
// API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MeResponse {
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrgInfo {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct MeFullResponse {
    pub user: MeResponse,
    pub organizations: Vec<OrgInfo>,
}

#[derive(Debug, Deserialize)]
pub struct CsrfResponse {
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// TUF server key response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ServerKeyResponse {
    pub public_key_hex: String,
    pub keyid: String,
}

#[derive(Debug, Deserialize)]
struct ServerKeyWrapper {
    data: ServerKeyResponse,
}

// ---------------------------------------------------------------------------
// Resource listing response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DeviceInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub identifier: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub cohort_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CohortInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ClaimTokenInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub cohort_id: Option<String>,
    #[serde(default)]
    pub max_uses: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Resource CRUD request types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateProjectRequest {
    pub project: CreateProjectParams,
}

#[derive(Debug, Serialize)]
pub struct CreateProjectParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateDeviceRequest {
    pub device: CreateDeviceParams,
}

#[derive(Debug, Serialize)]
pub struct CreateDeviceParams {
    pub name: String,
    pub identifier: String,
}

#[derive(Debug, Serialize)]
pub struct CreateCohortRequest {
    pub cohort: CreateCohortParams,
}

#[derive(Debug, Serialize)]
pub struct CreateCohortParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateClaimTokenRequest {
    pub claim_token: CreateClaimTokenParams,
}

#[derive(Debug, Serialize)]
pub struct CreateClaimTokenParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cohort_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i64>,
    /// Set to a far-future date for no expiration, or omit for default (24h).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Runtime upload request types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateRuntimeRequest {
    pub runtime: RuntimeParams,
}

#[derive(Debug, Serialize)]
pub struct RuntimeParams {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<serde_json::Value>,
    pub artifacts: Vec<ArtifactParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegated_targets_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_key_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_keyid: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactParam {
    pub image_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct CompleteRuntimeRequest {
    pub parts: Vec<BlobParts>,
}

#[derive(Debug, Serialize)]
pub struct BlobParts {
    pub image_id: String,
    pub parts: Vec<CompletedPart>,
}

#[derive(Debug, Serialize)]
pub struct CompletedPart {
    pub part_number: u64,
    pub etag: String,
}

// ---------------------------------------------------------------------------
// Runtime upload response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateRuntimeResponse {
    pub data: RuntimeCreateData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeCreateData {
    pub id: String,
    pub version: String,
    pub status: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactUploadSpec>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ArtifactUploadSpec {
    pub image_id: String,
    pub upload_id: Option<String>,
    #[serde(default)]
    pub parts: Vec<PartSpec>,
}

#[derive(Debug, Deserialize)]
pub struct PartSpec {
    pub part_number: u64,
    pub upload_url: String,
}

#[derive(Debug, Deserialize)]
struct CompleteRuntimeResponse {
    pub data: RuntimeSummary,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeSummary {
    pub id: String,
    pub version: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UploadUrlsResponse {
    data: UploadUrlsData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UploadUrlsData {
    image_id: String,
    parts: Vec<PartSpec>,
}

// ---------------------------------------------------------------------------
// Config file I/O
// ---------------------------------------------------------------------------

/// Get the avocado-connect config directory path.
fn get_config_dir() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "avocado-connect")
        .context("Could not determine config directory")?;
    Ok(proj_dirs.config_dir().to_path_buf())
}

fn get_config_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join(CONFIG_FILE))
}

/// Load the config from disk.
pub fn load_config() -> Result<Option<ConnectConfig>> {
    let path = get_config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    let config: ConnectConfig =
        serde_json::from_str(&contents).with_context(|| "Failed to parse config file")?;
    Ok(Some(config))
}

/// Save the full config to disk.
pub fn save_config(config: &ConnectConfig) -> Result<()> {
    let dir = get_config_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create directory: {}", dir.display()))?;
    let path = dir.join(CONFIG_FILE);
    let json = serde_json::to_string_pretty(config)?;
    fs::write(&path, json)
        .with_context(|| format!("Failed to write config: {}", path.display()))?;
    Ok(())
}

/// Delete the config file entirely.
pub fn delete_config_file() -> Result<bool> {
    let path = get_config_path()?;
    if path.exists() {
        fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// HTTP clients
// ---------------------------------------------------------------------------

/// HTTP client for the Connect platform (Bearer token auth).
pub struct ConnectClient {
    http: reqwest::Client,
    pub api_url: String,
    token: String,
}

impl ConnectClient {
    /// Create a client from a profile.
    pub fn from_profile(profile: &Profile) -> Result<Self> {
        let http = reqwest::ClientBuilder::new()
            .use_rustls_tls()
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            http,
            api_url: profile.api_url.clone(),
            token: profile.token.clone(),
        })
    }

    /// Verify auth by calling GET /api/me.
    #[allow(dead_code)]
    pub async fn get_me(&self) -> Result<MeResponse> {
        let res = self
            .http
            .get(format!("{}/api/me", self.api_url))
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to connect to API")?;

        if !res.status().is_success() {
            anyhow::bail!("Auth check failed (HTTP {})", res.status());
        }

        let body: serde_json::Value = res.json().await?;
        let user_val = body
            .get("data")
            .and_then(|d| d.get("user"))
            .cloned()
            .context("Response missing data.user")?;
        let me: MeResponse = serde_json::from_value(user_val)?;
        Ok(me)
    }

    /// Fetch full user info including organizations from GET /api/me.
    pub async fn get_me_full(&self) -> Result<MeFullResponse> {
        let res = self
            .http
            .get(format!("{}/api/me", self.api_url))
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to connect to API")?;

        if !res.status().is_success() {
            anyhow::bail!("Auth check failed (HTTP {})", res.status());
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").context("Response missing data")?;

        let user_val = data
            .get("user")
            .cloned()
            .context("Response missing data.user")?;
        let user: MeResponse = serde_json::from_value(user_val)?;

        let orgs_val = data
            .get("organizations")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));
        let organizations: Vec<OrgInfo> = serde_json::from_value(orgs_val)?;

        Ok(MeFullResponse {
            user,
            organizations,
        })
    }

    /// List projects for an organization.
    pub async fn list_projects(&self, org: &str) -> Result<Vec<ProjectInfo>> {
        let url = format!("{}/api/orgs/{}/projects", self.api_url, org);

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list projects")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list projects (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));
        let projects: Vec<ProjectInfo> = serde_json::from_value(data)?;
        Ok(projects)
    }

    /// List devices for an organization.
    pub async fn list_devices(&self, org: &str) -> Result<Vec<DeviceInfo>> {
        let url = format!("{}/api/orgs/{}/devices", self.api_url, org);

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list devices")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list devices (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));
        let devices: Vec<DeviceInfo> = serde_json::from_value(data)?;
        Ok(devices)
    }

    /// List cohorts for an organization's project.
    pub async fn list_cohorts(&self, org: &str, project: &str) -> Result<Vec<CohortInfo>> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/cohorts",
            self.api_url, org, project
        );

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list cohorts")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list cohorts (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));
        let cohorts: Vec<CohortInfo> = serde_json::from_value(data)?;
        Ok(cohorts)
    }

    /// List claim tokens for an organization.
    pub async fn list_claim_tokens(&self, org: &str) -> Result<Vec<ClaimTokenInfo>> {
        let url = format!("{}/api/orgs/{}/claim_tokens", self.api_url, org);

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list claim tokens")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list claim tokens (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(vec![]));
        let tokens: Vec<ClaimTokenInfo> = serde_json::from_value(data)?;
        Ok(tokens)
    }

    // -----------------------------------------------------------------------
    // Resource create/delete methods
    // -----------------------------------------------------------------------

    /// Create a project in an organization.
    pub async fn create_project(
        &self,
        org: &str,
        req: &CreateProjectRequest,
    ) -> Result<ProjectInfo> {
        let url = format!("{}/api/orgs/{}/projects", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create project")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create project (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let project: ProjectInfo = serde_json::from_value(data)?;
        Ok(project)
    }

    /// Delete a project.
    pub async fn delete_project(&self, org: &str, project_id: &str) -> Result<()> {
        let url = format!("{}/api/orgs/{}/projects/{}", self.api_url, org, project_id);

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to delete project")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete project (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Create a device in an organization.
    pub async fn create_device(&self, org: &str, req: &CreateDeviceRequest) -> Result<DeviceInfo> {
        let url = format!("{}/api/orgs/{}/devices", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create device")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create device (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let device: DeviceInfo = serde_json::from_value(data)?;
        Ok(device)
    }

    /// Delete a device.
    pub async fn delete_device(&self, org: &str, device_id: &str) -> Result<()> {
        let url = format!("{}/api/orgs/{}/devices/{}", self.api_url, org, device_id);

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to delete device")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete device (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Create a cohort in a project.
    pub async fn create_cohort(
        &self,
        org: &str,
        project: &str,
        req: &CreateCohortRequest,
    ) -> Result<CohortInfo> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/cohorts",
            self.api_url, org, project
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create cohort")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create cohort (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let cohort: CohortInfo = serde_json::from_value(data)?;
        Ok(cohort)
    }

    /// Delete a cohort.
    pub async fn delete_cohort(&self, org: &str, project: &str, cohort_id: &str) -> Result<()> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/cohorts/{}",
            self.api_url, org, project, cohort_id
        );

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to delete cohort")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete cohort (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Create a claim token.
    pub async fn create_claim_token(
        &self,
        org: &str,
        req: &CreateClaimTokenRequest,
    ) -> Result<ClaimTokenInfo> {
        let url = format!("{}/api/orgs/{}/claim_tokens", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create claim token")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create claim token (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let token: ClaimTokenInfo = serde_json::from_value(data)?;
        Ok(token)
    }

    /// Delete a claim token.
    pub async fn delete_claim_token(&self, org: &str, token_id: &str) -> Result<()> {
        let url = format!(
            "{}/api/orgs/{}/claim_tokens/{}",
            self.api_url, org, token_id
        );

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to delete claim token")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete claim token (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Step 1: Create a runtime record with artifact metadata.
    /// Returns upload specs with presigned URLs for each artifact.
    pub async fn create_runtime(
        &self,
        org: &str,
        project_id: &str,
        req: &CreateRuntimeRequest,
    ) -> Result<RuntimeCreateData> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/runtimes",
            self.api_url, org, project_id
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create runtime")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create runtime (HTTP {status}): {body}");
        }

        let resp: CreateRuntimeResponse = res.json().await?;
        Ok(resp.data)
    }

    /// Step 2: Upload a single part to a presigned S3 URL.
    /// Returns the ETag from the response headers.
    pub async fn upload_part(&self, presigned_url: &str, body: Vec<u8>) -> Result<String> {
        let res = self
            .http
            .put(presigned_url)
            .body(body)
            .send()
            .await
            .context("Failed to upload part")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Part upload failed (HTTP {status}): {body}");
        }

        let etag = res
            .headers()
            .get("etag")
            .context("S3 response missing ETag header")?
            .to_str()
            .context("Invalid ETag header")?
            .to_string();

        Ok(etag)
    }

    /// Step 3: Complete the runtime upload (finalize all multipart uploads).
    pub async fn complete_runtime(
        &self,
        org: &str,
        project_id: &str,
        runtime_id: &str,
        req: &CompleteRuntimeRequest,
    ) -> Result<RuntimeSummary> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/runtimes/{}/complete",
            self.api_url, org, project_id, runtime_id
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to complete runtime upload")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to complete runtime (HTTP {status}): {body}");
        }

        let resp: CompleteRuntimeResponse = res.json().await?;
        Ok(resp.data)
    }

    /// Re-fetch presigned URLs for an artifact (crash recovery).
    #[allow(dead_code)]
    pub async fn get_upload_urls(
        &self,
        org: &str,
        project_id: &str,
        runtime_id: &str,
        image_id: &str,
    ) -> Result<Vec<PartSpec>> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/runtimes/{}/artifacts/{}/upload-urls",
            self.api_url, org, project_id, runtime_id, image_id
        );

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to get upload URLs")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get upload URLs (HTTP {status}): {body}");
        }

        let resp: UploadUrlsResponse = res.json().await?;
        Ok(resp.data.parts)
    }

    /// Fetch the org's TUF server signing public key.
    pub async fn get_tuf_server_key(&self, org: &str) -> Result<ServerKeyResponse> {
        let url = format!("{}/api/orgs/{}/signing/server-key", self.api_url, org);

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to fetch server key")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to fetch server key (HTTP {status}): {body}");
        }

        let resp: ServerKeyWrapper = res.json().await?;
        Ok(resp.data)
    }
}

// ---------------------------------------------------------------------------
// Delegate key types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RegisterDelegateKeyRequest {
    pub public_key_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApproveDelegateKeyRequest {
    pub keyid: String,
}

#[derive(Debug, Deserialize)]
struct DelegateKeyWrapper {
    data: DelegateKeyData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct DelegateKeyData {
    pub keyid: String,
    pub public_key_hex: String,
    pub status: String,
    pub key_type: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub staged_at: Option<String>,
    #[serde(default)]
    pub activated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DelegateKeyListWrapper {
    data: Vec<DelegateKeyData>,
}

impl ConnectClient {
    /// Register a delegate key with the server.
    pub async fn register_delegate_key(
        &self,
        org: &str,
        req: &RegisterDelegateKeyRequest,
    ) -> Result<DelegateKeyData> {
        let url = format!("{}/api/orgs/{}/signing/keys", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to register delegate key")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to register delegate key (HTTP {status}): {body}");
        }

        let resp: DelegateKeyWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Approve a staged delegate key.
    pub async fn approve_delegate_key(
        &self,
        org: &str,
        req: &ApproveDelegateKeyRequest,
    ) -> Result<DelegateKeyData> {
        let url = format!("{}/api/orgs/{}/signing/keys/approve", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to approve delegate key")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to approve delegate key (HTTP {status}): {body}");
        }

        let resp: DelegateKeyWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// List delegate keys for an org.
    pub async fn list_delegate_keys(
        &self,
        org: &str,
        key_type: Option<&str>,
    ) -> Result<Vec<DelegateKeyData>> {
        let mut url = format!("{}/api/orgs/{}/signing/keys", self.api_url, org);
        if let Some(kt) = key_type {
            url.push_str(&format!("?key_type={kt}"));
        }

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list delegate keys")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list delegate keys (HTTP {status}): {body}");
        }

        let resp: DelegateKeyListWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Discard a staged delegate key by keyid.
    pub async fn discard_staged_key(&self, org: &str, keyid: &str) -> Result<()> {
        let url = format!(
            "{}/api/orgs/{}/signing/keys/staged?keyid={}",
            self.api_url, org, keyid
        );

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to discard staged key")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to discard staged key (HTTP {status}): {body}");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trust status types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TrustStatusData {
    pub current_root_version: i64,
    pub setup_complete: bool,
    pub root_rotated: bool,
    #[serde(default)]
    pub security_level: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub has_pending_promotion: bool,
    pub root_version_distribution: Vec<RootVersionBucket>,
    pub total_tracked_devices: i64,
    pub stale_device_count: i64,
}

// ---------------------------------------------------------------------------
// Root promotion / server key rotation response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProposeWrapper {
    data: ProposeData,
}

#[derive(Debug, Deserialize)]
pub struct ProposeData {
    pub pending_root_json: String,
    pub version: i64,
}

#[derive(Debug, Deserialize)]
struct CommitWrapper {
    data: CommitData,
}

#[derive(Debug, Deserialize)]
pub struct CommitData {
    pub version: i64,
    pub security_level: i64,
}

#[derive(Debug, Deserialize)]
struct RotateWrapper {
    data: RotateData,
}

#[derive(Debug, Deserialize)]
pub struct RotateData {
    pub version: i64,
}

#[derive(Debug, Deserialize)]
pub struct RootVersionBucket {
    pub root_version: i64,
    pub count: i64,
}

impl ConnectClient {
    /// Fetch fleet trust posture for an org.
    pub async fn get_trust_status(&self, org: &str) -> Result<TrustStatusData> {
        let url = format!("{}/api/orgs/{}/trust/status", self.api_url, org);

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to fetch trust status")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to fetch trust status (HTTP {status}): {body}");
        }

        let data: TrustStatusData = res.json().await?;
        Ok(data)
    }

    /// Propose root promotion (Level 1 → 2).
    pub async fn propose_promote_root(&self, org: &str) -> Result<ProposeData> {
        let url = format!(
            "{}/api/orgs/{}/trust/promote-root/propose",
            self.api_url, org
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to propose root promotion")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to propose root promotion (HTTP {status}): {body}");
        }

        let resp: ProposeWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Commit root promotion with the user's co-signature.
    pub async fn commit_promote_root(
        &self,
        org: &str,
        signature: &serde_json::Value,
    ) -> Result<CommitData> {
        let url = format!(
            "{}/api/orgs/{}/trust/promote-root/commit",
            self.api_url, org
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "signature": signature }))
            .send()
            .await
            .context("Failed to commit root promotion")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to commit root promotion (HTTP {status}): {body}");
        }

        let resp: CommitWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Cancel a pending root promotion.
    #[allow(dead_code)]
    pub async fn cancel_promote_root(&self, org: &str) -> Result<()> {
        let url = format!(
            "{}/api/orgs/{}/trust/promote-root/pending",
            self.api_url, org
        );

        let res = self
            .http
            .delete(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to cancel root promotion")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to cancel root promotion (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Rotate server signing key at Level 0/1 (no user action needed).
    pub async fn rotate_server_key(&self, org: &str) -> Result<RotateData> {
        let url = format!("{}/api/orgs/{}/trust/rotate-server-key", self.api_url, org);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to rotate server key")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to rotate server key (HTTP {status}): {body}");
        }

        let resp: RotateWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Propose server key rotation at Level 2 (requires user co-signing).
    pub async fn propose_rotate_server_key(&self, org: &str) -> Result<ProposeData> {
        let url = format!(
            "{}/api/orgs/{}/trust/rotate-server-key/propose",
            self.api_url, org
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to propose server key rotation")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to propose server key rotation (HTTP {status}): {body}");
        }

        let resp: ProposeWrapper = res.json().await?;
        Ok(resp.data)
    }

    /// Commit Level 2 server key rotation with the user's signature.
    pub async fn commit_rotate_server_key(
        &self,
        org: &str,
        signature: &serde_json::Value,
    ) -> Result<CommitData> {
        let url = format!(
            "{}/api/orgs/{}/trust/rotate-server-key/commit",
            self.api_url, org
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "signature": signature }))
            .send()
            .await
            .context("Failed to commit server key rotation")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to commit server key rotation (HTTP {status}): {body}");
        }

        let resp: CommitWrapper = res.json().await?;
        Ok(resp.data)
    }
}

/// Session-based client for login flow (cookie-based, not Bearer).
pub struct LoginClient {
    http: reqwest::Client,
    api_url: String,
}

impl LoginClient {
    pub fn new(api_url: &str) -> Result<Self> {
        let http = reqwest::ClientBuilder::new()
            .use_rustls_tls()
            .cookie_store(true)
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            http,
            api_url: api_url.to_string(),
        })
    }

    /// Get a CSRF token (also sets the session cookie).
    async fn get_csrf_token(&self) -> Result<String> {
        let res = self
            .http
            .get(format!("{}/auth/csrf-token", self.api_url))
            .send()
            .await
            .context("Failed to fetch CSRF token")?;

        if !res.status().is_success() {
            anyhow::bail!("Failed to get CSRF token (HTTP {})", res.status());
        }

        let body: CsrfResponse = res.json().await?;
        Ok(body.csrf_token)
    }

    /// Login with email + password. Returns session cookie (managed by cookie jar).
    pub async fn login(&self, email: &str, password: &str) -> Result<()> {
        let csrf = self.get_csrf_token().await?;

        let res = self
            .http
            .post(format!("{}/auth/login", self.api_url))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({
                "email": email,
                "password": password
            }))
            .send()
            .await
            .context("Failed to login")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Login failed (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Get current user info (requires active session).
    pub async fn get_me(&self) -> Result<MeResponse> {
        let res = self
            .http
            .get(format!("{}/api/me", self.api_url))
            .send()
            .await
            .context("Failed to fetch user info")?;

        if !res.status().is_success() {
            anyhow::bail!("Failed to get user info (HTTP {})", res.status());
        }

        let body: serde_json::Value = res.json().await?;
        let user_val = body
            .get("data")
            .and_then(|d| d.get("user"))
            .cloned()
            .context("Response missing data.user")?;
        let me: MeResponse = serde_json::from_value(user_val)?;
        Ok(me)
    }

    /// Create a personal API token (requires active session).
    pub async fn create_api_token(&self, name: &str) -> Result<String> {
        let csrf = self.get_csrf_token().await?;

        let res = self
            .http
            .post(format!("{}/api/me/api-tokens", self.api_url))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .context("Failed to create API token")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create API token (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let raw_token = body
            .get("data")
            .and_then(|d| d.get("token"))
            .and_then(|t| t.get("token"))
            .and_then(|v| v.as_str())
            .context("Response missing data.token.token field")?
            .to_string();

        Ok(raw_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_trust_status_data_deserializes_all_fields() {
        let json_val = json!({
            "current_root_version": 3,
            "setup_complete": true,
            "root_rotated": true,
            "security_level": 2,
            "has_pending_promotion": false,
            "root_version_distribution": [
                {"root_version": 1, "count": 5},
                {"root_version": 2, "count": 10}
            ],
            "total_tracked_devices": 15,
            "stale_device_count": 2
        });

        let data: TrustStatusData = serde_json::from_value(json_val).unwrap();
        assert_eq!(data.current_root_version, 3);
        assert!(data.setup_complete);
        assert!(data.root_rotated);
        assert_eq!(data.security_level, 2);
        assert!(!data.has_pending_promotion);
        assert_eq!(data.root_version_distribution.len(), 2);
        assert_eq!(data.root_version_distribution[0].root_version, 1);
        assert_eq!(data.root_version_distribution[0].count, 5);
        assert_eq!(data.total_tracked_devices, 15);
        assert_eq!(data.stale_device_count, 2);
    }

    #[test]
    fn test_trust_status_data_defaults_for_missing_optional_fields() {
        let json_val = json!({
            "current_root_version": 0,
            "setup_complete": false,
            "root_rotated": false,
            "root_version_distribution": [],
            "total_tracked_devices": 0,
            "stale_device_count": 0
        });

        let data: TrustStatusData = serde_json::from_value(json_val).unwrap();
        assert_eq!(data.security_level, 0);
        assert!(!data.has_pending_promotion);
    }

    #[test]
    fn test_propose_data_deserializes() {
        let json_val = json!({
            "data": {
                "pending_root_json": "{\"signed\":{\"_type\":\"root\"}}",
                "version": 2
            }
        });

        let wrapper: ProposeWrapper = serde_json::from_value(json_val).unwrap();
        assert_eq!(wrapper.data.version, 2);
        assert!(!wrapper.data.pending_root_json.is_empty());
    }

    #[test]
    fn test_commit_data_deserializes() {
        let json_val = json!({
            "data": {
                "version": 3,
                "security_level": 2
            }
        });

        let wrapper: CommitWrapper = serde_json::from_value(json_val).unwrap();
        assert_eq!(wrapper.data.version, 3);
        assert_eq!(wrapper.data.security_level, 2);
    }

    #[test]
    fn test_rotate_data_deserializes() {
        let json_val = json!({
            "data": {
                "version": 4
            }
        });

        let wrapper: RotateWrapper = serde_json::from_value(json_val).unwrap();
        assert_eq!(wrapper.data.version, 4);
    }
}
