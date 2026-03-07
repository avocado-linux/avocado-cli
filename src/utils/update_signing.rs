use anyhow::{Context, Result};
use ed25519_compact::{KeyPair, PublicKey, SecretKey, Seed};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::output::{print_warning, OutputLevel};
use crate::utils::signing_keys::{is_file_uri, is_pkcs11_uri, KeysRegistry};

const AUTO_KEY_DIR: &str = "signing";
const AUTO_KEY_NAME: &str = "auto";

type SignFn = Box<dyn Fn(&[u8]) -> Result<Vec<u8>>>;

/// Unified signing abstraction for TUF metadata.
///
/// Mirrors the `SignFn` pattern from `image_signing.rs` to enable future PKCS#11/HSM support.
/// Holds the public key information needed to construct TUF key descriptors.
pub struct TufSigner {
    /// Signs arbitrary bytes; returns the raw 64-byte ed25519 signature.
    pub sign_fn: SignFn,
    /// 32-byte ed25519 public key as lowercase hex.
    pub public_key_hex: String,
    /// SHA-256 of the canonical JSON key descriptor (TUF key ID).
    pub key_id: String,
}

pub(crate) fn tuf_signer_from_parts(sk: SecretKey, pk: PublicKey) -> TufSigner {
    let public_key_hex = hex_encode(pk.as_ref());
    let key_id = compute_key_id_from_hex(&public_key_hex);
    TufSigner {
        sign_fn: Box::new(move |data: &[u8]| Ok(sk.sign(data, None).as_ref().to_vec())),
        public_key_hex,
        key_id,
    }
}

fn load_tuf_signer_from_registry(key_name: &str) -> Result<TufSigner> {
    let registry = KeysRegistry::load().context("Failed to load signing keys registry")?;
    let entry = registry
        .get_key(key_name)
        .with_context(|| format!("Signing key '{key_name}' not found in registry"))?;

    if entry.algorithm != "ed25519" {
        anyhow::bail!(
            "Signing key '{key_name}' uses algorithm '{}', but TUF signing requires ed25519",
            entry.algorithm
        );
    }

    if is_file_uri(&entry.uri) {
        let key_path = uri_to_path(&entry.uri)?;
        let (sk, pk) = load_keypair_from_files(&key_path)?;
        Ok(tuf_signer_from_parts(sk, pk))
    } else if is_pkcs11_uri(&entry.uri) {
        anyhow::bail!(
            "PKCS#11 signing for TUF metadata is not yet supported. \
             Use a file-based key for '{key_name}'."
        )
    } else {
        anyhow::bail!(
            "Unsupported key URI '{}' for key '{key_name}'. Only file:// URIs are supported.",
            entry.uri
        )
    }
}

/// Resolve the TUF signer for operational roles (root, targets, snapshot, timestamp).
///
/// Level 0: No key configured — auto-generate a persistent key stored in
///          `<project_dir>/.avocado/signing/`. Emits a warning.
/// Level 1: Named key — load from the global signing keys registry.
pub fn resolve_signing_key(key_name: Option<&str>, project_dir: &Path) -> Result<TufSigner> {
    match key_name {
        Some(name) => load_tuf_signer_from_registry(name),
        None => {
            let key_dir = project_dir.join(".avocado").join(AUTO_KEY_DIR);
            let key_path = key_dir.join(AUTO_KEY_NAME).with_extension("key");
            if key_path.exists() {
                print_warning(
                    &format!(
                        "No signing key configured. Using auto-generated key at {}.\n\
                         \x20        This key is NOT suitable for production use.\n\
                         \x20        Run 'avocado signing-keys create' and set 'signing.key' in your config.",
                        key_path.display()
                    ),
                    OutputLevel::Normal,
                );
            } else {
                print_warning(
                    &format!(
                        "No signing key configured. Generating a new key at {}.\n\
                         \x20        This key is NOT suitable for production use.\n\
                         \x20        Run 'avocado signing-keys create' and set 'signing.key' in your config.",
                        key_path.display()
                    ),
                    OutputLevel::Normal,
                );
            }
            let (sk, pk) = ensure_auto_generated_key(project_dir)?;
            Ok(tuf_signer_from_parts(sk, pk))
        }
    }
}

