//! Authentication for the Container Dev Mode registry.
//!
//! Two credential *types* exist, structurally distinct (design D2):
//!
//! - the host-only WRITE token, presented as an HTTP **Basic** credential
//!   (fixed username, password = the write token) on the write listener;
//! - the READ/CONTROL token, a **Bearer** value delivered to devices (task 3.4).
//!
//! This module owns both the write-side Basic validator (task 3.3) and the
//! read/control Bearer validator (task 3.4). The write validator also REJECTS a
//! Bearer credential presented on a write route (a Bearer scheme is not Basic,
//! so it never satisfies [`basic_write_is_valid`]); the read validator likewise
//! rejects the Basic write token on a read route (M-2). The read/control token
//! is authorized through ONE seam ([`read_request_authorized`]) that both the
//! bulk read listener ([`require_bearer_read`]) and the control-WS upgrade (task
//! 5.1) call, so the WS is not a second, separately-implemented auth surface
//! (G-5).

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
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

/// The per-session read/control token (design D2 split).
///
/// A **Bearer** value delivered to a device at bootstrap; it is the ONLY token
/// a device holds and authorizes both bulk pulls (the read listener) and the
/// control-WS upgrade (task 5.1). Structurally distinct from the Basic
/// [`WriteToken`]: a Bearer credential can never satisfy a write route, and the
/// Basic write token can never satisfy a read route (M-2).
#[derive(Clone)]
pub struct ReadToken(Arc<String>);

impl ReadToken {
    /// Wrap a freshly minted read/control token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(Arc::new(token.into()))
    }

    /// The raw token value, for host-side comparison only. Never logged.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Whether `header_value` is a Bearer credential whose token equals
/// `expected_token`.
///
/// Returns `false` for an absent header, a non-Bearer scheme (crucially the
/// Basic write token, which is rejected on a read route per M-2), or a wrong
/// token. This is the entire accept predicate for a read/control route.
///
/// The comparison is a plain byte equality, not constant-time: the read
/// listener is served over TLS to a device on a single-developer host, so a
/// timing side channel is out of scope (design D2, threat-model residual
/// assumption), matching [`basic_write_is_valid`].
pub fn bearer_read_is_valid(header_value: Option<&str>, expected_token: &str) -> bool {
    let Some(raw) = header_value else {
        return false;
    };
    // The scheme must be Bearer (case-insensitive per RFC 6750); a Basic write
    // credential is rejected right here (M-2).
    let Some(token) = scheme_payload(raw, "bearer") else {
        return false;
    };
    token == expected_token
}

/// Authorize a request against the read/control `token` by reading its
/// `Authorization` header.
///
/// This is the ONE seam both the bulk read listener ([`require_bearer_read`])
/// and the control-WS upgrade (task 5.1) call, so the two auth surfaces cannot
/// diverge (G-5). A WebSocket upgrade is an HTTP `GET` carrying the same
/// `Authorization` header, so the upgrade handler authorizes through this exact
/// function rather than re-implementing the check.
pub fn read_request_authorized(headers: &HeaderMap, token: &ReadToken) -> bool {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    bearer_read_is_valid(header, token.secret())
}

/// axum middleware gating every bulk read route on a valid Bearer read/control
/// credential, delegating to the shared [`read_request_authorized`] seam.
///
/// On failure it returns `401 Unauthorized` with a BARE `Bearer` challenge —
/// no `realm` or token-endpoint parameters (design L-1): a token-endpoint
/// redirect would send a stray client off to a phantom auth server that does
/// not exist.
pub async fn require_bearer_read(
    State(token): State<ReadToken>,
    request: Request,
    next: Next,
) -> Response {
    if read_request_authorized(request.headers(), &token) {
        next.run(request).await
    } else {
        read_unauthorized()
    }
}

