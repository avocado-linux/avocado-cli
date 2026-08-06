use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::commands::connect::client::{
    self, ConnectClient, ConnectConfig, OrgInfo, Profile, ProfileUser,
};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::output_format::{emit_json_event, emit_json_object};

/// Re-export so existing main.rs `use commands::connect::auth::OutputFormat` keeps working.
/// New code should prefer `crate::utils::output_format::OutputFormat` directly.
pub use crate::utils::output_format::OutputFormat;

pub struct ConnectAuthLoginCommand {
    pub url: String,
    pub profile: Option<String>,
    pub token: Option<String>,
    pub org: Option<String>,
    pub output: OutputFormat,
}

impl ConnectAuthLoginCommand {
    pub fn new(
        url: Option<String>,
        profile: Option<String>,
        token: Option<String>,
        org: Option<String>,
        output: OutputFormat,
    ) -> Self {
        let url = url
            .or_else(|| std::env::var("AVOCADO_CONNECT_URL").ok())
            .unwrap_or_else(|| "https://connect.peridio.com".to_string());
        Self {
            url,
            profile,
            token,
            org,
            output,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.output.is_json() {
            print_info(&format!("Logging in to {}", self.url), OutputLevel::Normal);
        }

        let result = if let Some(ref token) = self.token {
            self.login_with_token(token).await
        } else {
            self.login_with_browser().await
        };

        if let Err(ref e) = result {
            if self.output.is_json() {
                emit_json_event(&serde_json::json!({
                    "event": "error",
                    "message": e.to_string(),
                }));
            }
        }

        result
    }

    /// Browser-based login: print URL, wait for callback, exchange code for token.
    async fn login_with_browser(&self) -> Result<()> {
        // Bind to a random port on localhost
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind local callback server")?;
        let port = listener.local_addr()?.port();

        // Generate a random state parameter for CSRF protection
        let state = generate_state();

        let login_url = format!("{}/cli/login?port={}&state={}", self.url, port, state);

        if self.output.is_json() {
            emit_json_event(&serde_json::json!({
                "event": "login_url",
                "url": login_url,
            }));
        } else {
            println!();
            println!("  Open this URL in your browser to log in:");
            println!();
            println!("  {login_url}");
            println!();
            print_info(
                "Waiting for authentication... (link expires in 5 minutes)",
                OutputLevel::Normal,
            );
        }

        // Wait for the browser to redirect to our local server
        let (code, received_state, mut stream) = wait_for_callback(&listener).await?;

        // Validate state to prevent CSRF — respond to browser BEFORE bailing
        if received_state != state {
            let error_url = format!("{}/cli/login?error=state_mismatch", self.url);
            respond_redirect(&mut stream, &error_url).await;
            anyhow::bail!("state mismatch — possible CSRF attack. please try again.");
        }

        let profile_name = self.profile.as_deref().unwrap_or("default");
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        let token_name = format!("avocado-cli-{hostname}-{profile_name}");

        // Resolve the org BEFORE exchange so the minted token is scoped
        // correctly on the server side. Phoenix.Token codes are stateless, so
        // verifying via list-orgs does not consume the code — the same code
        // is reused for the exchange call below.
        let chosen_org =
            match resolve_org_for_login(&self.url, &code, self.org.as_deref(), self.output).await {
                Ok(o) => o,
                Err(e) => {
                    let error_url = format!("{}/cli/login?error=org_resolution_failed", self.url);
                    respond_redirect(&mut stream, &error_url).await;
                    return Err(e);
                }
            };

        let mut exchange_body = serde_json::json!({
            "code": code,
            "cli_token_name": token_name,
        });
        if let Some(ref org) = chosen_org {
            exchange_body["organization_id"] = serde_json::Value::String(org.id.clone());
        }

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/auth/cli/exchange", self.url))
            .json(&exchange_body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let error_url = format!("{}/cli/login?error=exchange_failed", self.url);
                respond_redirect(&mut stream, &error_url).await;
                anyhow::bail!("failed to exchange auth code: {e}");
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = body["message"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("unknown error");
            let error_url = format!("{}/cli/login?error=exchange_failed", self.url);
            respond_redirect(&mut stream, &error_url).await;
            anyhow::bail!("code exchange failed ({status}): {msg}");
        }

        let body: serde_json::Value = resp.json().await?;
        let token = body["data"]["token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no token in exchange response"))?;
        let organization_id = body["data"]["organization_id"].as_str().map(str::to_string);
        let user_email = body["data"]["user"]["email"].as_str().unwrap_or("unknown");
        let user_name = body["data"]["user"]["name"].as_str().unwrap_or("unknown");

        // Exchange succeeded — NOW redirect browser to success page
        let success_url = format!("{}/cli/success", self.url);
        respond_redirect(&mut stream, &success_url).await;

        self.save_profile(profile_name, token, user_email, user_name, organization_id)?;

        Ok(())
    }

    /// Token-based login: validate the provided token and save it.
    async fn login_with_token(&self, token: &str) -> Result<()> {
        if !self.output.is_json() {
            print_info("Validating token...", OutputLevel::Normal);
        }

        let profile_name = self.profile.as_deref().unwrap_or("default");

        // Build a temporary profile to validate the token via GET /api/me
        let temp_profile = Profile {
            api_url: self.url.clone(),
            token: token.to_string(),
            user: ProfileUser {
                email: String::new(),
                name: String::new(),
            },
            created_at: Utc::now().to_rfc3339(),
            organization_id: None,
        };

        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        let token_name = format!("avocado-cli-{hostname}-{profile_name}");

        let temp_client = ConnectClient::from_profile(&temp_profile)?;
        let me = temp_client
            .get_me_full()
            .await
            .context("token validation failed — is the token valid?")?;

        let (final_token, organization_id) =
            provision_org_token(&temp_client, &me, token, &token_name, self.output).await?;

        self.save_profile(
            profile_name,
            &final_token,
            &me.user.email,
            &me.user.name,
            organization_id,
        )?;

        Ok(())
    }

    fn save_profile(
        &self,
        profile_name: &str,
        token: &str,
        email: &str,
        name: &str,
        organization_id: Option<String>,
    ) -> Result<()> {
        let profile = Profile {
            api_url: self.url.clone(),
            token: token.to_string(),
            user: ProfileUser {
                email: email.to_string(),
                name: name.to_string(),
            },
            created_at: Utc::now().to_rfc3339(),
            organization_id,
        };

        let mut config = client::load_config()?;
        let is_new_config = config.is_none();
        let existed = config
            .as_ref()
            .map(|c| c.profiles.contains_key(profile_name))
            .unwrap_or(false);

        let cfg = match config.as_mut() {
            Some(cfg) => {
                cfg.upsert_profile(profile_name, profile);
                cfg.clone()
            }
            None => ConnectConfig::new_with_profile(profile_name, profile),
        };

        client::save_config(&cfg)?;

        let action = if is_new_config {
            format!("Created new profile '{profile_name}' (set as default)")
        } else if existed {
            format!("Updated profile '{profile_name}'")
        } else {
            format!("Created new profile '{profile_name}'")
        };

        if self.output.is_json() {
            emit_json_event(&serde_json::json!({
                "event": "complete",
                "profile_name": profile_name,
                "user": { "email": email, "name": name },
                "api_url": self.url,
                "organization_id": organization_id_for_event(&cfg, profile_name),
            }));
        } else {
            print_success(
                &format!("Logged in as {name} ({email}) at {}\n  {action}", self.url),
                OutputLevel::Normal,
            );
        }
        Ok(())
    }
}

/// Pull the saved organization_id out of the persisted config for use in
/// the `complete` JSON event. We could thread it down as a parameter, but
/// reading it back from the (just-saved) config keeps `save_profile`'s
/// signature simple and makes it obvious the JSON event matches what's on
/// disk.
fn organization_id_for_event(cfg: &ConnectConfig, profile_name: &str) -> Option<String> {
    cfg.profiles
        .get(profile_name)
        .and_then(|p| p.organization_id.clone())
}

/// Generate a random state string for CSRF protection.
fn generate_state() -> String {
    use rand::RngExt;
    let bytes: [u8; 16] = rand::rng().random();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// Wait for the browser to redirect to our local server with the auth code.
/// Returns (code, state, stream) — caller is responsible for sending the HTTP response.
async fn wait_for_callback(
    listener: &TcpListener,
) -> Result<(String, String, tokio::net::TcpStream)> {
    let (mut stream, _addr) = listener
        .accept()
        .await
        .context("failed to accept callback connection")?;

    // Read the HTTP request (we only need the first line for the path)
    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .context("failed to read callback request")?;

    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP request from browser callback"))?;

    // Parse query params from the path
    let params = parse_query_params(path);
    let code = params
        .get("code")
        .ok_or_else(|| anyhow::anyhow!("no 'code' parameter in callback"))?
        .clone();
    let state = params
        .get("state")
        .ok_or_else(|| anyhow::anyhow!("no 'state' parameter in callback"))?
        .clone();

    Ok((code, state, stream))
}

/// Send an HTTP 302 redirect response on the given stream and shut it down cleanly.
async fn respond_redirect(stream: &mut tokio::net::TcpStream, url: &str) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        url,
    );
    stream.write_all(response.as_bytes()).await.ok();
    stream.flush().await.ok();
    stream.shutdown().await.ok();
}

