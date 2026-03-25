use serde::Deserialize;
use std::path::Path;

/// Result metadata written by provisioning scripts to communicate
/// post-provision host-side actions back to the CLI.
///
/// Scripts write this as `.provision-result.json` in `AVOCADO_PROVISION_OUT`.
#[derive(Debug, Deserialize)]
pub struct ProvisionResult {
    /// The action the host CLI should perform.
    /// Known values: "burn_removable" (write image to removable storage).
    pub host_action: String,
    /// Filename of the disk image (relative to the output directory).
    pub image: String,
}

impl ProvisionResult {
    /// Read a provision result from the output directory, if one exists.
    pub fn read_from_dir(out_dir: &Path) -> anyhow::Result<Option<Self>> {
        let result_file = out_dir.join(".provision-result.json");
        if !result_file.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&result_file)?;
        let result: Self = serde_json::from_str(&content)?;
        Ok(Some(result))
    }
}
