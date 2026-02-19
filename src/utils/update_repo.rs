use anyhow::{Context, Result};
use ed25519_compact::{PublicKey, SecretKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

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

/// The three generated TUF metadata files (all signed JSON strings).
pub struct RepoMetadata {
    pub targets_json: String,
    pub snapshot_json: String,
    pub timestamp_json: String,
}

/// Generate all three TUF metadata files (targets, snapshot, timestamp),
/// each signed with the provided key. The root.json is assumed to already
/// exist from the build phase.
pub fn generate_repo_metadata(
    targets: &[TargetFileInfo],
    secret_key: &SecretKey,
    public_key: &PublicKey,
) -> Result<RepoMetadata> {
    let key_id = compute_key_id(public_key);

    let targets_json = generate_targets_json(targets, secret_key, &key_id)?;
    let snapshot_json = generate_snapshot_json(&targets_json, secret_key, &key_id)?;
    let timestamp_json = generate_timestamp_json(&snapshot_json, secret_key, &key_id)?;

    Ok(RepoMetadata {
        targets_json,
        snapshot_json,
        timestamp_json,
    })
}

fn generate_targets_json(
    targets: &[TargetFileInfo],
    secret_key: &SecretKey,
    key_id: &str,
) -> Result<String> {
    let expires = chrono::Utc::now() + chrono::Duration::days(365);
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
        "version": 1
    });

    sign_metadata(&signed, secret_key, key_id)
}

fn generate_snapshot_json(
    targets_json: &str,
    secret_key: &SecretKey,
    key_id: &str,
) -> Result<String> {
    let expires = chrono::Utc::now() + chrono::Duration::days(365);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let targets_hash = sha256_hex(targets_json.as_bytes());
    let targets_len = targets_json.len() as u64;

    let signed: serde_json::Value = serde_json::json!({
        "_type": "snapshot",
        "expires": expires_str,
        "meta": {
            "targets.json": {
                "hashes": {
                    "sha256": targets_hash
                },
                "length": targets_len,
                "version": 1
            }
        },
        "spec_version": "1.0.0",
        "version": 1
    });

    sign_metadata(&signed, secret_key, key_id)
}

fn generate_timestamp_json(
    snapshot_json: &str,
    secret_key: &SecretKey,
    key_id: &str,
) -> Result<String> {
    let expires = chrono::Utc::now() + chrono::Duration::days(1);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let snapshot_hash = sha256_hex(snapshot_json.as_bytes());
    let snapshot_len = snapshot_json.len() as u64;

    let signed: serde_json::Value = serde_json::json!({
        "_type": "timestamp",
        "expires": expires_str,
        "meta": {
            "snapshot.json": {
                "hashes": {
                    "sha256": snapshot_hash
                },
                "length": snapshot_len,
                "version": 1
            }
        },
        "spec_version": "1.0.0",
        "version": 1
    });

    sign_metadata(&signed, secret_key, key_id)
}

fn sign_metadata(
    signed: &serde_json::Value,
    secret_key: &SecretKey,
    key_id: &str,
) -> Result<String> {
    let canonical = serde_jcs::to_string(signed).context("Failed to serialize canonical JSON")?;

    let sig = secret_key.sign(&canonical, None);
    let sig_hex = hex_encode(sig.as_ref());

    let envelope: serde_json::Value = serde_json::json!({
        "signatures": [
            {
                "keyid": key_id,
                "sig": sig_hex
            }
        ],
        "signed": signed
    });

    serde_json::to_string_pretty(&envelope).context("Failed to serialize metadata")
}