/// Parse query parameters from a URL path like "/callback?code=abc&state=xyz"
fn parse_query_params(path: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    if let Some(query) = path.split('?').nth(1) {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = urldecode(key);
                let value = urldecode(value);
                params.insert(key, value);
            }
        }
    }
    params
}

/// Minimal percent-decoding for URL query parameters.
fn urldecode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        match b {
            b'+' => result.push(' '),
            b'%' => {
                let hi = chars.next().and_then(hex_digit);
                let lo = chars.next().and_then(hex_digit);
                if let (Some(h), Some(l)) = (hi, lo) {
                    result.push((h << 4 | l) as char);
                }
            }
            _ => result.push(b as char),
        }
    }
    result
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Outcome of the pure org-selection decision. Separated from the IO/UI
/// concerns so it can be unit-tested without an HTTP server or a TTY.
#[derive(Debug)]
enum OrgPick {
    /// User has zero orgs. Caller should pass no `organization_id` to
    /// exchange (server falls back to nil scope).
    None,
    /// An org was definitively chosen — either by explicit `--org` hint or
    /// by single-org auto-select.
    Resolved(OrgInfo),
    /// Multi-org interactive case. Caller must run the terminal picker
    /// over these orgs.
    NeedPrompt(Vec<OrgInfo>),
}

