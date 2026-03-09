use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::utils::update_signing::TufSigner;

/// Fixed namespace UUID for generating content-addressable image IDs.
/// image_id = UUIDv5(AVOCADO_IMAGE_NAMESPACE, sha256_hex_of_image_content)
pub const AVOCADO_IMAGE_NAMESPACE: Uuid = uuid::uuid!("7488fa35-6390-425b-bbbf-b156cfe1eed2");

/// Derive a deterministic UUIDv5 image identifier from an image's SHA-256 hex digest.
#[cfg(test)]
pub fn compute_image_id(sha256_hex: &str) -> String {
    Uuid::new_v5(&AVOCADO_IMAGE_NAMESPACE, sha256_hex.as_bytes()).to_string()
}

/// Information about a single target file in the TUF repository.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetFileInfo {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// The generated TUF metadata files (all signed JSON strings).
pub struct RepoMetadata {
    pub targets_json: String,
    pub snapshot_json: String,
    pub timestamp_json: String,
    /// Delegated-targets metadata for `delegations/runtime-<uuid>.json`
    pub delegated_targets_json: String,
}

/// Level 1 (content-only) metadata: delegated-targets signed by content key,
/// plus an unsigned targets.json carrier with delegation info for upload to read.
pub struct ContentOnlyMetadata {
    /// Unsigned targets.json with delegation keys/roles (read by upload to extract content key info)
    pub targets_json: String,
    /// Delegated-targets metadata signed by the content key
    pub delegated_targets_json: String,
}

/// Generate Level 1 metadata: only the content delegation, no root/snapshot/timestamp.
///
/// The server manages targets/snapshot/timestamp signing. The CLI only produces:
/// - `delegated_targets_json`: lists target files, signed by `content_signer`
/// - `targets_json`: unsigned carrier with delegation structure (so upload can extract content key info)
pub fn generate_content_only_metadata(
    targets: &[TargetFileInfo],
    runtime_uuid: &str,
    content_signer: &TufSigner,
) -> Result<ContentOnlyMetadata> {
    let delegated_targets_json =
        generate_delegated_targets_json(runtime_uuid, targets, content_signer)?;

    let targets_json = generate_unsigned_targets_json(runtime_uuid, content_signer)?;

    Ok(ContentOnlyMetadata {
        targets_json,
        delegated_targets_json,
    })
}

/// Generate all TUF metadata files using the delegated targets format.
///
/// - `targets.json`: empty inline targets, contains a delegation entry for `runtime-<uuid>`
/// - `delegations/runtime-<uuid>.json`: lists all target files, signed by `content_signer`
/// - `snapshot.json`: covers both metadata files
/// - `timestamp.json`: covers snapshot
///
/// `signer` signs targets/snapshot/timestamp; `content_signer` signs the delegation file.
/// At Level 0 (no keys configured) both are the same auto-generated key.
pub fn generate_repo_metadata(
    targets: &[TargetFileInfo],
    runtime_uuid: &str,
    signer: &TufSigner,
    content_signer: &TufSigner,
) -> Result<RepoMetadata> {
    let delegated_targets_json =
        generate_delegated_targets_json(runtime_uuid, targets, content_signer)?;

    let targets_json = generate_targets_json(runtime_uuid, content_signer, signer)?;

    let delegation_path = format!("delegations/runtime-{runtime_uuid}.json");
    let snapshot_json = generate_snapshot_json(
        &targets_json,
        &delegated_targets_json,
        &delegation_path,
        signer,
    )?;

    let timestamp_json = generate_timestamp_json(&snapshot_json, signer)?;

    Ok(RepoMetadata {
        targets_json,
        snapshot_json,
        timestamp_json,
        delegated_targets_json,
    })
}