/// Resolve the TUF signer for delegated-targets content signing.
///
/// Priority: `content_key_name` → `key_name` → auto-generated (Level 0 collapses to same key).
/// Does not emit an additional warning when falling back to the auto-generated key, since
/// `resolve_signing_key` already warned.
pub fn resolve_content_key(
    content_key_name: Option<&str>,
    key_name: Option<&str>,
    project_dir: &Path,
) -> Result<TufSigner> {
    match (content_key_name, key_name) {
        (Some(name), _) => load_tuf_signer_from_registry(name),
        (None, Some(name)) => load_tuf_signer_from_registry(name),
        (None, None) => {
            // Level 0: reuse the same auto-generated key (no extra warning)
            let (sk, pk) = ensure_auto_generated_key(project_dir)?;
            Ok(tuf_signer_from_parts(sk, pk))
        }
    }
}

fn uri_to_path(uri: &str) -> Result<PathBuf> {
    if let Some(path) = uri.strip_prefix("file://") {
        Ok(PathBuf::from(path))
    } else {
        anyhow::bail!(
            "Unsupported key URI: '{uri}'. Only file:// URIs are supported for TUF signing."
        );
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

    let keypair = KeyPair::from_seed(Seed::generate());

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

/// Compute the TUF key ID from a hex-encoded public key.
/// Key ID = SHA-256 of the canonical JSON key descriptor.
fn compute_key_id_from_hex(public_key_hex: &str) -> String {
    let canonical = format!(
        r#"{{"keytype":"ed25519","keyval":{{"public":"{public_key_hex}"}},"scheme":"ed25519"}}"#
    );
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Produce the canonical JSON for an ed25519 public key (sorted keys, no whitespace).
#[cfg(test)]
fn canonical_key_json(public_key: &PublicKey) -> String {
    let public_hex = hex_encode(public_key.as_ref());
    format!(r#"{{"keytype":"ed25519","keyval":{{"public":"{public_hex}"}},"scheme":"ed25519"}}"#)
}

/// Generate a TUF root.json with two distinct key roles:
/// - `root_signer` is authorized for the root role and signs the document.
/// - `server_public_key_hex` is the key authorized for targets/snapshot/timestamp
///   (for local deploy, pass `root_signer.public_key_hex`).
///
/// Used by `generate_root_json` (same key for all roles) and in future by
/// `avocado connect tuf setup` (split root vs. server operational key).
pub fn generate_multi_key_root_json(
    root_signer: &TufSigner,
    server_public_key_hex: &str,
    version: u64,
    expires_days: i64,
) -> Result<String> {
    let root_key_id = &root_signer.key_id;
    let root_public_hex = &root_signer.public_key_hex;
    let server_key_id = compute_key_id_from_hex(server_public_key_hex);

    let expires = chrono::Utc::now() + chrono::Duration::days(expires_days);
    let expires_str = expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Build the keys section — include both keys (may be the same key).
    let mut keys_obj = serde_json::Map::new();
    keys_obj.insert(
        root_key_id.clone(),
        serde_json::json!({
            "keytype": "ed25519",
            "keyval": { "public": root_public_hex },
            "scheme": "ed25519"
        }),
    );
    if server_key_id != *root_key_id {
        keys_obj.insert(
            server_key_id.clone(),
            serde_json::json!({
                "keytype": "ed25519",
                "keyval": { "public": server_public_key_hex },
                "scheme": "ed25519"
            }),
        );
    }

    let signed: serde_json::Value = serde_json::json!({
        "_type": "root",
        "consistent_snapshot": false,
        "expires": expires_str,
        "keys": keys_obj,
        "roles": {
            "root": {
                "keyids": [root_key_id],
                "threshold": 1
            },
            "snapshot": {
                "keyids": [&server_key_id],
                "threshold": 1
            },
            "targets": {
                "keyids": [&server_key_id],
                "threshold": 1
            },
            "timestamp": {
                "keyids": [&server_key_id],
                "threshold": 1
            }
        },
        "spec_version": "1.0.0",
        "version": version
    });

    // Canonical JSON per RFC 8785
    let canonical = serde_jcs::to_string(&signed).context("Failed to serialize canonical JSON")?;

    let sig_bytes =
        (root_signer.sign_fn)(canonical.as_bytes()).context("Failed to sign root.json")?;
    let sig_hex = hex_encode(&sig_bytes);

    let root_json: serde_json::Value = serde_json::json!({
        "signatures": [
            {
                "keyid": root_key_id,
                "sig": sig_hex
            }
        ],
        "signed": signed
    });

    serde_json::to_string_pretty(&root_json).context("Failed to serialize root.json")
}

/// Generate a TUF root.json signed by `signer`, with that same key authorized for
/// all four roles (root, targets, snapshot, timestamp). Used by `avocado runtime build`.
pub fn generate_root_json(signer: &TufSigner) -> Result<String> {
    generate_multi_key_root_json(signer, &signer.public_key_hex, 1, 365)
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

    fn test_tuf_signer() -> TufSigner {
        let keypair = KeyPair::from_seed(Seed::default());
        tuf_signer_from_parts(keypair.sk, keypair.pk)
    }

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
    fn test_tuf_signer_from_parts() {
        let keypair = KeyPair::from_seed(Seed::default());
        let signer = tuf_signer_from_parts(keypair.sk.clone(), keypair.pk);

        assert_eq!(signer.public_key_hex, hex_encode(keypair.pk.as_ref()));
        assert_eq!(signer.key_id.len(), 64); // SHA-256 = 32 bytes = 64 hex chars

        // Sign and verify
        let data = b"test message";
        let sig_bytes = (signer.sign_fn)(data).unwrap();
        assert_eq!(sig_bytes.len(), 64);
        let sig = ed25519_compact::Signature::from_slice(&sig_bytes).unwrap();
        assert!(keypair.pk.verify(data, &sig).is_ok());
    }

    #[test]
    fn test_generate_root_json_valid() {
        let signer = test_tuf_signer();
        let root_json = generate_root_json(&signer).unwrap();

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
        let signer = test_tuf_signer();
        let root_json = generate_root_json(&signer).unwrap();
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
        let signer = test_tuf_signer();
        let id1 = signer.key_id.clone();
        let id2 = compute_key_id_from_hex(&signer.public_key_hex);
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
        let signer = resolve_signing_key(None, tmp.path()).unwrap();
        assert_eq!(signer.public_key_hex.len(), 64);
        assert_eq!(signer.key_id.len(), 64);
    }

    #[test]
    fn test_resolve_content_key_fallback_to_auto() {
        let tmp = TempDir::new().unwrap();
        // First call resolve_signing_key to create the auto key
        let signer = resolve_signing_key(None, tmp.path()).unwrap();
        // content_key falls back to same auto key (both None)
        let content_signer = resolve_content_key(None, None, tmp.path()).unwrap();
        // Same key id and public key
        assert_eq!(signer.public_key_hex, content_signer.public_key_hex);
        assert_eq!(signer.key_id, content_signer.key_id);
    }

    #[test]
    fn test_generate_multi_key_root_json_same_key() {
        let signer = test_tuf_signer();
        // Same key for both roles
        let root_json =
            generate_multi_key_root_json(&signer, &signer.public_key_hex, 1, 365).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();
        let keys = parsed["signed"]["keys"].as_object().unwrap();
        // Only one key entry when root_key == server_key
        assert_eq!(keys.len(), 1);
        let roles = &parsed["signed"]["roles"];
        let root_keyid = roles["root"]["keyids"][0].as_str().unwrap();
        let targets_keyid = roles["targets"]["keyids"][0].as_str().unwrap();
        assert_eq!(root_keyid, targets_keyid);
    }

    #[test]
    fn test_generate_multi_key_root_json_different_keys() {
        let signer = test_tuf_signer();
        // Generate a different server key
        let server_kp = KeyPair::from_seed(Seed::from([99u8; 32]));
        let server_hex = hex_encode(server_kp.pk.as_ref());

        let root_json = generate_multi_key_root_json(&signer, &server_hex, 1, 365).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();
        let keys = parsed["signed"]["keys"].as_object().unwrap();
        // Two distinct key entries
        assert_eq!(keys.len(), 2);
        let roles = &parsed["signed"]["roles"];
        let root_keyid = roles["root"]["keyids"][0].as_str().unwrap();
        let targets_keyid = roles["targets"]["keyids"][0].as_str().unwrap();
        assert_ne!(root_keyid, targets_keyid);
    }

    #[test]
    fn test_generated_root_json_parseable_by_tough() {
        let signer = test_tuf_signer();
        let root_json = generate_root_json(&signer).unwrap();

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
        for key in root.keys.values() {
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