/// Decide which org to scope the about-to-be-minted CLI token to.
///
/// Resolution order:
///   1. `--org <id>` flag, if set, must match exactly one org by id (UUID).
///      No match → error. Name matching is intentionally not supported —
///      `--org` is for non-interactive scripting where org names can change
///      but ids are stable, and other `avocado connect` commands accept ids
///      only.
///   2. If the user has exactly one org, auto-select it.
///   3. Multiple orgs + JSON output → auto-select the default (first) org.
///      Non-interactive callers can't prompt; the first org is the default
///      by convention and `--org` overrides it.
///   4. Multiple orgs + human output → caller must prompt.
///   5. Zero orgs → `None` (caller passes no org to exchange; server falls
///      back to its default scoping).
///
/// Pure logic. No HTTP, no stdin, no stdout. The wrapper `resolve_org_for_login`
/// adds the IO around this.
fn pick_org(orgs: Vec<OrgInfo>, org_hint: Option<&str>, output: OutputFormat) -> Result<OrgPick> {
    // An explicit `--org` must be honored or fail loudly — never silently
    // dropped. Resolve it before the zero-orgs shortcut so a hint against an
    // empty org list errors instead of falling through to an unscoped token.
    if let Some(hint) = org_hint {
        return match orgs.iter().find(|o| o.id == hint) {
            Some(o) => Ok(OrgPick::Resolved(o.clone())),
            None => {
                let available = if orgs.is_empty() {
                    "none".to_string()
                } else {
                    orgs.iter()
                        .map(|o| format!("{} ({})", o.name, o.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                anyhow::bail!("organization '{hint}' not found. Available: {available}");
            }
        };
    }

    if orgs.is_empty() {
        return Ok(OrgPick::None);
    }

    if orgs.len() == 1 {
        return Ok(OrgPick::Resolved(orgs.into_iter().next().unwrap()));
    }

    // Multiple orgs. Non-interactive callers (JSON output — e.g. Avocado
    // Desktop) can't answer a prompt, so auto-select the default org. The
    // first org is the default by convention: the interactive picker lists
    // it first as the default, and the `--token` login path already
    // auto-selects `me.organizations.first()` (see `provision_org_token`).
    // An explicit `--org <id>` (handled above) always overrides.
    if output.is_json() {
        return Ok(OrgPick::Resolved(orgs.into_iter().next().unwrap()));
    }

    Ok(OrgPick::NeedPrompt(orgs))
}

/// Resolve which org the about-to-be-minted CLI token should be scoped to,
/// performing HTTP and interactive prompting around the pure decision in
/// `pick_org`.
async fn resolve_org_for_login(
    base_url: &str,
    code: &str,
    org_hint: Option<&str>,
    output: OutputFormat,
) -> Result<Option<OrgInfo>> {
    let orgs = list_orgs_by_code(base_url, code).await?;
    let org_count = orgs.len();

    match pick_org(orgs, org_hint, output)? {
        OrgPick::None => Ok(None),
        OrgPick::Resolved(org) => {
            // Only announce auto-select when there was no explicit hint —
            // the user already knows what they chose if they passed --org.
            if org_hint.is_none() {
                let msg = if org_count == 1 {
                    format!("Auto-selected only available organization: {}", org.name)
                } else {
                    format!("Auto-selected default organization: {}", org.name)
                };
                if output.is_json() {
                    emit_json_event(&serde_json::json!({"event": "info", "message": msg}));
                } else {
                    print_info(&msg, OutputLevel::Normal);
                }
            }
            Ok(Some(org))
        }
        OrgPick::NeedPrompt(orgs) => {
            let picked = prompt_select_org(&orgs)?;
            Ok(Some(picked))
        }
    }
}

/// POST /auth/cli/list-orgs {code} — returns the user's orgs without
/// consuming the code. The same `code` is reused for the subsequent exchange
/// call.
async fn list_orgs_by_code(base_url: &str, code: &str) -> Result<Vec<OrgInfo>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Data,
    }
    #[derive(serde::Deserialize)]
    struct Data {
        organizations: Vec<OrgInfo>,
    }

    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{base_url}/auth/cli/list-orgs"))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .context("failed to call /auth/cli/list-orgs")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["message"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("list-orgs failed ({status}): {msg}");
    }

    let parsed: Resp = resp.json().await.context("invalid list-orgs response")?;
    Ok(parsed.data.organizations)
}

