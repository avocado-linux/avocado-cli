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

    #[test]
    fn test_jcs_encode_matches_known_vectors() {
        // RFC 8785 style: empty object
        let value: serde_json::Value = serde_json::json!({});
        assert_eq!(jcs_encode(&value), "{}");

        // RFC 8785 style: empty array
        let value: serde_json::Value = serde_json::json!([]);
        assert_eq!(jcs_encode(&value), "[]");

        // Typical TUF root signed portion structure
        let value: serde_json::Value = serde_json::json!({
            "_type": "root",
            "version": 1,
            "spec_version": "1.0.0",
            "keys": {},
            "roles": {}
        });
        let canonical = jcs_encode(&value);
        // Keys must be sorted: _type < keys < roles < spec_version < version
        assert_eq!(
            canonical,
            r#"{"_type":"root","keys":{},"roles":{},"spec_version":"1.0.0","version":1}"#
        );

        // Boolean values
        let value: serde_json::Value = serde_json::json!({"b": true, "a": false});
        assert_eq!(jcs_encode(&value), r#"{"a":false,"b":true}"#);

        // String escaping
        let value: serde_json::Value = serde_json::json!({"key": "hello \"world\""});
        assert_eq!(jcs_encode(&value), r#"{"key":"hello \"world\""}"#);
    }

    #[test]
    fn test_compute_tuf_key_id_known_vector() {
        // For a known public hex, verify the key ID is SHA-256 of the canonical key descriptor.
        // The canonical form is: {"keytype":"ed25519","keyval":{"public":"<hex>"},"scheme":"ed25519"}
        let pub_hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let key_id = compute_tuf_key_id(pub_hex);

        // Manually compute expected: sha256 of the canonical descriptor
        use sha2::{Digest, Sha256};
        let canonical = format!(
            r#"{{"keytype":"ed25519","keyval":{{"public":"{}"}},"scheme":"ed25519"}}"#,
            pub_hex
        );
        let expected = hex_encode(&Sha256::digest(canonical.as_bytes()));
        assert_eq!(key_id, expected);

        // Different key should produce different ID
        let other_hex = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert_ne!(key_id, compute_tuf_key_id(other_hex));
    }
}
