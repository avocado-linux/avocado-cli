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
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        let extensions = self.get_extensions(&parsed);
        self.display_extensions(&extensions);

        print_success(
            &format!("Listed {} extension(s).", extensions.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn get_extensions(&self, parsed: &toml::Value) -> Vec<String> {
        parsed
            .get("ext")
            .and_then(|ext_section| ext_section.as_table())
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn display_extensions(&self, extensions: &[String]) {
        for ext_name in extensions {
            println!("{}", ext_name);
        }
    }
}