/// Numbered terminal picker for org selection. Mirrors the picker in
/// `commands/connect/init.rs`; intentionally duplicated here rather than
/// shared because the two call sites have slightly different framing
/// (init runs against a saved profile; login runs against a bootstrap code)
/// and a premature abstraction would obscure that.
fn prompt_select_org(orgs: &[OrgInfo]) -> Result<OrgInfo> {
    println!("\nSelect an organization:");
    for (i, org) in orgs.iter().enumerate() {
        println!(
            "  [{}] {} ({}) - role: {}",
            i + 1,
            org.name,
            org.id,
            org.role
        );
    }
    eprint!("\nEnter number (1-{}): ", orgs.len());

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("failed to read input")?;

    let choice: usize = input.trim().parse().context("invalid number")?;
    if choice < 1 || choice > orgs.len() {
        anyhow::bail!("selection out of range");
    }
    Ok(orgs[choice - 1].clone())
}

/// Given an authenticated client and its /api/me response, return (token, Option<org_id>).
///
/// - If the token is already org-scoped (`me.token.organization_id` is set), return it as-is.
/// - If unscoped and orgs are available, create an org-scoped token for the first org.
/// - If unscoped and no orgs, return the original token with no org.
async fn provision_org_token(
    client: &ConnectClient,
    me: &crate::commands::connect::client::MeFullResponse,
    original_token: &str,
    token_name: &str,
    output: OutputFormat,
) -> Result<(String, Option<String>)> {
    // Already org-scoped?
    if let Some(ref token_info) = me.token {
        if let Some(ref org_id) = token_info.organization_id {
            return Ok((original_token.to_string(), Some(org_id.clone())));
        }
    }

    // Unscoped — try to create an org-scoped token for the first org.
    if let Some(org) = me.organizations.first() {
        let msg = format!("Creating org-scoped token for '{}'...", org.name);
        if output.is_json() {
            emit_json_event(&serde_json::json!({"event": "info", "message": msg}));
        } else {
            print_info(&msg, OutputLevel::Normal);
        }
        match client.create_org_token(&org.id, token_name).await {
            Ok((new_token, org_id)) => return Ok((new_token, Some(org_id))),
            Err(e) => {
                // Non-fatal: fall back to unscoped token with a warning.
                let warn = format!("Warning: could not create org-scoped token: {e}");
                if output.is_json() {
                    emit_json_event(&serde_json::json!({"event": "info", "message": warn}));
                } else {
                    print_info(&warn, OutputLevel::Normal);
                }
            }
        }
    }

    Ok((original_token.to_string(), None))
}

