//! `avocado connect ext` — publish a packaged extension to the feed via avocado-connect.
//!
//! Flow (see avocado-connect docs/ext-publish.md): reserve a version + get a presigned
//! staging URL, PUT the RPM straight to storage, confirm (connect verifies + enqueues the
//! cluster ingest). Additive + safe: a taken version is rejected, never overwritten.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::commands::connect::client;
use crate::utils::output::{print_info, print_success, OutputLevel};

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .build()
        .context("Failed to build HTTP client")
}

/// Map connect API errors to plain, actionable messages.
fn api_error(status: u16, body: &str) -> anyhow::Error {
    let msg = match status {
        409 => {
            "that extension version is already taken — bump the version and republish".to_string()
        }
        422 => format!("the request was rejected as invalid: {body}"),
        401 => "not authenticated — run 'avocado connect auth login'".to_string(),
        403 => "not authorized — extension publish is super-admin only for now".to_string(),
        404 => "not found".to_string(),
        _ => format!("HTTP {status}: {body}"),
    };
    anyhow::anyhow!(msg)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse name/version/release/arch from an RPM filename
/// (`<name>-<version>-<release>.<arch>.rpm`). version/release must be dash-free.
fn parse_nevra(path: &Path) -> Result<(String, String, String, String)> {
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid RPM path")?;
    let stem = fname.strip_suffix(".rpm").context("not an .rpm file")?;
    let (nvr, arch) = stem
        .rsplit_once('.')
        .context("RPM filename missing .<arch>")?;
    let (nv, release) = nvr
        .rsplit_once('-')
        .context("RPM filename missing -<release>")?;
    let (name, version) = nv
        .rsplit_once('-')
        .context("RPM filename missing -<version>")?;
    Ok((
        name.to_string(),
        version.to_string(),
        release.to_string(),
        arch.to_string(),
    ))
}

pub struct ExtPublishCommand {
    pub config: String,
    pub org: Option<String>,
    pub profile: Option<String>,
    pub rpm: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub release: Option<String>,
    pub arch: Option<String>,
    pub target_release: String,
    pub target_channel: String,
    pub targets: String,
}

impl ExtPublishCommand {
    pub async fn execute(&self) -> Result<()> {
        // --org is optional for publish: omit it (and connect.org) to target the
        // platform (Peridio) org, which connect fills in server-side for super-admins.
        // When given (flag or connect.org), it publishes into that tenant org and
        // selects a matching auth profile.
        let org = self.org.clone().or_else(|| {
            std::path::Path::new(&self.config)
                .exists()
                .then(|| crate::utils::config::load_config(&self.config).ok())
                .flatten()
                .and_then(|c| c.connect)
                .and_then(|c| c.org)
        });
        let cfg = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = cfg.resolve_profile(self.profile.as_deref(), org.as_deref())?;
        let api = profile.api_url.trim_end_matches('/').to_string();
        let token = profile.token.clone();

        let path = Path::new(&self.rpm);
        let bytes = std::fs::read(path).with_context(|| format!("Failed to read {}", self.rpm))?;
        let size = bytes.len() as u64;
        let sha = sha256_hex(&bytes);

        let (pn, pv, pr, pa) = parse_nevra(path).unwrap_or_default();
        let name = self.name.clone().unwrap_or(pn);
        let version = self.version.clone().unwrap_or(pv);
        let release = self
            .release
            .clone()
            .unwrap_or(if pr.is_empty() { "r0".into() } else { pr });
        let arch = self
            .arch
            .clone()
            .unwrap_or(if pa.is_empty() { "noarch".into() } else { pa });
        if name.is_empty() || version.is_empty() {
            anyhow::bail!("could not determine extension name/version — pass --name and --version");
        }
        let machines: Vec<String> = self
            .targets
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let client = http_client()?;

        // 1. reserve version + get presigned staging URL (409 if taken)
        let org_label = org.as_deref().unwrap_or("platform");
        print_info(
            &format!("Publishing {name}-{version}-{release}.{arch} to {api} (org {org_label})..."),
            OutputLevel::Normal,
        );
        let res = client
            .post(format!("{api}/api/admin/extensions/publish"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "organization_id": org,
                "name": name,
                "version": version,
                "release": release,
                "arch": arch,
                "sha256": sha,
                "size_bytes": size,
                "target_release": self.target_release,
                "target_channel": self.target_channel,
                "target_machines": machines,
            }))
            .send()
            .await
            .context("publish request failed")?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(api_error(status, &body));
        }
        let data: serde_json::Value =
            serde_json::from_str(&body).context("failed to parse publish response")?;
        let id = data["data"]["id"]
            .as_str()
            .context("publish response missing version id")?
            .to_string();
        let upload_url = data["data"]["upload_url"]
            .as_str()
            .context("publish response missing upload_url")?
            .to_string();

        // 2. PUT the RPM straight to staging (bytes never pass through connect)
        print_info("Uploading package to staging...", OutputLevel::Normal);
        let put = client
            .put(&upload_url)
            .body(bytes)
            .send()
            .await
            .context("staging upload failed")?;
        if !put.status().is_success() {
            let s = put.status().as_u16();
            let b = put.text().await.unwrap_or_default();
            anyhow::bail!("staging upload failed (HTTP {s}): {b}");
        }

        // 3. confirm -> connect verifies bytes and enqueues the cluster ingest
        let conf = client
            .post(format!("{api}/api/admin/extensions/{id}/confirm"))
            .bearer_auth(&token)
            .send()
            .await
            .context("confirm request failed")?;
        let cs = conf.status().as_u16();
        let cb = conf.text().await.unwrap_or_default();
        if !(200..300).contains(&cs) {
            return Err(api_error(cs, &cb));
        }

        print_success(
            &format!("Published {name}-{version}; ingest queued."),
            OutputLevel::Normal,
        );
        print_info(
            &format!("Track it:  avocado connect ext status {id}"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

pub struct ExtStatusCommand {
    pub config: String,
    pub org: Option<String>,
    pub profile: Option<String>,
    pub id: String,
}

impl ExtStatusCommand {
    pub async fn execute(&self) -> Result<()> {
        let (api, token) = api_and_token(self.org.clone(), &self.config, self.profile.as_deref())?;
        let client = http_client()?;
        let res = client
            .get(format!("{api}/api/admin/extensions/{}", self.id))
            .bearer_auth(&token)
            .send()
            .await
            .context("status request failed")?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(api_error(status, &body));
        }
        let data: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        println!(
            "{}",
            serde_json::to_string_pretty(&data["data"]).unwrap_or(body)
        );
        Ok(())
    }
}

pub struct ExtListCommand {
    pub config: String,
    pub org: Option<String>,
    pub profile: Option<String>,
    pub name: Option<String>,
}

impl ExtListCommand {
    pub async fn execute(&self) -> Result<()> {
        let (api, token) = api_and_token(self.org.clone(), &self.config, self.profile.as_deref())?;
        let mut url = format!("{api}/api/admin/extensions");
        if let Some(n) = &self.name {
            url.push_str(&format!("?name={n}"));
        }
        let client = http_client()?;
        let res = client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("list request failed")?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(api_error(status, &body));
        }
        let data: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(items) = data["data"].as_array() {
            for v in items {
                println!(
                    "{:<28} {:<12} {:<10} {}",
                    v["package"].as_str().unwrap_or("?"),
                    v["version"].as_str().unwrap_or("?"),
                    v["status"].as_str().unwrap_or("?"),
                    v["nevra"].as_str().unwrap_or("")
                );
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&data).unwrap_or(body));
        }
        Ok(())
    }
}

fn api_and_token(
    org: Option<String>,
    config_path: &str,
    profile: Option<&str>,
) -> Result<(String, String)> {
    // --org is optional here too: fall back to connect.org, then to the
    // default/--profile auth, rather than hard-requiring an org.
    let org = org.or_else(|| {
        std::path::Path::new(config_path)
            .exists()
            .then(|| crate::utils::config::load_config(config_path).ok())
            .flatten()
            .and_then(|c| c.connect)
            .and_then(|c| c.org)
    });
    let cfg =
        client::load_config()?.context("Not logged in. Run 'avocado connect auth login' first.")?;
    let (_name, p) = cfg.resolve_profile(profile, org.as_deref())?;
    Ok((p.api_url.trim_end_matches('/').to_string(), p.token.clone()))
}
