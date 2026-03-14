use anyhow::Result;

use crate::commands::connect::client::{
    self, ConnectClient, CreateDeviceParams, CreateDeviceRequest,
};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub struct ConnectDevicesListCommand {
    pub org: String,
    pub profile: Option<String>,
}

impl ConnectDevicesListCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let devices = client.list_devices(&self.org).await?;

        if devices.is_empty() {
            print_info(
                &format!("No devices found in org '{}'.", self.org),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        let max_id = devices
            .iter()
            .map(|d| d.identifier.len())
            .max()
            .unwrap_or(0)
            .max(10);
        let max_name = devices
            .iter()
            .map(|d| d.name.as_deref().unwrap_or("-").len())
            .max()
            .unwrap_or(0)
            .max(4);

        println!(
            "{:<id_w$}  {:<name_w$}  STATUS",
            "IDENTIFIER",
            "NAME",
            id_w = max_id,
            name_w = max_name,
        );
        for device in &devices {
            println!(
                "{:<id_w$}  {:<name_w$}  {}",
                device.identifier,
                device.name.as_deref().unwrap_or("-"),
                device.status.as_deref().unwrap_or("-"),
                id_w = max_id,
                name_w = max_name,
            );
        }

        Ok(())
    }
}

pub struct ConnectDevicesCreateCommand {
    pub org: String,
    pub name: String,
    pub identifier: String,
    pub profile: Option<String>,
}

impl ConnectDevicesCreateCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        let req = CreateDeviceRequest {
            device: CreateDeviceParams {
                name: self.name.clone(),
                identifier: self.identifier.clone(),
            },
        };

        let device = client.create_device(&self.org, &req).await?;

        print_success(
            &format!(
                "Created device '{}' (identifier: {}, id: {})",
                device.name.as_deref().unwrap_or(&self.name),
                device.identifier,
                device.id
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }
}

pub struct ConnectDevicesDeleteCommand {
    pub org: String,
    pub id: String,
    pub yes: bool,
    pub profile: Option<String>,
}

impl ConnectDevicesDeleteCommand {
    pub async fn execute(&self) -> Result<()> {
        if !self.yes {
            eprint!(
                "Are you sure you want to delete device '{}'? This cannot be undone. [y/N]: ",
                self.id
            );
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Cancelled.");
                return Ok(());
            }
        }

        let config = client::load_config()?
            .ok_or_else(|| anyhow::anyhow!("Not logged in. Run 'avocado connect auth login'"))?;
        let (_, profile) = config.resolve_profile(self.profile.as_deref(), Some(&self.org))?;
        let client = ConnectClient::from_profile(profile)?;

        client.delete_device(&self.org, &self.id).await?;

        print_success(
            &format!("Deleted device '{}'.", self.id),
            OutputLevel::Normal,
        );

        Ok(())
    }
}