/// Generate `delegations/runtime-<uuid>.json` — lists the actual target files,
/// signed by the content key.
pub fn generate_delegated_targets_json(
    runtime_uuid: &str,
    targets: &[TargetFileInfo],
    content_signer: &TufSigner,
) -> Result<String> {
    // 5-year TTL for delegated content
    let expires = chrono::Utc::now() + chrono::Duration::days(365 * 5);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let mut targets_map = serde_json::Map::new();
    for t in targets {
        let mut hashes = serde_json::Map::new();
        hashes.insert(
            "sha256".to_string(),
            serde_json::Value::String(t.sha256.clone()),
        );
        targets_map.insert(
            t.name.clone(),
            serde_json::json!({
                "hashes": hashes,
                "length": t.size,
            }),
        );
    }

    let signed: serde_json::Value = serde_json::json!({
        "_type": "targets",
        "expires": expires_str,
        "spec_version": "1.0.0",
        "targets": targets_map,
        "version": 1,
        "_delegation_name": format!("runtime-{runtime_uuid}")
    });

    sign_metadata(&signed, content_signer)
}

/// Generate `targets.json` in delegated format — empty inline targets with a
/// delegation entry pointing to `runtime-<uuid>`.
fn generate_targets_json(
    runtime_uuid: &str,
    content_signer: &TufSigner,
    signer: &TufSigner,
) -> Result<String> {
    // 5-year TTL for top-level targets
    let expires = chrono::Utc::now() + chrono::Duration::days(365 * 5);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let signed: serde_json::Value = serde_json::json!({
        "_type": "targets",
        "expires": expires_str,
        "spec_version": "1.0.0",
        "targets": {},
        "delegations": {
            "keys": {
                &content_signer.key_id: {
                    "keytype": "ed25519",
                    "keyval": { "public": &content_signer.public_key_hex },
                    "scheme": "ed25519"
                }
            },
            "roles": [
                {
                    "name": format!("runtime-{runtime_uuid}"),
                    "keyids": [&content_signer.key_id],
                    "threshold": 1,
                    "paths": ["manifest.json", "*.raw"],
                    "terminating": true
                }
            ]
        },
        "version": 1
    });

    sign_metadata(&signed, signer)
}

/// Generate an unsigned `targets.json` with delegation structure.
/// Used at Level 1 where the server signs targets.json, but the CLI needs to
/// record the content key info so upload can read it.
fn generate_unsigned_targets_json(
    runtime_uuid: &str,
    content_signer: &TufSigner,
) -> Result<String> {
    let expires = chrono::Utc::now() + chrono::Duration::days(365 * 5);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let signed: serde_json::Value = serde_json::json!({
        "_type": "targets",
        "expires": expires_str,
        "spec_version": "1.0.0",
        "targets": {},
        "delegations": {
            "keys": {
                &content_signer.key_id: {
                    "keytype": "ed25519",
                    "keyval": { "public": &content_signer.public_key_hex },
                    "scheme": "ed25519"
                }
            },
            "roles": [
                {
                    "name": format!("runtime-{runtime_uuid}"),
                    "keyids": [&content_signer.key_id],
                    "threshold": 1,
                    "paths": ["manifest.json", "*.raw"],
                    "terminating": true
                }
            ]
        },
        "version": 1
    });

    // Wrap in TUF envelope format (unsigned — signatures array is empty)
    let envelope = serde_json::json!({
        "signed": signed,
        "signatures": []
    });

    serde_json::to_string_pretty(&envelope).context("Failed to serialize unsigned targets.json")
}

/// Generate `snapshot.json` covering both `targets.json` and the delegation file.
fn generate_snapshot_json(
    targets_json: &str,
    delegated_targets_json: &str,
    delegation_path: &str,
    signer: &TufSigner,
) -> Result<String> {
    // 90-day TTL for snapshot
    let expires = chrono::Utc::now() + chrono::Duration::days(90);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let targets_hash = sha256_hex(targets_json.as_bytes());
    let targets_len = targets_json.len() as u64;

    let delegation_hash = sha256_hex(delegated_targets_json.as_bytes());
    let delegation_len = delegated_targets_json.len() as u64;

    let signed: serde_json::Value = serde_json::json!({
        "_type": "snapshot",
        "expires": expires_str,
        "meta": {
            "targets.json": {
                "hashes": { "sha256": targets_hash },
                "length": targets_len,
                "version": 1
            },
            delegation_path: {
                "hashes": { "sha256": delegation_hash },
                "length": delegation_len,
                "version": 1
            }
        },
        "spec_version": "1.0.0",
        "version": 1
    });

    sign_metadata(&signed, signer)
}

