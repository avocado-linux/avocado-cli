//! Per-project TLS material and per-session token mint for Container Dev Mode
//! (task 3.6, design D2/D8).
//!
//! At `up` a session mints, in one shot:
//!
//! - a **per-project CA** and a **server leaf** signed by it. The leaf carries
//!   SANs `{runtime-name, 10.0.2.2, 127.0.0.1}` so the same certificate serves
//!   the native-Linux loopback path, the device loopback proxy, and the
//!   `10.0.2.2` avocado-vm guest-push path. `notBefore` is BACKDATED (not the
//!   generation instant): an RTC-less device that cold-boots believing it is the
//!   Unix epoch (or the firmware build date) must still fall inside the validity
//!   window (design D8, cert-lifecycle risk row).
//! - the two structurally distinct session tokens (design D2 split): the
//!   host-only Basic [`WriteToken`] and the device-delivered Bearer
//!   [`ReadToken`].
//!
//! The [`ServerConfig`] is built from the leaf and serves the bulk-read and
//! control-WS listeners over TLS (bound by tasks 3.7 / 5.2).
//!
//! CA custody (design D8, threat model): the CA **private key** never leaves the
//! host. It is used only to sign the leaf and is then dropped — this session
//! never retains it — so it cannot be serialized into the bootstrap payload. The
//! device is delivered ONLY the CA certificate (via [`DevSession::bootstrap_payload`])
//! plus the read/control token; never the write token and never the CA key.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use base64::Engine as _;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, Ia5String, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use serde::Serialize;
use thiserror::Error;

use super::auth::{ReadToken, WriteToken};

/// The QEMU user-networking host alias a VM guest reaches the host by; the leaf
/// MUST carry this as an IP SAN or the `10.0.2.2` guest-push path fails cert
/// validation (design D2, macOS fast-path risk row).
pub const VM_HOST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Loopback address the native-Linux push path and the device-side loopback
/// proxy reach the registry by; carried as an IP SAN on the leaf.
pub const LOOPBACK_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Backdated `notBefore` (year, month, day). Far enough in the past that a
/// cold-booted RTC-less device's clock lands inside the validity window (D8).
const NOT_BEFORE_YMD: (i32, u8, u8) = (2000, 1, 1);

/// `notAfter` for the long-lived per-project CA and leaf (D8).
const NOT_AFTER_YMD: (i32, u8, u8) = (2100, 1, 1);

/// Entropy for each minted token, in bytes (256 bits).
const TOKEN_BYTES: usize = 32;

/// Errors returned while minting TLS material or tokens.
#[derive(Debug, Error)]
pub enum TlsError {
    /// Key/certificate generation via rcgen failed.
    #[error("failed to generate container-dev TLS material: {0}")]
    Rcgen(#[from] rcgen::Error),
    /// Building the rustls server config from the leaf failed (e.g. the private
    /// key did not match the certificate).
    #[error("failed to build the container-dev rustls server config: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Host-side TLS material for a dev session.
///
/// Holds the CA **certificate** (PEM, for device/VM delivery) and the rustls
/// [`ServerConfig`] backed by the CA-signed leaf. The CA **private key** is
/// deliberately absent: it is dropped after the leaf is signed, so it cannot be
/// serialized anywhere (design D8).
pub struct TlsMaterial {
    ca_cert_pem: String,
    server_config: Arc<ServerConfig>,
}

impl TlsMaterial {
    /// Generate a per-project CA, a CA-signed server leaf carrying the
    /// `{runtime-name, 10.0.2.2, 127.0.0.1}` SANs and a backdated `notBefore`,
    /// and the rustls server config that serves TLS with the leaf.
    pub fn generate(runtime_name: &str) -> Result<Self, TlsError> {
        let chain = CertChain::build(runtime_name)?;

        let cert_der = chain.leaf_cert.der().clone();
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(chain.leaf_key.serialize_der()));
        // `with_single_cert` fails unless the key matches the leaf's public key,
        // so a successful build is evidence the leaf and its key are consistent.
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;

        // The CA key (chain.ca_key) is dropped here with `chain`: nothing retains
        // it past leaf signing, so it can never reach a payload (D8).
        Ok(Self {
            ca_cert_pem: chain.ca_cert_pem,
            server_config: Arc::new(server_config),
        })
    }

    /// The CA certificate in PEM form — the ONLY CA material delivered to a
    /// device or VM (design D8).
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// The rustls server config serving the read/bulk/WS listeners with the leaf.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.server_config)
    }
}

