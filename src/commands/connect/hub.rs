//! Connect side of the VM hub: upload provisionable bundles, and run a
//! provision job on a site VM's Rubicon (the CI entry point: records the
//! job's origin, waits, exits non-zero unless it passed).

use anyhow::{bail, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use crate::commands::connect::client::{self, ConnectClient};
use crate::commands::connect::upload::{compute_sha256_with_parts, PART_SIZE};
use crate::commands::provision_bundle::BundleMeta;
use crate::utils::output::{print_info, print_success, OutputLevel};

pub(crate) fn connect(profile: Option<&str>, org: Option<&str>) -> Result<(ConnectClient, String)> {
    let config = client::load_config()?
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
    let (_, p) = config.resolve_profile(profile, org)?;
    let org = org
        .map(String::from)
        .or_else(|| p.organization_id.clone())
        .context("no organization: pass --org or use an org-scoped profile")?;
    Ok((ConnectClient::from_profile(p)?, org))
}

pub struct BundleUploadCommand {
    /// A bundle directory from `avocado provision --pack`.
    pub file: PathBuf,
    pub name: Option<String>,
    pub org: Option<String>,
    pub profile: Option<String>,
}

impl BundleUploadCommand {
    pub async fn execute(&self) -> Result<()> {
        let (c, org) = connect(self.profile.as_deref(), self.org.as_deref())?;
        let name = self.name.clone().unwrap_or_else(|| {
            self.file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        upload_bundle(&c, &org, &self.file, &name, json!({}))
            .await
            .map(|_| ())
    }
}

/// Upload a bundle directory, sending only blobs Connect lacks. `links`
/// is merged into the create request (e.g. `{"runtime_id": …}`). Returns
/// the bundle id.
pub async fn upload_bundle(
    c: &ConnectClient,
    org: &str,
    dir: &std::path::Path,
    name: &str,
    links: Value,
) -> Result<String> {
    {
        let meta: BundleMeta = serde_json::from_slice(
            &std::fs::read(dir.join("bundle.json")).with_context(|| {
                format!(
                    "{} is not a bundle directory (make one with `avocado provision --pack`)",
                    dir.display()
                )
            })?,
        )?;
        let blobs = meta.blobs();
        let total: u64 = blobs.iter().map(|b| b.size).sum();

        // Content Connect already has (from any runtime or bundle upload in
        // this org) is referenced, not sent again.
        let missing: std::collections::HashSet<String> = c
            .json(
                Method::POST,
                &format!("/api/orgs/{org}/blobs/missing"),
                Some(&json!({"sha256": blobs.iter().map(|b| &b.sha256).collect::<Vec<_>>()})),
            )
            .await?["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let mut specs = Vec::new();
        for b in &blobs {
            let mut spec = json!({"sha256": b.sha256, "size_bytes": b.size});
            if missing.contains(&b.sha256) {
                let path = dir.join("blobs").join(&b.sha256);
                let (sha, parts) = compute_sha256_with_parts(&path)?;
                if sha != b.sha256 {
                    bail!(
                        "blob {} on disk does not match its name (sha256 {sha})",
                        b.sha256
                    );
                }
                spec["part_checksums"] = json!(parts);
            }
            specs.push(spec);
        }
        let mut bundle = json!({
            "name": name,
            "target": meta.target,
            "runtime": meta.runtime,
            "bundle_json": meta,
            "blobs": specs,
        });
        if let (Some(b), Some(l)) = (bundle.as_object_mut(), links.as_object()) {
            b.extend(l.clone());
        }
        let created = c
            .json(
                Method::POST,
                &format!("/api/orgs/{org}/provision-bundles"),
                Some(&json!({ "bundle": bundle })),
            )
            .await?;
        let id = created["data"]["id"]
            .as_str()
            .context("no bundle id in response")?
            .to_string();
        let uploads = created["data"]["uploads"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut completed = Vec::new();
        for u in &uploads {
            let sha = u["sha256"]
                .as_str()
                .context("upload without sha256")?
                .to_string();
            let path = dir.join("blobs").join(&sha);
            let size = std::fs::metadata(&path)?.len();
            let mut f = std::fs::File::open(&path)?;
            let mut parts: Vec<Value> = u["parts"].as_array().cloned().unwrap_or_default();
            let mut done = Vec::new();
            for i in 0..parts.len() {
                let n = parts[i]["part_number"].as_u64().context("part_number")?;
                let off = (n - 1) * PART_SIZE;
                let mut buf = vec![0u8; PART_SIZE.min(size - off) as usize];
                f.seek(SeekFrom::Start(off))?;
                f.read_exact(&mut buf)?;
                let sum = BASE64_STANDARD.encode(Sha256::digest(&buf));
                let mut tries = 0;
                let etag = loop {
                    let url = parts[i]["upload_url"]
                        .as_str()
                        .context("upload_url")?
                        .to_string();
                    match c.upload_part(&url, buf.clone(), &sum).await {
                        Ok(e) => break e,
                        Err(e) if tries < 3 => {
                            tries += 1;
                            eprintln!(
                                "  {}: part {n}: {e}; refreshing URLs and retrying",
                                &sha[..12]
                            );
                            let fresh = c
                                .json(
                                    Method::GET,
                                    &format!("/api/orgs/{org}/provision-bundles/{id}/upload-urls?sha256={sha}"),
                                    None,
                                )
                                .await?;
                            if let Some(p) = fresh["data"]["parts"].as_array() {
                                parts = p.clone();
                            }
                        }
                        Err(e) => bail!("blob {sha} part {n} failed: {e}"),
                    }
                };
                done.push(json!({"part_number": n, "etag": etag, "checksum_sha256": sum}));
            }
            print_info(
                &format!("  uploaded {} ({} MiB)", &sha[..12], size >> 20),
                OutputLevel::Normal,
            );
            completed.push(json!({"sha256": sha, "parts": done}));
        }
        c.json(
            Method::POST,
            &format!("/api/orgs/{org}/provision-bundles/{id}/complete"),
            Some(&json!({"blobs": completed})),
        )
        .await?;
        let up: u64 = blobs
            .iter()
            .filter(|b| missing.contains(&b.sha256))
            .map(|b| b.size)
            .sum();
        print_success(
            &format!(
                "Bundle '{name}' uploaded (id {id}): {} blobs, {} MiB total; {} MiB already in Connect, {} MiB sent",
                blobs.len(),
                total >> 20,
                (total - up) >> 20,
                up >> 20
            ),
            OutputLevel::Normal,
        );
        Ok(id)
    }
}

pub struct ProvisionJobCommand {
    pub vm: String,
    pub rubicon: String,
    pub bundle: Option<String>,
    pub runtime: Option<String>,
    pub profile: Option<String>,
    pub repository: Option<String>,
    pub pr_url: Option<String>,
    pub commit: Option<String>,
    pub run_url: Option<String>,
    pub wait: bool,
    pub org: Option<String>,
    pub profile_name: Option<String>,
}

/// PR number from a GitHub-style PR URL (`…/pull/123`).
pub fn pr_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/')
        .rsplit_once("/pull/")?
        .1
        .split('/')
        .next()?
        .parse()
        .ok()
}

impl ProvisionJobCommand {
    pub async fn execute(&self) -> Result<()> {
        let (c, org) = connect(self.profile_name.as_deref(), self.org.as_deref())?;
        let mut source = serde_json::Map::new();
        for (k, v) in [
            ("repository", &self.repository),
            ("pr_url", &self.pr_url),
            ("commit_sha", &self.commit),
            ("ci_run_url", &self.run_url),
        ] {
            if let Some(v) = v {
                source.insert(k.into(), json!(v));
            }
        }
        if let Some(n) = self.pr_url.as_deref().and_then(pr_number) {
            source.insert("pr_number".into(), json!(n));
        }
        let cmd = c
            .json(
                Method::POST,
                &format!("/api/orgs/{org}/vms/{}/commands", self.vm),
                Some(&json!({
                    "command": "provision",
                    "params": {
                        "address": self.rubicon,
                        "bundle_id": self.bundle,
                        "runtime_id": self.runtime,
                        "profile": self.profile,
                    },
                    "source": source,
                })),
            )
            .await?;
        let id = cmd["data"]["id"]
            .as_str()
            .context("no command id")?
            .to_string();
        print_success(&format!("Provision job {id} queued"), OutputLevel::Normal);
        if !self.wait {
            return Ok(());
        }
        let base = format!("/api/orgs/{org}/vms/{}/commands/{id}", self.vm);
        let mut after: i64 = -1;
        loop {
            let logs = c
                .json(Method::GET, &format!("{base}/logs?after_seq={after}"), None)
                .await?;
            for chunk in logs["data"].as_array().into_iter().flatten() {
                for l in chunk["lines"].as_array().into_iter().flatten() {
                    println!("{}", l.as_str().unwrap_or_default());
                }
            }
            if let Some(n) = logs["next_after_seq"].as_i64() {
                after = n;
            }
            let state = c.json(Method::GET, &base, None).await?;
            let st = state["data"]["state"].as_str().unwrap_or("");
            if matches!(st, "completed" | "failed" | "expired") {
                let result = &state["data"]["result"];
                for check in result["checks"].as_array().into_iter().flatten() {
                    println!(
                        "check {}: {}",
                        check["name"].as_str().unwrap_or("?"),
                        check["status"].as_str().unwrap_or("?")
                    );
                }
                let pass = st == "completed" && result["status"] == "pass";
                println!(
                    "job {id}: {st}, result {}",
                    result["status"].as_str().unwrap_or("none")
                );
                if !pass {
                    bail!("provision job did not pass");
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pr_number_from_url() {
        assert_eq!(super::pr_number("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(
            super::pr_number("https://github.com/o/r/pull/42/files"),
            Some(42)
        );
        assert_eq!(super::pr_number("https://github.com/o/r"), None);
    }
}
