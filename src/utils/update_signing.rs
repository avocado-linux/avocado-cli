use anyhow::{Context, Result};
use ed25519_compact::{KeyPair, PublicKey, SecretKey};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::signing_keys::{is_file_uri, is_pkcs11_uri, KeysRegistry};

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
/// Requires an explicit signing key configured in avocado.yaml.
/// Returns `None` if no key is configured (Level 0 — server manages signing).
/// Returns `Some(signer)` if a named key is found (Level 2 / Sideload).
pub fn resolve_signing_key(key_name: Option<&str>) -> Result<Option<TufSigner>> {
    match key_name {
        Some(name) => Ok(Some(load_tuf_signer_from_registry(name)?)),
        None => Ok(None),
    }
}

/// Resolve the TUF signer for delegated-targets content signing.
///
/// Priority: `content_key_name` → `signing_key_name` → None (Level 0).
/// Returns `None` if no key is configured (server manages content signing).
pub fn resolve_content_key(
    content_key_name: Option<&str>,
    signing_key_name: Option<&str>,
) -> Result<Option<TufSigner>> {
    match (content_key_name, signing_key_name) {
        (Some(name), _) => Ok(Some(load_tuf_signer_from_registry(name)?)),
        (None, Some(name)) => Ok(Some(load_tuf_signer_from_registry(name)?)),
        (None, None) => Ok(None),
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

/// Compute the TUF key ID from a hex-encoded public key.
/// Key ID = SHA-256 of the canonical JSON key descriptor.
pub fn compute_key_id_from_hex(public_key_hex: &str) -> String {
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

/// The build-time warning for a baked trust anchor that a plain, locally-signed
/// `avocado deploy` cannot satisfy, or `None` when a local deploy is accepted.
///
/// The inputs are the ones `avocado build` already resolves: whether a local
/// signing key is present, whether this is a Connect project (`connect.org`
/// set), and whether the Connect `server_key` equals the local signing key (by
/// TUF key id). Only the Level-2 split produces a warning here:
///
/// - Connect + signing key, server key differs from the local key: the anchor
///   trusts the server key for the update metadata roles (Level 2), so a
///   key-local deploy is rejected.
/// - Connect + signing key, server key equals the local key: the anchor
///   authorizes that one key for every role (the sideload self-override), so a
///   key-local deploy is accepted.
/// - Every other case (Level 0, sideload, or an auto-generated dev key) returns
///   `None`. The Level-0 server-managed setup is the normal Connect path, so
///   `build` describes it with an info line rather than a warning.
pub fn baked_anchor_deploy_warning(
    has_signing_key: bool,
    is_connect: bool,
    server_key_matches_local: bool,
) -> Option<&'static str> {
    match (has_signing_key, is_connect) {
        (true, true) if !server_key_matches_local => Some(
            "The baked trust anchor trusts the Connect server key for update \
             metadata, so a plain `avocado deploy` signed with your local key is \
             rejected by the device. Sign through Connect with \
             `avocado deploy --connect-sign`, or set `server_key` to this signing \
             key's own public hex to build a sideload-capable image.",
        ),
        _ => None,
    }
}

/// The deploy-time warning when `avocado deploy` is about to sign an update with
/// a local key, or `None` when a local signature is expected to be accepted.
///
/// This covers only the local-signing path (not `--connect-sign`). The inputs
/// describe the project setup for the runtime being deployed:
///
/// - A server key that differs from the local signing key: the device may hold a
///   server-key anchor from a Connect OTA that the local signature does not
///   satisfy. A server key that equals the local key collapses every role onto
///   that one key, so a local deploy is accepted and this returns `None`.
/// - A Connect project with no server key and no signing key (Level 0): `deploy`
///   signs with an auto-generated development key that a Connect device does not
///   trust.
pub fn local_deploy_signing_warning(
    has_server_key: bool,
    server_key_matches_local: bool,
    is_connect: bool,
    has_signing_key: bool,
) -> Option<&'static str> {
    if has_server_key && !server_key_matches_local {
        Some(
            "This project has a Connect server key configured. A locally-signed \
             deploy may be rejected by a device that has received a Connect OTA \
             update. Use `--connect-sign` to sign via Connect.",
        )
    } else if is_connect && !has_server_key && !has_signing_key {
        Some(
            "No project signing key is configured, so this deploy signs with an \
             auto-generated local development key. A Connect device trusts the \
             root.json it receives from Connect, not this local key, so it rejects \
             the update unless this key is the provisioned anchor.",
        )
    } else {
        None
    }
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

    fn test_tuf_signer() -> TufSigner {
        let keypair = KeyPair::from_seed(ed25519_compact::Seed::default());
        tuf_signer_from_parts(keypair.sk, keypair.pk)
    }

    #[test]
    fn test_tuf_signer_from_parts() {
        let keypair = KeyPair::from_seed(ed25519_compact::Seed::default());
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

        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();

        assert!(parsed.get("signatures").is_some());
        assert!(parsed.get("signed").is_some());

        let signed = &parsed["signed"];
        assert_eq!(signed["_type"], "root");
        assert_eq!(signed["spec_version"], "1.0.0");
        assert_eq!(signed["version"], 1);

        let roles = signed["roles"].as_object().unwrap();
        assert!(roles.contains_key("root"));
        assert!(roles.contains_key("targets"));
        assert!(roles.contains_key("snapshot"));
        assert!(roles.contains_key("timestamp"));

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

        let sig_hex = sig["sig"].as_str().unwrap();
        assert_eq!(sig_hex.len(), 128);
    }

    #[test]
    fn test_key_id_deterministic() {
        let signer = test_tuf_signer();
        let id1 = signer.key_id.clone();
        let id2 = compute_key_id_from_hex(&signer.public_key_hex);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 64);
    }

    #[test]
    fn test_canonical_key_json_format() {
        let keypair = KeyPair::from_seed(ed25519_compact::Seed::default());
        let canonical = canonical_key_json(&keypair.pk);

        let parsed: serde_json::Value = serde_json::from_str(&canonical).unwrap();
        assert_eq!(parsed["keytype"], "ed25519");
        assert_eq!(parsed["scheme"], "ed25519");
        assert!(parsed["keyval"]["public"].as_str().unwrap().len() == 64);
    }

    #[test]
    fn test_resolve_signing_key_none_returns_none() {
        assert!(resolve_signing_key(None).unwrap().is_none());
    }

    #[test]
    fn test_resolve_content_key_none_returns_none() {
        assert!(resolve_content_key(None, None).unwrap().is_none());
    }

    #[test]
    fn test_generate_multi_key_root_json_same_key() {
        let signer = test_tuf_signer();
        let root_json =
            generate_multi_key_root_json(&signer, &signer.public_key_hex, 1, 365).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();
        let keys = parsed["signed"]["keys"].as_object().unwrap();
        assert_eq!(keys.len(), 1);
        let roles = &parsed["signed"]["roles"];
        let root_keyid = roles["root"]["keyids"][0].as_str().unwrap();
        let targets_keyid = roles["targets"]["keyids"][0].as_str().unwrap();
        assert_eq!(root_keyid, targets_keyid);
    }

    #[test]
    fn test_generate_multi_key_root_json_different_keys() {
        let signer = test_tuf_signer();
        let server_kp = KeyPair::from_seed(ed25519_compact::Seed::from([99u8; 32]));
        let server_hex = hex_encode(server_kp.pk.as_ref());

        let root_json = generate_multi_key_root_json(&signer, &server_hex, 1, 365).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&root_json).unwrap();
        let keys = parsed["signed"]["keys"].as_object().unwrap();
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

        let signed_root: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_str(&root_json)
                .expect("root.json should be parseable by tough::schema");

        let root = &signed_root.signed;
        assert_eq!(root.version.get(), 1);
        assert_eq!(root.spec_version, "1.0.0");
        assert!(!root.consistent_snapshot);
        assert_eq!(root.keys.len(), 1);
        assert_eq!(root.roles.len(), 4);

        for key in root.keys.values() {
            assert!(matches!(key, tough::schema::key::Key::Ed25519 { .. }));
        }

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

    // --- build-time anchor/deploy mismatch warnings (ENG-2802) ---

    #[test]
    fn test_baked_anchor_level_2_differing_key_warns() {
        // Connect + signing key, server key differs from the local key: the
        // anchor trusts the server key, so a key-local deploy is rejected.
        let warning = baked_anchor_deploy_warning(true, true, false).expect("Level 2 warns");
        assert!(warning.contains("--connect-sign"));
        assert!(warning.contains("server_key"));
        assert!(!warning.contains('—'));
    }

    #[test]
    fn test_baked_anchor_level_2_matching_key_is_silent() {
        // Connect + signing key, server key equals the local key: the anchor
        // authorizes that one key for every role, so a plain deploy is accepted.
        assert!(baked_anchor_deploy_warning(true, true, true).is_none());
    }

    #[test]
    fn test_baked_anchor_level_0_is_silent() {
        // Connect with no local signing key is the normal server-managed path.
        // `build` describes it with an info line, not a warning, so the helper
        // returns None regardless of the server-key match flag.
        for server_key_matches_local in [false, true] {
            assert!(baked_anchor_deploy_warning(false, true, server_key_matches_local).is_none());
        }
    }

    #[test]
    fn test_baked_anchor_sideload_is_silent() {
        // A single local key signs every role; a plain deploy with it is accepted.
        assert!(baked_anchor_deploy_warning(true, false, false).is_none());
        // No key and not Connect: build auto-generates a dev key, then behaves
        // as sideload.
        assert!(baked_anchor_deploy_warning(false, false, false).is_none());
    }

    // --- deploy-time local-signing warnings (ENG-2802) ---

    #[test]
    fn test_local_deploy_warns_when_server_key_differs() {
        // A configured server key that differs from the local key: the device
        // may hold a server-key anchor the local signature does not satisfy.
        let warning = local_deploy_signing_warning(true, false, true, true)
            .expect("differing server key warns");
        assert!(warning.contains("--connect-sign"));
        assert!(!warning.contains('—'));
    }

    #[test]
    fn test_local_deploy_silent_when_server_key_matches() {
        // Server key equals the local key: every role collapses onto one key,
        // so a local deploy is accepted. Matches the build side.
        assert!(local_deploy_signing_warning(true, true, true, true).is_none());
    }

    #[test]
    fn test_local_deploy_warns_on_level_0() {
        // Connect project, no server key, no signing key: `deploy` local-signs
        // with an auto-generated dev key the device does not trust.
        let warning =
            local_deploy_signing_warning(false, false, true, false).expect("Level 0 warns");
        assert!(warning.contains("development key"));
        assert!(warning.contains("Connect"));
        assert!(!warning.contains('—'));
    }

    #[test]
    fn test_local_deploy_silent_for_sideload() {
        // Not a Connect project: a plain sideload deploy is accepted, with or
        // without a local signing key.
        assert!(local_deploy_signing_warning(false, false, false, true).is_none());
        assert!(local_deploy_signing_warning(false, false, false, false).is_none());
    }
}
