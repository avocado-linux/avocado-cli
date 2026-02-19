use anyhow::{Context, Result};
use ed25519_compact::{KeyPair, PublicKey, SecretKey, Seed};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::signing_keys::KeysRegistry;

const AUTO_KEY_DIR: &str = "signing";
const AUTO_KEY_NAME: &str = "auto";

/// Resolve the ed25519 key pair for update authority generation.
///
/// Level 0: No signing key configured -- auto-generate an ephemeral key
///          stored in `<project_dir>/.avocado/signing/`.
/// Level 1: `signing.key` is configured -- load the named key from the
///          global signing keys registry.
///
/// Returns (secret_key, public_key).
pub fn resolve_signing_key(
    runtime_signing_key_name: Option<&str>,
    project_dir: &Path,
) -> Result<(SecretKey, PublicKey)> {
    match runtime_signing_key_name {
        Some(key_name) => load_key_from_registry(key_name),
        None => ensure_auto_generated_key(project_dir),
    }
}

fn load_key_from_registry(key_name: &str) -> Result<(SecretKey, PublicKey)> {
    let registry = KeysRegistry::load().context("Failed to load signing keys registry")?;
    let entry = registry
        .get_key(key_name)
        .with_context(|| format!("Signing key '{key_name}' not found in registry"))?;

    if entry.algorithm != "ed25519" {
        anyhow::bail!(
            "Signing key '{key_name}' uses algorithm '{}', but update authority requires ed25519",
            entry.algorithm
        );
    }

    let key_path = uri_to_path(&entry.uri)?;
    load_keypair_from_files(&key_path)
}

fn uri_to_path(uri: &str) -> Result<PathBuf> {
    if let Some(path) = uri.strip_prefix("file://") {
        Ok(PathBuf::from(path))
    } else {
        anyhow::bail!("Unsupported key URI: '{uri}'. Only file:// URIs are supported for update authority generation.");
    }
}

fn load_keypair_from_files(base_path: &Path) -> Result<(SecretKey, PublicKey)> {
    let private_key_path = base_path.with_extension("key");
    let public_key_path = base_path.with_extension("pub");

    let private_b64 = fs::read_to_string(&private_key_path)
        .with_context(|| format!("Failed to read private key: {}", private_key_path.display()))?;
    let public_b64 = fs::read_to_string(&public_key_path)
        .with_context(|| format!("Failed to read public key: {}", public_key_path.display()))?;

    use base64::prelude::*;
    let seed_bytes = BASE64_STANDARD
        .decode(private_b64.trim())
        .context("Failed to decode private key")?;
    let public_bytes = BASE64_STANDARD
        .decode(public_b64.trim())
        .context("Failed to decode public key")?;

    let seed =
        ed25519_compact::Seed::from_slice(&seed_bytes).context("Invalid ed25519 seed length")?;
    let keypair = KeyPair::from_seed(seed);
    let expected_pk =
        PublicKey::from_slice(&public_bytes).context("Invalid ed25519 public key length")?;

    if keypair.pk != expected_pk {
        anyhow::bail!("Public key does not match private key seed");
    }

    Ok((keypair.sk, keypair.pk))
}

fn ensure_auto_generated_key(project_dir: &Path) -> Result<(SecretKey, PublicKey)> {
    let key_dir = project_dir.join(".avocado").join(AUTO_KEY_DIR);
    let base_path = key_dir.join(AUTO_KEY_NAME);
    let private_key_path = base_path.with_extension("key");

    if private_key_path.exists() {
        return load_keypair_from_files(&base_path);
    }

    fs::create_dir_all(&key_dir)
        .with_context(|| format!("Failed to create directory: {}", key_dir.display()))?;

    let keypair = KeyPair::from_seed(Seed::default());

    use base64::prelude::*;
    let private_b64 = BASE64_STANDARD.encode(keypair.sk.seed().as_ref());
    let public_b64 = BASE64_STANDARD.encode(keypair.pk.as_ref());

    fs::write(&private_key_path, &private_b64)
        .with_context(|| format!("Failed to write key: {}", private_key_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600))?;
    }

    let public_key_path = base_path.with_extension("pub");
    fs::write(&public_key_path, &public_b64)
        .with_context(|| format!("Failed to write key: {}", public_key_path.display()))?;

    Ok((keypair.sk, keypair.pk))
}

