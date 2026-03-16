use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::commands::connect::client::{self, ConnectClient, ConnectConfig, Profile, ProfileUser};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};

pub struct ConnectAuthLoginCommand {
    pub url: String,
    pub profile: Option<String>,
    pub token: Option<String>,
}

impl ConnectAuthLoginCommand {
    pub fn new(url: Option<String>, profile: Option<String>, token: Option<String>) -> Self {
        let url = url
            .or_else(|| std::env::var("AVOCADO_CONNECT_URL").ok())
            .unwrap_or_else(|| "https://connect.peridio.com".to_string());
        Self {
            url,
            profile,
            token,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        print_info(&format!("Logging in to {}", self.url), OutputLevel::Normal);

        if let Some(ref token) = self.token {
            self.login_with_token(token).await
        } else {
            self.login_with_browser().await
        }
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

        println!();
        println!("  Open this URL in your browser to log in:");
        println!();
        println!("  {login_url}");
        println!();
        print_info(
            "Waiting for authentication... (link expires in 5 minutes)",
            OutputLevel::Normal,
        );

        // Wait for the browser to redirect to our local server
        let (code, received_state, mut stream) = wait_for_callback(&listener).await?;

        // Validate state to prevent CSRF — respond to browser BEFORE bailing
        if received_state != state {
            let error_url = format!("{}/cli/login?error=state_mismatch", self.url);
            respond_redirect(&mut stream, &error_url).await;
            anyhow::bail!("state mismatch — possible CSRF attack. please try again.");
        }

        // Exchange the code for an API token before redirecting browser
        let profile_name = self.profile.as_deref().unwrap_or("default");
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        let token_name = format!("avocado-cli-{hostname}-{profile_name}");

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/auth/cli/exchange", self.url))
            .json(&serde_json::json!({
                "code": code,
                "cli_token_name": token_name
            }))
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
        let user_email = body["data"]["user"]["email"].as_str().unwrap_or("unknown");
        let user_name = body["data"]["user"]["name"].as_str().unwrap_or("unknown");

        // Exchange succeeded — NOW redirect browser to success page
        let success_url = format!("{}/cli/success", self.url);
        respond_redirect(&mut stream, &success_url).await;

        // Provision an org-scoped token from the unscoped browser token.
        // If this fails for any reason, fall back to saving the unscoped token so
        // the user is always logged in after a successful browser auth.
        let temp_profile = Profile {
            api_url: self.url.clone(),
            token: token.to_string(),
            user: ProfileUser {
                email: user_email.to_string(),
                name: user_name.to_string(),
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            organization_id: None,
        };
        let temp_client = ConnectClient::from_profile(&temp_profile)?;
        let (final_token, organization_id) = match temp_client.get_me_full().await {
            Ok(me) => provision_org_token(&temp_client, &me, token, &token_name).await?,
            Err(e) => {
                print_info(
                    &format!("Warning: could not fetch org info ({e}); saving unscoped profile."),
                    OutputLevel::Normal,
                );
                (token.to_string(), None)
            }
        };

        self.save_profile(
            profile_name,
            &final_token,
            user_email,
            user_name,
            organization_id,
        )?;

        Ok(())
    }

    /// Token-based login: validate the provided token and save it.
    async fn login_with_token(&self, token: &str) -> Result<()> {
        print_info("Validating token...", OutputLevel::Normal);

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
            provision_org_token(&temp_client, &me, token, &token_name).await?;

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

        print_success(
            &format!("Logged in as {name} ({email}) at {}\n  {action}", self.url),
            OutputLevel::Normal,
        );
        Ok(())
    }
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
) -> Result<(String, Option<String>)> {
    // Already org-scoped?
    if let Some(ref token_info) = me.token {
        if let Some(ref org_id) = token_info.organization_id {
            return Ok((original_token.to_string(), Some(org_id.clone())));
        }
    }

    // Unscoped — try to create an org-scoped token for the first org.
    if let Some(org) = me.organizations.first() {
        print_info(
            &format!("Creating org-scoped token for '{}'...", org.name),
            OutputLevel::Normal,
        );
        match client.create_org_token(&org.id, token_name).await {
            Ok((new_token, org_id)) => return Ok((new_token, Some(org_id))),
            Err(e) => {
                // Non-fatal: fall back to unscoped token with a warning.
                print_info(
                    &format!("Warning: could not create org-scoped token: {e}"),
                    OutputLevel::Normal,
                );
            }
        }
    }

    Ok((original_token.to_string(), None))
}

pub struct ConnectAuthLogoutCommand {
    pub profile: Option<String>,
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
                    if available.is_empty() {
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

                print_success(
                    &format!("Logged out of profile '{profile_name}'. Credentials removed."),
                    OutputLevel::Normal,
                );
            }
            None => {
                print_info("Not logged in.", OutputLevel::Normal);
            }
        }
        Ok(())
    }
}

pub struct ConnectAuthStatusCommand {
    pub profile: Option<String>,
}

impl ConnectAuthStatusCommand {
    pub async fn execute(&self) -> Result<()> {
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
}
