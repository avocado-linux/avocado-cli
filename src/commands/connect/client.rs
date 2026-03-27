use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

/// Typed error for part uploads so callers can distinguish expiry from real failures.
#[derive(Debug, Error)]
pub enum UploadPartError {
    #[error("presigned URL expired (HTTP {status}): {body}")]
    UrlExpired { status: u16, body: String },
    #[error("part upload failed (HTTP {status}): {body}")]
    HttpError { status: u16, body: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Classify an S3 error response into the appropriate `UploadPartError` variant.
///
/// S3 expiry signals:
/// - HTTP 400 + `<Code>ExpiredToken</Code>`: STS credentials used to sign the URL have expired.
/// - HTTP 403 + `<Code>RequestExpired</Code>`: presigned URL's own `X-Amz-Expires` has passed.
/// - HTTP 403 + `<Message>` containing "Request has expired": same condition, alternate wording.
///
/// All other responses (including unrelated 403s like `SignatureDoesNotMatch`) are `HttpError`.
pub(crate) fn classify_upload_part_error(status: u16, body: &str) -> UploadPartError {
    let is_expired = (status == 400 && body.contains("<Code>ExpiredToken</Code>"))
        || (status == 403 && body.contains("<Code>RequestExpired</Code>"))
        || (status == 403 && body.contains("Request has expired"));

    if is_expired {
        UploadPartError::UrlExpired {
            status,
            body: body.to_string(),
        }
    } else {
        UploadPartError::HttpError {
            status,
            body: body.to_string(),
        }
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
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

    /// Resolve a profile: explicit name > org-based lookup > default profile.
    pub fn resolve_profile<'a>(
        &'a self,
        name: Option<&'a str>,
        org_id: Option<&str>,
    ) -> Result<(&'a str, &'a Profile)> {
        // Explicit --profile flag always wins.
        if let Some(profile_name) = name {
            return match self.profiles.get(profile_name) {
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
            };
        }

        // Org-based lookup: find a profile whose organization_id matches.
        if let Some(oid) = org_id {
            if let Some(found) = self.find_profile_by_org(oid) {
                return Ok(found);
            }
        }

        // Fall back to default profile.
        let profile_name = self.default_profile.as_str();
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

    /// Find a profile whose organization_id matches the given org ID.
    pub fn find_profile_by_org<'a>(&'a self, org_id: &str) -> Option<(&'a str, &'a Profile)> {
        self.profiles
            .iter()
            .find(|(_, p)| p.organization_id.as_deref() == Some(org_id))
            .map(|(name, p)| (name.as_str(), p))
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
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    pub name: String,
    pub organization_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MeFullResponse {
    pub user: MeResponse,
    pub organizations: Vec<OrgInfo>,
    pub token: Option<TokenInfo>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Deployment types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateDeploymentRequest {
    pub deployment: CreateDeploymentParams,
}

#[derive(Debug, Serialize)]
pub struct CreateDeploymentParams {
    pub name: String,
    pub cohort_id: String,
    pub runtime_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filter_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DeploymentInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub status: String,
    pub cohort_id: String,
    pub runtime_id: String,
    #[serde(default)]
    pub filter_tags: Option<Vec<String>>,
    #[serde(default)]
    pub is_targeted: bool,
}

#[derive(Debug, Serialize)]
pub struct UpdateDeploymentRequest {
    pub deployment: UpdateDeploymentParams,
}

#[derive(Debug, Serialize)]
pub struct UpdateDeploymentParams {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeListItem {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub build_id: Option<String>,
    #[serde(default)]
    pub display_version: Option<String>,
    pub status: String,
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

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
// Container interchange types (for in-container upload flow)
// ---------------------------------------------------------------------------

/// Phase A output: metadata collected inside the container via discovery script.
#[derive(Debug, Deserialize)]
pub struct ContainerDiscoveryResult {
    pub manifest: serde_json::Value,
    pub artifacts: Vec<ContainerArtifactInfo>,
    pub delegation: Option<ContainerDelegationInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ContainerArtifactInfo {
    pub image_id: String,
    /// Friendly display name (e.g. "avocado-ext-connect" or "os-bundle").
    pub name: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// Absolute path inside the container (e.g. /opt/_avocado/.../images/UUID.raw)
    pub container_path: String,
}

#[derive(Debug, Deserialize)]
pub struct ContainerDelegationInfo {
    pub delegated_targets_json: String,
    pub content_key_hex: String,
    pub content_keyid: String,
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

        let token = match data.get("token") {
            Some(t) if !t.is_null() => Some(serde_json::from_value::<TokenInfo>(t.clone())?),
            _ => None,
        };

        Ok(MeFullResponse {
            user,
            organizations,
            token,
        })
    }

    /// Create an org-scoped API token.  Returns (token_string, organization_id).
    pub async fn create_org_token(&self, org_id: &str, name: &str) -> Result<(String, String)> {
        let url = format!("{}/api/orgs/{}/api-tokens", self.api_url, org_id);

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "api_token": { "name": name } }))
            .send()
            .await
            .context("Failed to create org-scoped token")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create org-scoped token (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let token_str = body["data"]["token"]["token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no token in create-org-token response"))?
            .to_string();
        let token_org_id = body["data"]["token"]["organization_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no organization_id in create-org-token response"))?
            .to_string();

        Ok((token_str, token_org_id))
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
    /// Returns `UploadPartError::UrlExpired` when S3 responds with an expired-token error
    /// (HTTP 400 with `<Code>ExpiredToken</Code>` or HTTP 403), so callers can refresh
    /// presigned URLs and retry rather than treating it as a hard failure.
    pub async fn upload_part(
        &self,
        presigned_url: &str,
        body: Vec<u8>,
    ) -> Result<String, UploadPartError> {
        self.upload_part_with_progress(presigned_url, body, None).await
    }

    /// Upload a single part with optional real-time progress tracking.
    /// The progress bar is incremented as bytes are sent.
    pub async fn upload_part_with_progress(
        &self,
        presigned_url: &str,
        body: Vec<u8>,
        progress: Option<&indicatif::ProgressBar>,
    ) -> Result<String, UploadPartError> {
        let body_len = body.len();

        let res = if let Some(pb) = progress {
            // Wrap body in a streaming reader that reports progress
            let pb = pb.clone();
            let stream = futures_util::stream::unfold(
                (std::io::Cursor::new(body), pb, 0usize),
                |(mut cursor, pb, sent)| async move {
                    use std::io::Read;
                    let mut buf = vec![0u8; 64 * 1024]; // 64KB chunks for smooth progress
                    match cursor.read(&mut buf) {
                        Ok(0) => None,
                        Ok(n) => {
                            buf.truncate(n);
                            pb.inc(n as u64);
                            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), (cursor, pb, sent + n)))
                        }
                        Err(e) => Some((Err(e), (cursor, pb, sent))),
                    }
                },
            );
            self.http
                .put(presigned_url)
                .header("content-length", body_len)
                .body(reqwest::Body::wrap_stream(stream))
                .send()
                .await
                .map_err(|e| UploadPartError::Other(anyhow::anyhow!("Failed to upload part: {e}")))?
        } else {
            self.http
                .put(presigned_url)
                .body(body)
                .send()
                .await
                .map_err(|e| UploadPartError::Other(anyhow::anyhow!("Failed to upload part: {e}")))?
        };

        let status = res.status();
        if !status.is_success() {
            let status_u16 = status.as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(classify_upload_part_error(status_u16, &body));
        }

        let etag = res
            .headers()
            .get("etag")
            .ok_or_else(|| {
                UploadPartError::Other(anyhow::anyhow!("S3 response missing ETag header"))
            })?
            .to_str()
            .map_err(|e| UploadPartError::Other(anyhow::anyhow!("Invalid ETag header: {e}")))?
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

    /// Publish a draft runtime (draft → published).
    pub async fn publish_runtime(
        &self,
        org: &str,
        project_id: &str,
        runtime_id: &str,
    ) -> Result<RuntimeSummary> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/runtimes/{}",
            self.api_url, org, project_id, runtime_id
        );

        let body = serde_json::json!({
            "runtime": { "status": "published" }
        });

        let res = self
            .http
            .put(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await
            .context("Failed to publish runtime")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to publish runtime (HTTP {status}): {body}");
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

    // -----------------------------------------------------------------------
    // Deployments
    // -----------------------------------------------------------------------

    /// List runtimes for a project (for interactive picker).
    pub async fn list_runtimes(&self, org: &str, project: &str) -> Result<Vec<RuntimeListItem>> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/runtimes?status=published",
            self.api_url, org, project
        );

        let res = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to list runtimes")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list runtimes (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().unwrap_or(serde_json::json!([]));
        let runtimes: Vec<RuntimeListItem> = serde_json::from_value(data)?;
        Ok(runtimes)
    }