/// Generate `timestamp.json` covering `snapshot.json`. 7-day TTL.
fn generate_timestamp_json(snapshot_json: &str, signer: &TufSigner) -> Result<String> {
    let expires = chrono::Utc::now() + chrono::Duration::days(7);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let snapshot_hash = sha256_hex(snapshot_json.as_bytes());
    let snapshot_len = snapshot_json.len() as u64;

    let signed: serde_json::Value = serde_json::json!({
        "_type": "timestamp",
        "expires": expires_str,
        "meta": {
            "snapshot.json": {
                "hashes": { "sha256": snapshot_hash },
                "length": snapshot_len,
                "version": 1
            }
        },
        "spec_version": "1.0.0",
        "version": 1
    });

    sign_metadata(&signed, signer)
}

fn sign_metadata(signed: &serde_json::Value, signer: &TufSigner) -> Result<String> {
    let canonical = serde_jcs::to_string(signed).context("Failed to serialize canonical JSON")?;
    let sig_bytes = (signer.sign_fn)(canonical.as_bytes()).context("Failed to sign metadata")?;
    let sig_hex = hex_encode(&sig_bytes);

    let envelope: serde_json::Value = serde_json::json!({
        "signatures": [
            {
                "keyid": &signer.key_id,
                "sig": sig_hex
            }
        ],
        "signed": signed
    });

    serde_json::to_string_pretty(&envelope).context("Failed to serialize metadata")
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Structured output from the Phase 1 hash-collection script.
#[derive(Debug, Deserialize)]
pub struct HashCollectionOutput {
    pub targets: Vec<TargetFileInfo>,
    /// root.json content. Present at Level 2 / Sideload, absent at Level 1.
    #[serde(default)]
    pub root_json: Option<String>,
    /// UUID of the active runtime (from the `runtimes/<uuid>` symlink target)
    pub runtime_uuid: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::update_signing::tuf_signer_from_parts;
    use ed25519_compact::{KeyPair, Seed};

    fn test_signer() -> crate::utils::update_signing::TufSigner {
        let kp = KeyPair::from_seed(Seed::default());
        tuf_signer_from_parts(kp.sk, kp.pk)
    }

    fn test_signer_b() -> crate::utils::update_signing::TufSigner {
        let kp = KeyPair::from_seed(Seed::from([99u8; 32]));
        tuf_signer_from_parts(kp.sk, kp.pk)
    }

    fn parse_utc(expires: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(expires)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    const TEST_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn test_generate_repo_metadata_valid_json() {
        let signer = test_signer();
        let content_signer = test_signer();
        let targets = vec![
            TargetFileInfo {
                name: "manifest.json".to_string(),
                sha256: "abcd1234".repeat(8),
                size: 512,
            },
            TargetFileInfo {
                name: "app-0.1.0.raw".to_string(),
                sha256: "ef567890".repeat(8),
                size: 1048576,
            },
        ];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &content_signer).unwrap();

        let targets_parsed: serde_json::Value = serde_json::from_str(&repo.targets_json).unwrap();
        let snapshot_parsed: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let timestamp_parsed: serde_json::Value =
            serde_json::from_str(&repo.timestamp_json).unwrap();
        let delegated_parsed: serde_json::Value =
            serde_json::from_str(&repo.delegated_targets_json).unwrap();

        // targets.json must have empty inline targets
        assert_eq!(targets_parsed["signed"]["_type"], "targets");
        assert_eq!(targets_parsed["signed"]["version"], 1);
        let inline_targets = targets_parsed["signed"]["targets"].as_object().unwrap();
        assert!(
            inline_targets.is_empty(),
            "targets.json must have empty targets map"
        );

        // targets.json must have delegation block
        let delegations = &targets_parsed["signed"]["delegations"];
        assert!(
            delegations.is_object(),
            "targets.json must contain delegations"
        );
        let roles = delegations["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(
            roles[0]["name"].as_str().unwrap(),
            format!("runtime-{TEST_UUID}")
        );

        // delegated targets must list both files
        let dt_targets = delegated_parsed["signed"]["targets"].as_object().unwrap();
        assert_eq!(dt_targets.len(), 2);
        assert!(dt_targets.contains_key("manifest.json"));
        assert!(dt_targets.contains_key("app-0.1.0.raw"));

        // snapshot covers both files
        assert_eq!(snapshot_parsed["signed"]["_type"], "snapshot");
        let meta = snapshot_parsed["signed"]["meta"].as_object().unwrap();
        assert!(meta.contains_key("targets.json"));
        let delegation_key = format!("delegations/runtime-{TEST_UUID}.json");
        assert!(
            meta.contains_key(&delegation_key),
            "snapshot must cover delegation file"
        );

        assert_eq!(timestamp_parsed["signed"]["_type"], "timestamp");
        assert!(timestamp_parsed["signed"]["meta"]["snapshot.json"].is_object());
    }

    #[test]
    fn test_generate_delegated_targets_json_valid() {
        let signer = test_signer();
        let targets = vec![
            TargetFileInfo {
                name: "manifest.json".to_string(),
                sha256: "aa".repeat(32),
                size: 100,
            },
            TargetFileInfo {
                name: "app.raw".to_string(),
                sha256: "bb".repeat(32),
                size: 200,
            },
        ];

        let json = generate_delegated_targets_json(TEST_UUID, &targets, &signer).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["signed"]["_type"], "targets");
        let t = parsed["signed"]["targets"].as_object().unwrap();
        assert_eq!(t.len(), 2);
        assert!(t.contains_key("manifest.json"));
        assert!(t.contains_key("app.raw"));

        // 5-year TTL
        let expires = parsed["signed"]["expires"].as_str().unwrap();
        let years_from_now = (parse_utc(expires) - chrono::Utc::now()).num_days() / 365;
        assert!(
            years_from_now >= 4,
            "Delegated targets should expire ~5 years from now"
        );

        // Signature present
        let sigs = parsed["signatures"].as_array().unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0]["sig"].as_str().unwrap().len(), 128);
    }

    #[test]
    fn test_snapshot_covers_delegation_file() {
        let signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "manifest.json".to_string(),
            sha256: "cc".repeat(32),
            size: 50,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &signer).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let meta = &snapshot["signed"]["meta"];

        // Check hash + length match for delegation file
        let delegation_key = format!("delegations/runtime-{TEST_UUID}.json");
        let delegation_entry = &meta[&delegation_key];
        assert!(delegation_entry.is_object());
        let recorded_hash = delegation_entry["hashes"]["sha256"].as_str().unwrap();
        let recorded_len = delegation_entry["length"].as_u64().unwrap();
        assert_eq!(
            recorded_hash,
            sha256_hex(repo.delegated_targets_json.as_bytes())
        );
        assert_eq!(recorded_len, repo.delegated_targets_json.len() as u64);
    }

    #[test]
    fn test_ttl_targets_5_years() {
        let signer = test_signer();
        let repo = generate_repo_metadata(&[], TEST_UUID, &signer, &signer).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repo.targets_json).unwrap();
        let expires = parsed["signed"]["expires"].as_str().unwrap();
        let expiry = parse_utc(expires);
        let days = (expiry - chrono::Utc::now()).num_days();
        assert!(
            days > 365 * 4,
            "targets.json should have ~5yr TTL, got {days} days"
        );
    }

    #[test]
    fn test_ttl_snapshot_90_days() {
        let signer = test_signer();
        let repo = generate_repo_metadata(&[], TEST_UUID, &signer, &signer).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let expires = parsed["signed"]["expires"].as_str().unwrap();
        let expiry = parse_utc(expires);
        let days = (expiry - chrono::Utc::now()).num_days();
        assert!(
            (89..=91).contains(&days),
            "snapshot.json should have ~90d TTL, got {days} days"
        );
    }

    #[test]
    fn test_ttl_timestamp_7_days() {
        let signer = test_signer();
        let repo = generate_repo_metadata(&[], TEST_UUID, &signer, &signer).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repo.timestamp_json).unwrap();
        let expires = parsed["signed"]["expires"].as_str().unwrap();
        let expiry = parse_utc(expires);
        let days = (expiry - chrono::Utc::now()).num_days();
        assert!(
            (6..=8).contains(&days),
            "timestamp.json should have ~7d TTL, got {days} days"
        );
    }

    #[test]
    fn test_signatures_present_and_valid_format() {
        let signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "test.raw".to_string(),
            sha256: "aa".repeat(32),
            size: 100,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &signer).unwrap();

        for json_str in [
            &repo.targets_json,
            &repo.snapshot_json,
            &repo.timestamp_json,
            &repo.delegated_targets_json,
        ] {
            let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
            let sigs = parsed["signatures"].as_array().unwrap();
            assert_eq!(sigs.len(), 1);
            assert!(sigs[0]["keyid"].is_string());
            let sig_hex = sigs[0]["sig"].as_str().unwrap();
            assert_eq!(sig_hex.len(), 128);
        }
    }

    #[test]
    fn test_separate_content_key() {
        let signer = test_signer();
        let content_signer = test_signer_b();
        let targets = vec![TargetFileInfo {
            name: "manifest.json".to_string(),
            sha256: "dd".repeat(32),
            size: 256,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &content_signer).unwrap();

        // targets.json is signed by signer's key
        let targets_parsed: serde_json::Value = serde_json::from_str(&repo.targets_json).unwrap();
        let targets_keyid = targets_parsed["signatures"][0]["keyid"].as_str().unwrap();
        assert_eq!(targets_keyid, signer.key_id);

        // delegation is signed by content_signer's key
        let dt_parsed: serde_json::Value =
            serde_json::from_str(&repo.delegated_targets_json).unwrap();
        let dt_keyid = dt_parsed["signatures"][0]["keyid"].as_str().unwrap();
        assert_eq!(dt_keyid, content_signer.key_id);

        // targets.json delegation keys section references the content_signer key
        let delegation_keys = targets_parsed["signed"]["delegations"]["keys"]
            .as_object()
            .unwrap();
        assert!(delegation_keys.contains_key(&content_signer.key_id));
    }

    #[test]
    fn test_snapshot_references_targets_hash() {
        let signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "m.json".to_string(),
            sha256: "bb".repeat(32),
            size: 42,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &signer).unwrap();

        let snapshot: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let meta_entry = &snapshot["signed"]["meta"]["targets.json"];
        let recorded_hash = meta_entry["hashes"]["sha256"].as_str().unwrap();
        let recorded_len = meta_entry["length"].as_u64().unwrap();

        assert_eq!(recorded_hash, sha256_hex(repo.targets_json.as_bytes()));
        assert_eq!(recorded_len, repo.targets_json.len() as u64);
    }

    #[test]
    fn test_timestamp_references_snapshot_hash() {
        let signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "x.raw".to_string(),
            sha256: "cc".repeat(32),
            size: 99,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &signer).unwrap();

        let timestamp: serde_json::Value = serde_json::from_str(&repo.timestamp_json).unwrap();
        let meta_entry = &timestamp["signed"]["meta"]["snapshot.json"];
        let recorded_hash = meta_entry["hashes"]["sha256"].as_str().unwrap();
        let recorded_len = meta_entry["length"].as_u64().unwrap();

        assert_eq!(recorded_hash, sha256_hex(repo.snapshot_json.as_bytes()));
        assert_eq!(recorded_len, repo.snapshot_json.len() as u64);
    }

    #[test]
    fn test_parseable_by_tough() {
        let signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "manifest.json".to_string(),
            sha256: "dd".repeat(32),
            size: 256,
        }];

        let repo = generate_repo_metadata(&targets, TEST_UUID, &signer, &signer).unwrap();

        let _: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&repo.targets_json)
                .expect("targets.json should be parseable by tough");

        let _: tough::schema::Signed<tough::schema::Snapshot> =
            serde_json::from_str(&repo.snapshot_json)
                .expect("snapshot.json should be parseable by tough");

        let _: tough::schema::Signed<tough::schema::Timestamp> =
            serde_json::from_str(&repo.timestamp_json)
                .expect("timestamp.json should be parseable by tough");

        // The delegated targets file is also a Targets document
        let _: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&repo.delegated_targets_json)
                .expect("delegated targets.json should be parseable by tough");
    }

    #[test]
    fn test_compute_image_id_deterministic() {
        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let id1 = compute_image_id(hash);
        let id2 = compute_image_id(hash);
        assert_eq!(id1, id2, "same content hash must produce same image_id");
    }

    #[test]
    fn test_compute_image_id_different_content() {
        let hash_a = "aaaa".repeat(16);
        let hash_b = "bbbb".repeat(16);
        let id_a = compute_image_id(&hash_a);
        let id_b = compute_image_id(&hash_b);
        assert_ne!(
            id_a, id_b,
            "different content hashes must produce different image_ids"
        );
    }

    #[test]
    fn test_compute_image_id_is_valid_uuid() {
        let hash = "cc".repeat(32);
        let id = compute_image_id(&hash);
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "image_id must be a valid UUID"
        );
    }

    #[test]
    fn test_generate_content_only_metadata_valid_json() {
        let content_signer = test_signer();
        let targets = vec![
            TargetFileInfo {
                name: "manifest.json".to_string(),
                sha256: "abcd1234".repeat(8),
                size: 512,
            },
            TargetFileInfo {
                name: "app-0.1.0.raw".to_string(),
                sha256: "ef567890".repeat(8),
                size: 1048576,
            },
        ];

        let metadata =
            generate_content_only_metadata(&targets, TEST_UUID, &content_signer).unwrap();

        // delegated_targets_json should be signed by content key
        let delegated: serde_json::Value =
            serde_json::from_str(&metadata.delegated_targets_json).unwrap();
        assert_eq!(delegated["signed"]["_type"], "targets");
        let del_targets = delegated["signed"]["targets"].as_object().unwrap();
        assert_eq!(del_targets.len(), 2);
        assert!(!delegated["signatures"].as_array().unwrap().is_empty());

        // targets.json should be unsigned (empty signatures) with delegation info
        let targets_parsed: serde_json::Value =
            serde_json::from_str(&metadata.targets_json).unwrap();
        assert_eq!(targets_parsed["signed"]["_type"], "targets");
        assert!(
            targets_parsed["signatures"].as_array().unwrap().is_empty(),
            "Level 1 targets.json must be unsigned"
        );

        // Should have delegation structure with content key info
        let delegations = &targets_parsed["signed"]["delegations"];
        assert!(delegations["keys"].is_object());
        let keys = delegations["keys"].as_object().unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains_key(&content_signer.key_id));

        let roles = delegations["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0]["name"], format!("runtime-{TEST_UUID}"));
    }

    #[test]
    fn test_content_only_metadata_readable_by_upload() {
        // Verify that the upload's read_delegation_info logic can extract
        // content key info from Level 1 unsigned targets.json
        let content_signer = test_signer();
        let targets = vec![TargetFileInfo {
            name: "manifest.json".to_string(),
            sha256: "abcd1234".repeat(8),
            size: 512,
        }];

        let metadata =
            generate_content_only_metadata(&targets, TEST_UUID, &content_signer).unwrap();

        // Simulate what upload's read_delegation_info does:
        let targets_parsed: serde_json::Value =
            serde_json::from_str(&metadata.targets_json).unwrap();
        let delegations = &targets_parsed["signed"]["delegations"];

        // Extract role name
        let roles = delegations["roles"].as_array().unwrap();
        let role = &roles[0];
        let role_name = role["name"].as_str().unwrap();
        assert!(role_name.starts_with("runtime-"));

        // Extract content keyid
        let content_keyid = role["keyids"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(content_keyid, content_signer.key_id);

        // Extract content key hex
        let content_key_hex = delegations
            .pointer(&format!("/keys/{content_keyid}/keyval/public"))
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(content_key_hex, content_signer.public_key_hex);
    }
}