/// A minted dev session: TLS material plus the two D2 tokens.
pub struct DevSession {
    /// Per-project CA cert + leaf-backed server config.
    pub tls: TlsMaterial,
    /// Host-only Basic write token (never delivered to a device).
    pub write_token: WriteToken,
    /// Device-delivered Bearer read/control token.
    pub read_token: ReadToken,
}

impl DevSession {
    /// Mint fresh TLS material and both tokens for a runtime named `runtime_name`.
    ///
    /// Called once per `up`; the write token rotates hard and the read/control
    /// token is what the bootstrap payload delivers to the device (design D5;
    /// rotation orchestration lives in task 5.2).
    pub fn mint(runtime_name: &str) -> Result<Self, TlsError> {
        Ok(Self {
            tls: TlsMaterial::generate(runtime_name)?,
            write_token: WriteToken::new(mint_token()),
            read_token: ReadToken::new(mint_token()),
        })
    }

    /// The device-delivery payload: the CA certificate and the read/control
    /// token, and nothing else.
    ///
    /// By construction it carries neither the CA private key (which this session
    /// never retains) nor the host-only write token — the two things design D8 /
    /// D2 forbid ever reaching a device. Task 5.2 writes this to the device
    /// writable partition (adding the resolved host endpoint); it owns the file
    /// path and endpoint resolution, this owns the field set.
    pub fn bootstrap_payload(&self) -> BootstrapPayload {
        BootstrapPayload {
            ca_cert_pem: self.tls.ca_cert_pem().to_string(),
            read_token: self.read_token.secret().to_string(),
        }
    }
}

/// The device-delivery subset of a session, serialized into the bootstrap
/// payload written to the device writable partition (task 5.2).
///
/// Deliberately holds no field for the CA private key or the write token, so a
/// serialization can never leak either (design D8 / D2).
#[derive(Debug, Serialize)]
pub struct BootstrapPayload {
    /// The per-project CA certificate the device pins the host TLS leaf against.
    pub ca_cert_pem: String,
    /// The Bearer read/control token the device authenticates pulls and the
    /// control WS with.
    pub read_token: String,
}

/// A freshly generated CA + CA-signed leaf and their keys, held only long enough
/// to build the server config; the CA key is dropped with this value.
struct CertChain {
    leaf_cert: rcgen::Certificate,
    leaf_key: KeyPair,
    ca_cert_pem: String,
    // The CA key is intentionally NOT a field: it is consumed by `signed_by`
    // inside `build` and never escapes, so it cannot be retained or serialized.
}

impl CertChain {
    fn build(runtime_name: &str) -> Result<Self, TlsError> {
        let not_before = rcgen::date_time_ymd(NOT_BEFORE_YMD.0, NOT_BEFORE_YMD.1, NOT_BEFORE_YMD.2);
        let not_after = rcgen::date_time_ymd(NOT_AFTER_YMD.0, NOT_AFTER_YMD.1, NOT_AFTER_YMD.2);

        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.not_before = not_before;
        ca_params.not_after = not_after;
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params.distinguished_name.push(
            DnType::CommonName,
            format!("avocado container-dev CA ({runtime_name})"),
        );
        let ca_key = KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key)?;

        let mut leaf_params = CertificateParams::new(Vec::<String>::new())?;
        leaf_params.not_before = not_before;
        leaf_params.not_after = not_after;
        leaf_params.subject_alt_names = vec![
            SanType::DnsName(Ia5String::try_from(runtime_name)?),
            SanType::IpAddress(IpAddr::V4(VM_HOST_IP)),
            SanType::IpAddress(IpAddr::V4(LOOPBACK_IP)),
        ];
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, runtime_name.to_string());
        let leaf_key = KeyPair::generate()?;
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key)?;

        Ok(Self {
            leaf_cert,
            leaf_key,
            ca_cert_pem: ca_cert.pem(),
        })
    }
}