fn compute_key_id(public_key: &PublicKey) -> String {
    let public_hex = hex_encode(public_key.as_ref());
    let canonical = format!(
        r#"{{"keytype":"ed25519","keyval":{{"public":"{public_hex}"}},"scheme":"ed25519"}}"#
    );
    sha256_hex(canonical.as_bytes())
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
    pub root_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_compact::{KeyPair, Seed};

    fn test_keypair() -> KeyPair {
        KeyPair::from_seed(Seed::default())
    }

    #[test]
    fn test_generate_repo_metadata_valid_json() {
        let kp = test_keypair();
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

        let repo = generate_repo_metadata(&targets, &kp.sk, &kp.pk).unwrap();

        let targets_parsed: serde_json::Value = serde_json::from_str(&repo.targets_json).unwrap();
        let snapshot_parsed: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let timestamp_parsed: serde_json::Value =
            serde_json::from_str(&repo.timestamp_json).unwrap();

        assert_eq!(targets_parsed["signed"]["_type"], "targets");
        assert_eq!(targets_parsed["signed"]["version"], 1);
        assert_eq!(targets_parsed["signed"]["spec_version"], "1.0.0");

        let target_entries = targets_parsed["signed"]["targets"].as_object().unwrap();
        assert_eq!(target_entries.len(), 2);
        assert!(target_entries.contains_key("manifest.json"));
        assert!(target_entries.contains_key("app-0.1.0.raw"));

        assert_eq!(snapshot_parsed["signed"]["_type"], "snapshot");
        assert!(snapshot_parsed["signed"]["meta"]["targets.json"].is_object());

        assert_eq!(timestamp_parsed["signed"]["_type"], "timestamp");
        assert!(timestamp_parsed["signed"]["meta"]["snapshot.json"].is_object());
    }

    #[test]
    fn test_signatures_present_and_valid_format() {
        let kp = test_keypair();
        let targets = vec![TargetFileInfo {
            name: "test.raw".to_string(),
            sha256: "aa".repeat(32),
            size: 100,
        }];

        let repo = generate_repo_metadata(&targets, &kp.sk, &kp.pk).unwrap();

        for json_str in [
            &repo.targets_json,
            &repo.snapshot_json,
            &repo.timestamp_json,
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
    fn test_snapshot_references_targets_hash() {
        let kp = test_keypair();
        let targets = vec![TargetFileInfo {
            name: "m.json".to_string(),
            sha256: "bb".repeat(32),
            size: 42,
        }];

        let repo = generate_repo_metadata(&targets, &kp.sk, &kp.pk).unwrap();

        let snapshot: serde_json::Value = serde_json::from_str(&repo.snapshot_json).unwrap();
        let meta_entry = &snapshot["signed"]["meta"]["targets.json"];
        let recorded_hash = meta_entry["hashes"]["sha256"].as_str().unwrap();
        let recorded_len = meta_entry["length"].as_u64().unwrap();

        let actual_hash = sha256_hex(repo.targets_json.as_bytes());
        assert_eq!(recorded_hash, actual_hash);
        assert_eq!(recorded_len, repo.targets_json.len() as u64);
    }

    #[test]
    fn test_timestamp_references_snapshot_hash() {
        let kp = test_keypair();
        let targets = vec![TargetFileInfo {
            name: "x.raw".to_string(),
            sha256: "cc".repeat(32),
            size: 99,
        }];

        let repo = generate_repo_metadata(&targets, &kp.sk, &kp.pk).unwrap();

        let timestamp: serde_json::Value = serde_json::from_str(&repo.timestamp_json).unwrap();
        let meta_entry = &timestamp["signed"]["meta"]["snapshot.json"];
        let recorded_hash = meta_entry["hashes"]["sha256"].as_str().unwrap();
        let recorded_len = meta_entry["length"].as_u64().unwrap();

        let actual_hash = sha256_hex(repo.snapshot_json.as_bytes());
        assert_eq!(recorded_hash, actual_hash);
        assert_eq!(recorded_len, repo.snapshot_json.len() as u64);
    }

    #[test]
    fn test_key_id_matches_update_signing() {
        let kp = test_keypair();
        let key_id = compute_key_id(&kp.pk);
        assert_eq!(key_id.len(), 64);

        let key_id_2 = compute_key_id(&kp.pk);
        assert_eq!(key_id, key_id_2);
    }

    #[test]
    fn test_empty_targets_list() {
        let kp = test_keypair();
        let repo = generate_repo_metadata(&[], &kp.sk, &kp.pk).unwrap();

        let targets: serde_json::Value = serde_json::from_str(&repo.targets_json).unwrap();
        let target_entries = targets["signed"]["targets"].as_object().unwrap();
        assert!(target_entries.is_empty());
    }

    #[test]
    fn test_parseable_by_tough() {
        let kp = test_keypair();
        let targets = vec![TargetFileInfo {
            name: "manifest.json".to_string(),
            sha256: "dd".repeat(32),
            size: 256,
        }];

        let repo = generate_repo_metadata(&targets, &kp.sk, &kp.pk).unwrap();

        let _: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&repo.targets_json)
                .expect("targets.json should be parseable by tough");

        let _: tough::schema::Signed<tough::schema::Snapshot> =
            serde_json::from_str(&repo.snapshot_json)
                .expect("snapshot.json should be parseable by tough");

        let _: tough::schema::Signed<tough::schema::Timestamp> =
            serde_json::from_str(&repo.timestamp_json)
                .expect("timestamp.json should be parseable by tough");
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
}
