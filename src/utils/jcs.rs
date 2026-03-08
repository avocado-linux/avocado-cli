//! JCS (JSON Canonicalization Scheme, RFC 8785) encoder and TUF key ID computation.
//!
//! Produces identical canonical JSON as the server's `Signer.jcs_encode/1`,
//! which is critical for signature verification.

use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Encode bytes as lowercase hex string.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// JCS-canonicalize a serde_json::Value.
///
/// Objects have keys sorted lexicographically; arrays preserve order;
/// strings/numbers/bools use standard JSON serialization.
/// TUF metadata never contains floats or null.
pub fn jcs_encode(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let pairs: Vec<String> = keys
                .iter()
                .map(|k| {
                    let key_json = serde_json::to_string(k).unwrap();
                    let val_json = jcs_encode(map.get(*k).unwrap());
                    format!("{}:{}", key_json, val_json)
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(jcs_encode).collect();
            format!("[{}]", items.join(","))
        }
        _ => serde_json::to_string(value).unwrap(),
    }
}

/// Compute the TUF key ID for an ed25519 public key.
///
/// Must match the server's `Signer.compute_key_id/1`:
/// `sha256_hex({"keytype":"ed25519","keyval":{"public":"<hex>"},"scheme":"ed25519"})`
///
/// NOTE: This is different from `signing_keys::generate_keyid` which hashes
/// the raw public key bytes. The TUF key ID hashes the canonical key descriptor.
pub fn compute_tuf_key_id(public_hex: &str) -> String {
    let canonical = format!(
        r#"{{"keytype":"ed25519","keyval":{{"public":"{}"}},"scheme":"ed25519"}}"#,
        public_hex
    );
    let hash = Sha256::digest(canonical.as_bytes());
    hex_encode(&hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jcs_sorts_object_keys() {
        let value: serde_json::Value = serde_json::json!({
            "z": 1,
            "a": 2,
            "m": 3
        });
        assert_eq!(jcs_encode(&value), r#"{"a":2,"m":3,"z":1}"#);
    }

    #[test]
    fn test_jcs_nested_objects() {
        let value: serde_json::Value = serde_json::json!({
            "b": {"d": 1, "c": 2},
            "a": 3
        });
        assert_eq!(jcs_encode(&value), r#"{"a":3,"b":{"c":2,"d":1}}"#);
    }

    #[test]
    fn test_jcs_array_preserves_order() {
        let value: serde_json::Value = serde_json::json!([3, 1, 2]);
        assert_eq!(jcs_encode(&value), "[3,1,2]");
    }

    #[test]
    fn test_compute_tuf_key_id() {
        // The key descriptor for a known public hex should produce a deterministic key ID
        let pub_hex = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let key_id = compute_tuf_key_id(pub_hex);
        assert_eq!(key_id.len(), 64); // SHA-256 hex = 64 chars

        // Same input always produces same output
        assert_eq!(key_id, compute_tuf_key_id(pub_hex));
    }
}
