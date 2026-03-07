use anyhow::{Context, Result};

use crate::commands::connect::client::{self, ApproveDelegateKeyRequest, ConnectClient};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectKeysApproveCommand {
    pub org: String,
    pub user_id: String,
    pub key_type: String,
    pub profile: Option<String>,
}

impl ConnectKeysApproveCommand {
    pub async fn execute(&self) -> Result<()> {
        print_info(
            &format!(
                "Approving {} key for user {} in org {}...",
                self.key_type, self.user_id, self.org
            ),
            OutputLevel::Normal,
        );

        let config = client::load_config()?
            .context("Not logged in. Run 'avocado connect auth login' first.")?;
        let (_name, profile) = config.resolve_profile(self.profile.as_deref())?;
        let connect = ConnectClient::from_profile(profile)?;

        let result = connect
            .approve_delegate_key(
                &self.org,
                &self.user_id,
                &ApproveDelegateKeyRequest {
                    key_type: Some(self.key_type.clone()),
                },
            )
            .await?;

        print_success(
            &format!(
                "Key approved (keyid: {}, type: {}, status: {}). Previous active key retired.",
                result.keyid, result.key_type, result.status
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
