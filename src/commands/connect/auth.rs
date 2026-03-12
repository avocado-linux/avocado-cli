use anyhow::Result;
use chrono::Utc;

use crate::commands::connect::client::{
    self, ConnectClient, ConnectConfig, LoginClient, Profile, ProfileUser,
};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};

pub struct ConnectAuthLoginCommand {
    pub url: String,
    pub email: Option<String>,
    pub password: Option<String>,
    pub profile: Option<String>,
}

impl ConnectAuthLoginCommand {
    pub fn new(
        url: Option<String>,
        email: Option<String>,
        password: Option<String>,
        profile: Option<String>,
    ) -> Self {
        let url = url
            .or_else(|| std::env::var("AVOCADO_CONNECT_URL").ok())
            .unwrap_or_else(|| "https://connect.peridio.com".to_string());
        Self {
            url,
            email,
            password,
            profile,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        print_info(&format!("Logging in to {}", self.url), OutputLevel::Normal);

        // Use provided email or prompt interactively
        let email = if let Some(ref e) = self.email {
            e.clone()
        } else {
            eprint!("Email: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        };
        if email.is_empty() {
            anyhow::bail!("Email cannot be empty");
        }

        // Use provided password or prompt interactively
        let password = if let Some(ref p) = self.password {
            p.clone()
        } else {
            rpassword::prompt_password("Password: ")?
        };
        if password.is_empty() {
            anyhow::bail!("Password cannot be empty");
        }

        // Login via session
        let login_client = LoginClient::new(&self.url)?;
        login_client.login(&email, &password).await?;

        // Get user info
        let me = login_client.get_me().await?;

        // Create a persistent API token
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        let profile_name = self.profile.as_deref().unwrap_or("default");
        let token_name = format!("avocado-cli-{hostname}-{profile_name}");

        print_info("Creating API token...", OutputLevel::Normal);
        let raw_token = login_client.create_api_token(&token_name).await?;

        let profile = Profile {
            api_url: self.url.clone(),
            token: raw_token,
            user: ProfileUser {
                email: me.email.clone(),
                name: me.name.clone(),
            },
            created_at: Utc::now().to_rfc3339(),
        };

        // Load existing config or create new
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
            &format!(
                "Logged in as {} ({}) at {}\n  {}",
                me.name, me.email, self.url, action
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
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
                let (profile_name, profile) = match cfg.resolve_profile(self.profile.as_deref()) {
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

                // Verify token is still valid and show org memberships
                print_info("Verifying token...", OutputLevel::Normal);
                let client = ConnectClient::from_profile(profile)?;
                match client.get_me_full().await {
                    Ok(me_full) => {
                        print_success("Token is valid.", OutputLevel::Normal);

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