/// Compute the TUF key ID: SHA-256 of the canonical JSON representation of the key.
fn compute_key_id(public_key: &PublicKey) -> String {
    let key_value = canonical_key_json(public_key);
    let mut hasher = Sha256::new();
    hasher.update(key_value.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Produce the canonical JSON for an ed25519 public key (sorted keys, no whitespace).
fn canonical_key_json(public_key: &PublicKey) -> String {
    let public_hex = hex_encode(public_key.as_ref());
    format!(r#"{{"keytype":"ed25519","keyval":{{"public":"{public_hex}"}},"scheme":"ed25519"}}"#)
}

/// Generate the TUF root.json content as a JSON string.
///
/// Produces a valid TUF 1.0.0 root metadata file with the given key
/// assigned to all four roles (root, targets, snapshot, timestamp),
/// each with threshold 1.
pub fn generate_root_json(secret_key: &SecretKey, public_key: &PublicKey) -> Result<String> {
    let key_id = compute_key_id(public_key);
    let public_hex = hex_encode(public_key.as_ref());

    let expires = chrono::Utc::now() + chrono::Duration::days(365);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Build the "signed" portion as a serde_json::Value for canonical serialization
    let signed: serde_json::Value = serde_json::json!({
        "_type": "root",
        "consistent_snapshot": false,
        "expires": expires_str,
        "keys": {
            &key_id: {
                "keytype": "ed25519",
                "keyval": {
                    "public": public_hex
                },
                "scheme": "ed25519"
            }
        },
        "roles": {
            "root": {
                "keyids": [&key_id],
                "threshold": 1
            },
            "snapshot": {
                "keyids": [&key_id],
                "threshold": 1
            },
            "targets": {
                "keyids": [&key_id],
                "threshold": 1
            },
            "timestamp": {
                "keyids": [&key_id],
                "threshold": 1
            }
        },
        "spec_version": "1.0.0",
        "version": 1
    });

    // Canonical JSON: sorted keys, no extra whitespace.
    // serde_jcs produces RFC 8785 JSON Canonicalization Scheme output.
    let canonical = serde_jcs::to_string(&signed).context("Failed to serialize canonical JSON")?;

    // Sign the canonical form
    let sig = secret_key.sign(&canonical, None);
    let sig_hex = hex_encode(sig.as_ref());

    // Assemble the complete root.json
    let root_json: serde_json::Value = serde_json::json!({
        "signatures": [
            {
                "keyid": key_id,
                "sig": sig_hex
            }
        ],
        "signed": signed
    });

    serde_json::to_string_pretty(&root_json).context("Failed to serialize root.json")
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_auto_generate_key() {
        let tmp = TempDir::new().unwrap();
        let (sk, pk) = ensure_auto_generated_key(tmp.path()).unwrap();

        // Key should be loadable again
        let (sk2, pk2) = ensure_auto_generated_key(tmp.path()).unwrap();
        assert_eq!(pk, pk2);
        assert_eq!(sk.seed(), sk2.seed());
    }

    #[test]
    fn test_generate_root_json_valid() {
        let keypair = KeyPair::from_seed(Seed::default());
        let root_json = generate_root_json(&keypair.sk, &keypair.pk).unwrap();

        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();

        // Should have signatures and signed sections
        assert!(parsed.get("signatures").is_some());
        assert!(parsed.get("signed").is_some());

        let signed = &parsed["signed"];
        assert_eq!(signed["_type"], "root");
        assert_eq!(signed["spec_version"], "1.0.0");
        assert_eq!(signed["version"], 1);

        // All four roles should be present
        let roles = signed["roles"].as_object().unwrap();
        assert!(roles.contains_key("root"));
        assert!(roles.contains_key("targets"));
        assert!(roles.contains_key("snapshot"));
        assert!(roles.contains_key("timestamp"));

        // Should have exactly one key
        let keys = signed["keys"].as_object().unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_generate_root_json_signature_present() {
        let keypair = KeyPair::from_seed(Seed::default());
        let root_json = generate_root_json(&keypair.sk, &keypair.pk).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();

        let sigs = parsed["signatures"].as_array().unwrap();
        assert_eq!(sigs.len(), 1);

        let sig = &sigs[0];
        assert!(sig.get("keyid").is_some());
        assert!(sig.get("sig").is_some());

        // Signature should be 128 hex chars (64 bytes)
        let sig_hex = sig["sig"].as_str().unwrap();
        assert_eq!(sig_hex.len(), 128);
    }

    #[test]
    fn test_key_id_deterministic() {
        let keypair = KeyPair::from_seed(Seed::default());
        let id1 = compute_key_id(&keypair.pk);
        let id2 = compute_key_id(&keypair.pk);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_canonical_key_json_format() {
        let keypair = KeyPair::from_seed(Seed::default());
        let canonical = canonical_key_json(&keypair.pk);

        // Should parse as valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&canonical).unwrap();
        assert_eq!(parsed["keytype"], "ed25519");
        assert_eq!(parsed["scheme"], "ed25519");
        assert!(parsed["keyval"]["public"].as_str().unwrap().len() == 64);
    }

    #[test]
    fn test_resolve_signing_key_level0() {
        let tmp = TempDir::new().unwrap();
        let (_, pk) = resolve_signing_key(None, tmp.path()).unwrap();
        assert_eq!(pk.as_ref().len(), 32);
    }

    #[test]
    fn test_generated_root_json_parseable_by_tough() {
        let keypair = KeyPair::from_seed(Seed::default());
        let root_json = generate_root_json(&keypair.sk, &keypair.pk).unwrap();

        // tough::schema::Signed<Root> should be able to deserialize our output
        let signed_root: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_str(&root_json)
                .expect("root.json should be parseable by tough::schema");

        let root = &signed_root.signed;
        assert_eq!(root.version.get(), 1);
        assert_eq!(root.spec_version, "1.0.0");
        assert!(!root.consistent_snapshot);
        assert_eq!(root.keys.len(), 1);
        assert_eq!(root.roles.len(), 4);

        // Verify key type
        for (_, key) in &root.keys {
            assert!(matches!(key, tough::schema::key::Key::Ed25519 { .. }));
        }

        // Verify all roles present with threshold 1
        for role_type in &[
            tough::schema::RoleType::Root,
            tough::schema::RoleType::Targets,
            tough::schema::RoleType::Snapshot,
            tough::schema::RoleType::Timestamp,
        ] {
            let role_keys = root
                .roles
                .get(role_type)
                .unwrap_or_else(|| panic!("Missing role: {role_type:?}"));
            assert_eq!(role_keys.threshold.get(), 1);
            assert_eq!(role_keys.keyids.len(), 1);
        }
    }
}