pub struct ConnectAuthLogoutCommand {
    pub profile: Option<String>,
    pub output: OutputFormat,
}

impl ConnectAuthLogoutCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?;
        match config {
            Some(mut cfg) => {
                let profile_name = self.profile.as_deref().unwrap_or(&cfg.default_profile);
                let profile_name = profile_name.to_string(); // avoid borrow issue

                if !cfg.remove_profile(&profile_name) {
                    let available: Vec<&str> = cfg.profiles.keys().map(|s| s.as_str()).collect();
                    if self.output.is_json() {
                        emit_json_object(&serde_json::json!({
                            "logged_out": false,
                            "profile_name": profile_name,
                            "reason": if available.is_empty() {
                                "no profiles configured".to_string()
                            } else {
                                format!("profile not found (available: {})", available.join(", "))
                            },
                        }));
                    } else if available.is_empty() {
                        print_info("No profiles configured.", OutputLevel::Normal);
                    } else {
                        print_error(
                            &format!(
                                "Profile '{}' not found. Available profiles: {}",
                                profile_name,
                                available.join(", ")
                            ),
                            OutputLevel::Normal,
                        );
                    }
                    return Ok(());
                }

                if cfg.profiles.is_empty() {
                    client::delete_config_file()?;
                } else {
                    client::save_config(&cfg)?;
                }

                if self.output.is_json() {
                    emit_json_object(&serde_json::json!({
                        "logged_out": true,
                        "profile_name": profile_name,
                    }));
                } else {
                    print_success(
                        &format!("Logged out of profile '{profile_name}'. Credentials removed."),
                        OutputLevel::Normal,
                    );
                }
            }
            None => {
                if self.output.is_json() {
                    emit_json_object(&serde_json::json!({
                        "logged_out": false,
                        "reason": "not logged in",
                    }));
                } else {
                    print_info("Not logged in.", OutputLevel::Normal);
                }
            }
        }
        Ok(())
    }
}

pub struct ConnectAuthStatusCommand {
    pub profile: Option<String>,
    pub output: OutputFormat,
}

