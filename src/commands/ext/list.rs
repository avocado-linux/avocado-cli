use anyhow::Result;
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::output::{print_success, OutputLevel};

pub struct ExtListCommand {
    config_path: String,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl ExtListCommand {
    pub fn new(config_path: String) -> Self {
        Self {
            config_path,
            composed_config: None,
        }
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(&self.config_path, None)?),
        };
        let parsed = &composed.merged_value;

        let extensions = self.get_extensions(parsed);
        self.display_extensions(&extensions);

        print_success(
            &format!("Listed {} extension(s).", extensions.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn get_extensions(&self, parsed: &serde_yaml::Value) -> Vec<String> {
        parsed
            .get("extensions")
            .and_then(|ext_section| ext_section.as_mapping())
            .map(|table| {
                table
                    .keys()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn display_extensions(&self, extensions: &[String]) {
        for ext_name in extensions {
            println!("{ext_name}");
        }
    }
}
