//! Authentication for the Container Dev Mode registry.
//!
//! Two credential *types* exist, structurally distinct (design D2):
//!
//! - the host-only WRITE token, presented as an HTTP **Basic** credential
//!   (fixed username, password = the write token) on the write listener;
//! - the READ/CONTROL token, a **Bearer** value delivered to devices (task 3.4).
//!
//! This module owns only the write-side Basic validator (task 3.3). The
//! read/control Bearer validator lands in task 3.4. The validator here is also
//! what REJECTS a Bearer credential presented on a write route: a Bearer scheme
//! is not Basic, so it never satisfies [`basic_write_is_valid`].

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine as _;

/// Fixed Basic-auth username paired with the write token as the password.
///
/// docker/podman inject a `user:password` pair on push; the username carries no
/// authority (only the password — the write token — is checked) but it must be
/// a fixed, known value so the injected credential form is deterministic.
pub const WRITE_USERNAME: &str = "avocado";

/// Realm advertised in the write listener's `WWW-Authenticate: Basic` challenge.
const WRITE_REALM: &str = "avocado-container-dev";

/// The host-only write token gating every write route (design D2).
///
/// Presented by the engine push as an HTTP Basic password. It never leaves the
/// host and is never delivered to a device.
#[derive(Clone)]
pub struct WriteToken(Arc<String>);

impl WriteToken {
    /// Wrap a freshly minted write token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(Arc::new(token.into()))
    }

    /// The raw token value, for host-side comparison only. Never logged.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Whether `header_value` is a Basic credential whose username is
/// [`WRITE_USERNAME`] and whose password equals `expected_token`.
///
/// Returns `false` for an absent header, a non-Basic scheme (e.g. the Bearer
/// read/control token), undecodable base64, a missing `:` separator, a wrong
/// username, or a wrong password. This is the entire accept predicate for a
/// write route.
///
/// The password comparison is a plain byte equality, not constant-time: the
/// threat model scopes the write listener to loopback on native Linux (or a
/// routable HTTPS listener never disclosed to a device) on a single-developer
/// host, so a timing side channel is not in scope (design D2, threat-model
/// residual assumption).
pub fn basic_write_is_valid(header_value: Option<&str>, expected_token: &str) -> bool {
    let Some(raw) = header_value else {
        return false;
    };
    // The scheme must be Basic (case-insensitive per RFC 7617); a Bearer
    // read/control token is rejected right here.
    let Some(encoded) = scheme_payload(raw, "basic") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(pair) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((user, pass)) = pair.split_once(':') else {
        return false;
    };
    user == WRITE_USERNAME && pass == expected_token
}

/// Split an `Authorization` header into its scheme and payload, returning the
/// trimmed payload only when the scheme matches `scheme` case-insensitively.
fn scheme_payload<'a>(header: &'a str, scheme: &str) -> Option<&'a str> {
    let (got, rest) = header.split_once(' ')?;
    got.eq_ignore_ascii_case(scheme).then(|| rest.trim())
}

/// axum middleware gating every write route on a valid Basic write credential.
///
/// On failure it returns `401 Unauthorized` with a `WWW-Authenticate: Basic`
/// challenge — never a Bearer challenge (design D2/L-2): issuing a Basic
/// challenge is what makes docker/podman send a Basic credential on push.
pub async fn require_basic_write(
    State(token): State<WriteToken>,
    request: Request,
    next: Next,
) -> Response {
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if basic_write_is_valid(header, token.secret()) {
        next.run(request).await
    } else {
        write_unauthorized()
    }
}

/// A `401` carrying the Basic challenge for the write path.
fn write_unauthorized() -> Response {
    let body = serde_json::json!({
        "errors": [{ "code": "UNAUTHORIZED", "message": "write token required" }]
    })
    .to_string();
    (
        StatusCode::UNAUTHORIZED,
        [
            (
                header::WWW_AUTHENTICATE,
                format!("Basic realm=\"{WRITE_REALM}\""),
            ),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a `user:pass` pair as a Basic `Authorization` header value.
    fn basic(user: &str, pass: &str) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {encoded}")
    }

    #[test]
    fn correct_username_and_password_are_accepted() {
        let header = basic(WRITE_USERNAME, "s3cret");
        assert!(basic_write_is_valid(Some(&header), "s3cret"));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let header = basic(WRITE_USERNAME, "wrong");
        assert!(!basic_write_is_valid(Some(&header), "s3cret"));
    }

    #[test]
    fn wrong_username_is_rejected() {
        let header = basic("intruder", "s3cret");
        assert!(!basic_write_is_valid(Some(&header), "s3cret"));
    }

    #[test]
    fn a_bearer_token_is_not_a_basic_credential() {
        // Even if the Bearer value equals the write token, the scheme is wrong.
        let header = "Bearer s3cret";
        assert!(!basic_write_is_valid(Some(header), "s3cret"));
    }

    #[test]
    fn absent_header_is_rejected() {
        assert!(!basic_write_is_valid(None, "s3cret"));
    }

    #[test]
    fn undecodable_base64_is_rejected() {
        assert!(!basic_write_is_valid(
            Some("Basic !!!not-base64!!!"),
            "s3cret"
        ));
    }

    #[test]
    fn a_credential_without_a_colon_is_rejected() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("no-colon-here");
        let header = format!("Basic {encoded}");
        assert!(!basic_write_is_valid(Some(&header), "s3cret"));
    }
}
