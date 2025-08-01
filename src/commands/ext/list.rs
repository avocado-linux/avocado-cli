use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::output::{print_success, OutputLevel};

pub struct ExtListCommand {
    config_path: String,
}

impl ExtListCommand {
    pub fn new(config_path: String) -> Self {
        Self { config_path }
    }

    pub fn execute(&self) -> Result<()> {
        // Load configuration
        let _config = load_config(&self.config_path)?;

        // Check if ext section exists in the raw TOML
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        let ext_section = match parsed.get("ext") {
            Some(ext) => ext,
            None => {
                return Ok(());
            }
        };

        // Get extension names
        let extensions = match ext_section.as_table() {
            Some(table) => table.keys().collect::<Vec<_>>(),
            None => {
                return Ok(());
            }
        };

        // List extension names
        for ext_name in &extensions {
            println!("{}", ext_name);
        }

        print_success(
            &format!("Listed {} extension(s).", extensions.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    // Note: These helper methods are no longer used in the simplified list command
    // but are kept for potential future use or reference
}