impl ConnectAuthStatusCommand {
    pub async fn execute(&self) -> Result<()> {
        if self.output.is_json() {
            return self.execute_json().await;
        }

        match client::load_config()? {
            Some(cfg) => {
                let (profile_name, profile) =
                    match cfg.resolve_profile(self.profile.as_deref(), None) {
                        Ok(p) => p,
                        Err(e) => {
                            print_error(&e.to_string(), OutputLevel::Normal);
                            return Ok(());
                        }
                    };

                println!("Profile: {profile_name}");
                println!(
                    "Logged in as {} ({})",
                    profile.user.name, profile.user.email
                );
                println!("API URL: {}", profile.api_url);
                println!("Token created: {}", profile.created_at);
                if let Some(ref org_id) = profile.organization_id {
                    println!("Token scope: org {org_id}");
                } else {
                    println!("Token scope: unscoped (all orgs)");
                }

                // Verify token is still valid and show org memberships
                print_info("Verifying token...", OutputLevel::Normal);
                let client = ConnectClient::from_profile(profile)?;
                match client.get_me_full().await {
                    Ok(me_full) => {
                        print_success("Token is valid.", OutputLevel::Normal);

                        // Show live scope from API (may differ if token was upgraded server-side)
                        if let Some(ref token_info) = me_full.token {
                            if let Some(ref org_id) = token_info.organization_id {
                                println!(
                                    "API token scope: org {org_id} (token: {})",
                                    token_info.name
                                );
                            } else {
                                println!("API token scope: unscoped (token: {})", token_info.name);
                            }
                        }

                        if !me_full.organizations.is_empty() {
                            println!("\nOrganizations:");
                            for org in &me_full.organizations {
                                println!("  {}  (id: {})  role: {}", org.name, org.id, org.role);
                            }
                            println!(
                                "\nTip: Use org ID with --org or set connect.org in avocado.yaml"
                            );
                        }
                    }
                    Err(e) => {
                        print_error(&format!("Token may be invalid: {e}"), OutputLevel::Normal)
                    }
                }
            }
            None => {
                println!("Not logged in.");
                println!("Run 'avocado connect auth login' to authenticate.");
            }
        }
        Ok(())
    }

