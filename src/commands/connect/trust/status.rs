use anyhow::{Context, Result};

use crate::commands::connect::client;
use crate::utils::output::{print_info, OutputLevel};

pub struct ConnectTrustStatusCommand {
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectTrustStatusCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let connect = client::ConnectClient::from_profile(profile)?;

        let status = connect.get_trust_status(&self.org).await?;

        print_info(
            &format!("Fleet trust status for org: {}", self.org),
            OutputLevel::Normal,
        );
        print_info(
            &format!("  Current root version: {}", status.current_root_version),
            OutputLevel::Normal,
        );
        print_info(
            &format!("  Setup complete:       {}", status.setup_complete),
            OutputLevel::Normal,
        );
        print_info(
            &format!("  Root rotated:         {}", status.root_rotated),
            OutputLevel::Normal,
        );
        let level_label = match status.security_level {
            0 => "0 (server-managed)".to_string(),
            1 => "1 (content-delegated)".to_string(),
            2 => "2 (user-controlled root)".to_string(),
            n => format!("{n} (unknown)"),
        };
        print_info(
            &format!("  Security level:       {level_label}"),
            OutputLevel::Normal,
        );
        if status.has_pending_promotion {
            print_info(
                "  Pending promotion:    yes (awaiting co-signature)",
                OutputLevel::Normal,
            );
        }
        print_info(
            &format!("  Tracked devices:      {}", status.total_tracked_devices),
            OutputLevel::Normal,
        );
        print_info(
            &format!("  Stale devices:        {}", status.stale_device_count),
            OutputLevel::Normal,
        );

        if !status.root_version_distribution.is_empty() {
            print_info("  Root version distribution:", OutputLevel::Normal);
            for bucket in &status.root_version_distribution {
                print_info(
                    &format!("    v{}: {} devices", bucket.root_version, bucket.count),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(())
    }
}