    /// Create a deployment.
    pub async fn create_deployment(
        &self,
        org: &str,
        project: &str,
        req: &CreateDeploymentRequest,
    ) -> Result<DeploymentInfo> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/deployments",
            self.api_url, org, project
        );

        let res = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(req)
            .send()
            .await
            .context("Failed to create deployment")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to create deployment (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let deployment: DeploymentInfo = serde_json::from_value(data)?;
        Ok(deployment)
    }

    /// Activate a deployment (transition from draft to active).
    pub async fn activate_deployment(
        &self,
        org: &str,
        project: &str,
        deployment_id: &str,
    ) -> Result<DeploymentInfo> {
        let url = format!(
            "{}/api/orgs/{}/projects/{}/deployments/{}",
            self.api_url, org, project, deployment_id
        );

        let req = UpdateDeploymentRequest {
            deployment: UpdateDeploymentParams {
                status: "active".to_string(),
            },
        };

        let res = self
            .http
            .put(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to activate deployment")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Failed to activate deployment (HTTP {status}): {body}");
        }

        let body: serde_json::Value = res.json().await?;
        let data = body.get("data").cloned().context("Response missing data")?;
        let deployment: DeploymentInfo = serde_json::from_value(data)?;
        Ok(deployment)
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

    // --- classify_upload_part_error ---

    #[test]
    fn test_classify_400_expired_token_is_url_expired() {
        let body = "<Error><Code>ExpiredToken</Code><Message>The provided token has expired.</Message></Error>";
        let err = classify_upload_part_error(400, body);
        assert!(
            matches!(err, UploadPartError::UrlExpired { status: 400, .. }),
            "expected UrlExpired, got {err}"
        );
    }

    #[test]
    fn test_classify_403_request_expired_code_is_url_expired() {
        let body =
            "<Error><Code>RequestExpired</Code><Message>Request has expired.</Message></Error>";
        let err = classify_upload_part_error(403, body);
        assert!(
            matches!(err, UploadPartError::UrlExpired { status: 403, .. }),
            "expected UrlExpired, got {err}"
        );
    }

    #[test]
    fn test_classify_403_request_has_expired_message_is_url_expired() {
        let body = "<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>";
        let err = classify_upload_part_error(403, body);
        assert!(
            matches!(err, UploadPartError::UrlExpired { status: 403, .. }),
            "expected UrlExpired, got {err}"
        );
    }

    #[test]
    fn test_classify_403_unrelated_access_denied_is_http_error() {
        let body = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
        let err = classify_upload_part_error(403, body);
        assert!(
            matches!(err, UploadPartError::HttpError { status: 403, .. }),
            "expected HttpError, got {err}"
        );
    }

    #[test]
    fn test_classify_403_signature_mismatch_is_http_error() {
        let body = "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided.</Message></Error>";
        let err = classify_upload_part_error(403, body);
        assert!(
            matches!(err, UploadPartError::HttpError { status: 403, .. }),
            "expected HttpError, got {err}"
        );
    }

    #[test]
    fn test_classify_400_without_expired_token_is_http_error() {
        let body = "<Error><Code>MalformedXML</Code><Message>Bad request.</Message></Error>";
        let err = classify_upload_part_error(400, body);
        assert!(
            matches!(err, UploadPartError::HttpError { status: 400, .. }),
            "expected HttpError, got {err}"
        );
    }

    #[test]
    fn test_classify_500_is_http_error() {
        let body = "Internal Server Error";
        let err = classify_upload_part_error(500, body);
        assert!(
            matches!(err, UploadPartError::HttpError { status: 500, .. }),
            "expected HttpError, got {err}"
        );
    }

    #[test]
    fn test_classify_preserves_body() {
        let body = "<Error><Code>ExpiredToken</Code></Error>";
        let err = classify_upload_part_error(400, body);
        if let UploadPartError::UrlExpired { body: b, .. } = err {
            assert_eq!(b, body);
        } else {
            panic!("expected UrlExpired");
        }
    }
}