/// Mint one URL-safe base64 token from [`TOKEN_BYTES`] of randomness.
fn mint_token() -> String {
    use rand::RngExt;
    let bytes: [u8; TOKEN_BYTES] = rand::rng().random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const RUNTIME: &str = "dev-runtime";

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_secs() as i64
    }

    #[test]
    fn leaf_carries_the_10_0_2_2_ip_san_and_loopback_and_runtime_name() {
        let chain = CertChain::build(RUNTIME).expect("cert chain builds");
        let sans = &chain.leaf_cert.params().subject_alt_names;

        assert!(
            sans.contains(&SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)))),
            "the leaf MUST carry the 10.0.2.2 IP SAN (VM guest-push path), got {sans:?}"
        );
        assert!(
            sans.contains(&SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))),
            "the leaf MUST carry the 127.0.0.1 IP SAN (loopback path), got {sans:?}"
        );
        assert!(
            sans.contains(&SanType::DnsName(
                Ia5String::try_from(RUNTIME).expect("runtime name is a valid DNS SAN")
            )),
            "the leaf MUST carry the runtime-name DNS SAN, got {sans:?}"
        );
    }

    #[test]
    fn not_before_is_backdated_strictly_before_now() {
        let chain = CertChain::build(RUNTIME).expect("cert chain builds");
        let now = now_unix();

        let leaf_not_before = chain.leaf_cert.params().not_before.unix_timestamp();
        assert!(
            leaf_not_before < now,
            "leaf notBefore ({leaf_not_before}) must be backdated strictly before now ({now}), \
             not set to generation time"
        );
        assert!(
            chain.leaf_cert.params().not_before < chain.leaf_cert.params().not_after,
            "leaf notBefore must precede notAfter"
        );
    }

    #[test]
    fn both_tokens_are_non_empty_and_distinct() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        assert!(
            !session.write_token.secret().is_empty(),
            "the write token must be non-empty"
        );
        assert!(
            !session.read_token.secret().is_empty(),
            "the read/control token must be non-empty"
        );
        assert_ne!(
            session.write_token.secret(),
            session.read_token.secret(),
            "the write and read/control tokens must be distinct secrets"
        );
    }

    #[test]
    fn each_mint_produces_fresh_tokens() {
        let a = DevSession::mint(RUNTIME).expect("first session mints");
        let b = DevSession::mint(RUNTIME).expect("second session mints");
        assert_ne!(
            a.read_token.secret(),
            b.read_token.secret(),
            "the read/control token must rotate across mints"
        );
        assert_ne!(
            a.write_token.secret(),
            b.write_token.secret(),
            "the write token must rotate across mints"
        );
    }

    #[test]
    fn bootstrap_payload_carries_the_ca_cert_but_not_the_ca_private_key() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let payload = session.bootstrap_payload();
        let json = serde_json::to_string(&payload).expect("payload serializes");

        assert!(
            json.contains("BEGIN CERTIFICATE"),
            "the bootstrap payload must deliver the CA certificate"
        );
        assert!(
            !json.contains("PRIVATE KEY"),
            "the bootstrap payload must NOT contain any private key material (D8)"
        );
        assert!(
            json.contains(session.read_token.secret()),
            "the bootstrap payload must deliver the read/control token"
        );
        assert!(
            !json.contains(session.write_token.secret()),
            "the bootstrap payload must NEVER contain the host-only write token (D2)"
        );
    }

    #[test]
    fn payload_ca_cert_matches_the_session_ca_cert() {
        let session = DevSession::mint(RUNTIME).expect("session mints");
        assert_eq!(
            session.bootstrap_payload().ca_cert_pem,
            session.tls.ca_cert_pem(),
            "the delivered CA cert must be the session's CA cert"
        );
    }

    #[test]
    fn mint_builds_a_server_config_from_the_leaf() {
        // A successful mint means `with_single_cert` accepted the leaf and its
        // key, i.e. the rustls server config is backed by the CA-signed leaf.
        let session = DevSession::mint(RUNTIME).expect("session mints");
        let _config = session.tls.server_config();
        assert!(
            session.tls.ca_cert_pem().contains("BEGIN CERTIFICATE"),
            "the CA cert must be retained in PEM form for delivery"
        );
    }
}