    /// JSON branch — emits a single JSON object on stdout. No prose, no
    /// stderr noise, no ANSI. Network calls are best-effort: if `/api/me`
    /// fails we still emit a logged-in object with `token_valid: false`
    /// so callers can distinguish "creds on disk but server rejected
    /// them" from "no creds at all".
    async fn execute_json(&self) -> Result<()> {
        let Some(cfg) = client::load_config()? else {
            emit_json_object(&serde_json::json!({ "logged_in": false }));
            return Ok(());
        };

        let (profile_name, profile) = match cfg.resolve_profile(self.profile.as_deref(), None) {
            Ok(p) => p,
            Err(e) => {
                emit_json_object(&serde_json::json!({
                    "logged_in": false,
                    "reason": e.to_string(),
                }));
                return Ok(());
            }
        };

        let mut payload = serde_json::json!({
            "logged_in": true,
            "profile_name": profile_name,
            "user": { "email": profile.user.email, "name": profile.user.name },
            "api_url": profile.api_url,
            "created_at": profile.created_at,
            "organization_id": profile.organization_id,
            "token_valid": serde_json::Value::Null,
            "organizations": serde_json::Value::Array(vec![]),
        });

        if let Ok(client) = ConnectClient::from_profile(profile) {
            match client.get_me_full().await {
                Ok(me) => {
                    payload["token_valid"] = serde_json::Value::Bool(true);
                    let orgs: Vec<serde_json::Value> = me
                        .organizations
                        .iter()
                        .map(|o| {
                            serde_json::json!({
                                "id": o.id,
                                "name": o.name,
                                "role": o.role,
                            })
                        })
                        .collect();
                    payload["organizations"] = serde_json::Value::Array(orgs);
                }
                Err(e) => {
                    payload["token_valid"] = serde_json::Value::Bool(false);
                    payload["token_error"] = serde_json::Value::String(e.to_string());
                }
            }
        }

        emit_json_object(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(id: &str, name: &str) -> OrgInfo {
        OrgInfo {
            id: id.to_string(),
            name: name.to_string(),
            role: "owner".to_string(),
        }
    }

    #[test]
    fn pick_org_zero_orgs_returns_none() {
        let result = pick_org(vec![], None, OutputFormat::Human).unwrap();
        assert!(matches!(result, OrgPick::None));
    }

    #[test]
    fn pick_org_explicit_hint_with_no_orgs_errors_not_dropped() {
        // An explicit --org must never be silently dropped: an empty org
        // list combined with a hint has to fail loudly rather than fall
        // through to an unscoped token. Regression guard for the silent-drop
        // login bug — the auth server returned zero orgs, so the hint was
        // ignored and a bogus unscoped profile was minted while reporting
        // success.
        let result = pick_org(vec![], Some("uuid-x"), OutputFormat::Json);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("uuid-x"), "got: {err}");
        assert!(err.contains("Available: none"), "got: {err}");
    }

    #[test]
    fn pick_org_single_org_is_resolved_without_hint() {
        let only = org("uuid-1", "Acme");
        let result = pick_org(vec![only], None, OutputFormat::Human).unwrap();
        match result {
            OrgPick::Resolved(o) => assert_eq!(o.id, "uuid-1"),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn pick_org_hint_matches_by_id_in_multi_org() {
        let acme = org("uuid-1", "Acme");
        let northwind = org("uuid-2", "Northwind");
        let result = pick_org(vec![acme, northwind], Some("uuid-2"), OutputFormat::Human).unwrap();
        match result {
            OrgPick::Resolved(o) => assert_eq!(o.id, "uuid-2"),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn pick_org_hint_does_not_match_by_name() {
        // Name matching is intentionally not supported — id-only.
        let acme = org("uuid-1", "Acme");
        let result = pick_org(vec![acme], Some("Acme"), OutputFormat::Human);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("'Acme' not found"), "got: {err}");
    }

    #[test]
    fn pick_org_hint_not_found_lists_available_orgs() {
        let acme = org("uuid-1", "Acme");
        let northwind = org("uuid-2", "Northwind");
        let result = pick_org(
            vec![acme, northwind],
            Some("uuid-bogus"),
            OutputFormat::Human,
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("uuid-bogus"), "got: {err}");
        assert!(err.contains("Acme (uuid-1)"), "got: {err}");
        assert!(err.contains("Northwind (uuid-2)"), "got: {err}");
    }

    #[test]
    fn pick_org_multi_org_json_output_auto_selects_default() {
        // Non-interactive (JSON) callers can't be prompted, so the first
        // org (the default by convention) is auto-selected rather than
        // erroring. Regression guard for the multi-org desktop login bug.
        let acme = org("uuid-1", "Acme");
        let northwind = org("uuid-2", "Northwind");
        let result = pick_org(vec![acme, northwind], None, OutputFormat::Json).unwrap();
        match result {
            OrgPick::Resolved(o) => assert_eq!(o.id, "uuid-1"),
            other => panic!("expected Resolved(default), got {other:?}"),
        }
    }

    #[test]
    fn pick_org_multi_org_human_output_needs_prompt() {
        let acme = org("uuid-1", "Acme");
        let northwind = org("uuid-2", "Northwind");
        let result = pick_org(vec![acme, northwind], None, OutputFormat::Human).unwrap();
        match result {
            OrgPick::NeedPrompt(orgs) => {
                assert_eq!(orgs.len(), 2);
                assert_eq!(orgs[0].id, "uuid-1");
                assert_eq!(orgs[1].id, "uuid-2");
            }
            other => panic!("expected NeedPrompt, got {other:?}"),
        }
    }

    #[test]
    fn pick_org_hint_wins_even_in_json_output() {
        // JSON-mode multi-org normally errors, but an explicit hint bypasses
        // the prompt-required check.
        let acme = org("uuid-1", "Acme");
        let northwind = org("uuid-2", "Northwind");
        let result = pick_org(vec![acme, northwind], Some("uuid-1"), OutputFormat::Json).unwrap();
        match result {
            OrgPick::Resolved(o) => assert_eq!(o.id, "uuid-1"),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }
}