/// A `401` carrying a BARE `Bearer` challenge for the read path (design L-1).
///
/// The challenge is the single word `Bearer` with no `realm`/token-endpoint
/// parameters, so a client that stumbles onto the read listener is told the
/// scheme without being redirected to an auth server that does not exist.
fn read_unauthorized() -> Response {
    let body = serde_json::json!({
        "errors": [{ "code": "UNAUTHORIZED", "message": "read/control token required" }]
    })
    .to_string();
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer".to_string()),
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

    // ---- read/control Bearer validator (task 3.4) ----

    const READ_TOKEN: &str = "read-control-token";
    const A_WRITE_TOKEN: &str = "write-token-secret";

    use axum::{middleware, routing::get, routing::put, Router};

    /// A trivial handler standing in for a real read route or write route; the
    /// auth middleware runs before it, so reaching it means the request passed.
    async fn ok() -> &'static str {
        "ok"
    }

    /// Serve a Bearer-gated read router (the bulk-listener shape) over
    /// [`require_bearer_read`]; return its base URL.
    async fn spawn_read(token: &str) -> String {
        let app = Router::new()
            .route("/v2/", get(ok))
            .route("/v2/{*rest}", get(ok))
            .layer(middleware::from_fn_with_state(
                ReadToken::new(token),
                require_bearer_read,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Serve a Basic-gated write route over [`require_basic_write`]; return its
    /// base URL. Used to assert the Bearer read/control token is refused here.
    async fn spawn_write(token: &str) -> String {
        let app =
            Router::new()
                .route("/v2/{*rest}", put(ok))
                .layer(middleware::from_fn_with_state(
                    WriteToken::new(token),
                    require_basic_write,
                ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Build a `HeaderMap` carrying a single `Authorization` header.
    fn headers_with(auth: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth.parse().unwrap());
        headers
    }

    /// The same `Authorization` header a WebSocket upgrade carries: a `GET` with
    /// the `Connection: Upgrade` / `Upgrade: websocket` handshake headers added.
    fn ws_upgrade_headers(auth: &str) -> HeaderMap {
        let mut headers = headers_with(auth);
        headers.insert(header::CONNECTION, "Upgrade".parse().unwrap());
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());
        headers
    }

    #[test]
    fn correct_bearer_read_token_is_accepted() {
        let header = format!("Bearer {READ_TOKEN}");
        assert!(bearer_read_is_valid(Some(&header), READ_TOKEN));
    }

    #[test]
    fn wrong_bearer_read_token_is_rejected() {
        assert!(!bearer_read_is_valid(
            Some("Bearer not-the-token"),
            READ_TOKEN
        ));
    }

    #[test]
    fn absent_header_is_rejected_on_a_read_route() {
        assert!(!bearer_read_is_valid(None, READ_TOKEN));
    }

    #[test]
    fn a_basic_credential_is_not_a_bearer_read_token() {
        // M-2: even if the Basic password equals the read token, the scheme is
        // Basic, so it cannot satisfy a read route.
        let header = basic(WRITE_USERNAME, READ_TOKEN);
        assert!(!bearer_read_is_valid(Some(&header), READ_TOKEN));
    }

    #[test]
    fn bearer_scheme_matching_is_case_insensitive() {
        let header = format!("bearer {READ_TOKEN}");
        assert!(bearer_read_is_valid(Some(&header), READ_TOKEN));
    }

    #[test]
    fn bulk_and_ws_upgrade_authorize_through_the_same_seam() {
        let token = ReadToken::new(READ_TOKEN);
        let good = format!("Bearer {READ_TOKEN}");
        // A Basic credential is the write token's transport form.
        let write_basic = basic(WRITE_USERNAME, A_WRITE_TOKEN);

        // A bulk GET and a WS upgrade carrying the SAME credential get the SAME
        // decision because both authorize through read_request_authorized (G-5);
        // the WS upgrade (task 5.1) is not a divergent auth surface.
        assert!(read_request_authorized(&headers_with(&good), &token));
        assert!(read_request_authorized(&ws_upgrade_headers(&good), &token));
        assert!(!read_request_authorized(
            &headers_with(&write_basic),
            &token
        ));
        assert!(!read_request_authorized(
            &ws_upgrade_headers(&write_basic),
            &token
        ));
    }

    #[tokio::test]
    async fn a_read_request_without_the_token_is_rejected_with_a_bare_bearer_challenge() {
        let base = spawn_read(READ_TOKEN).await;
        let resp = reqwest::get(format!("{base}/v2/")).await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "an unauthenticated read must be refused"
        );
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        // A bare `Bearer` challenge: exactly the scheme, no realm/token-endpoint
        // redirect that would send a stray client to a phantom auth server (L-1).
        assert_eq!(
            challenge.trim(),
            "Bearer",
            "the read challenge must be a bare Bearer, got {challenge:?}"
        );
        assert!(
            !challenge.to_ascii_lowercase().contains("realm"),
            "the read challenge must not carry a realm/token-endpoint redirect"
        );
    }

    #[tokio::test]
    async fn the_basic_write_token_is_rejected_on_a_read_route() {
        let base = spawn_read(READ_TOKEN).await;
        // The write token presented in its Basic transport form on the read
        // listener must be refused (M-2 — read routes accept only Bearer).
        let resp = reqwest::Client::new()
            .get(format!("{base}/v2/my-app/blobs/sha256:aa"))
            .basic_auth(WRITE_USERNAME, Some(A_WRITE_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "a Basic write credential must not authorize a read route"
        );
    }

    #[tokio::test]
    async fn a_valid_bearer_read_token_is_accepted_on_a_read_route() {
        let base = spawn_read(READ_TOKEN).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/v2/"))
            .bearer_auth(READ_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "a valid Bearer read/control token must be accepted"
        );
    }

    #[tokio::test]
    async fn the_bearer_read_token_is_rejected_on_a_write_route() {
        let base = spawn_write(A_WRITE_TOKEN).await;
        // The device-held Bearer read/control token must never authorize a write
        // — even when its value equals the write token's secret.
        let resp = reqwest::Client::new()
            .put(format!("{base}/v2/my-app/manifests/dev"))
            .bearer_auth(A_WRITE_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "the Bearer read/control token must not authorize a write"
        );
    }
}
