use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtCleanCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl ExtCleanCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_extension_exists(&parsed)?;
        let container_image = self.get_container_image(&parsed)?;
        let target = self.resolve_target_architecture(&config)?;

        self.clean_extension(&container_image, &target).await
    }

    fn validate_extension_exists(&self, parsed: &toml::Value) -> Result<()> {
        let ext_section = parsed.get("ext").ok_or_else(|| {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No ext section found")
        })?;

        let ext_table = ext_section
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("Invalid ext section format"))?;

        if !ext_table.contains_key(&self.extension) {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Extension not found"));
        }

        Ok(())
    }

    fn get_container_image(&self, parsed: &toml::Value) -> Result<String> {
        parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, config: &crate::utils::config::Config) -> Result<String> {
        resolve_target_required(self.target.as_deref(), config)
    }

    async fn clean_extension(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning extension '{}'...", self.extension),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();
        let clean_command = format!("rm -rf $AVOCADO_EXT_SYSROOTS/{}", self.extension);

        if self.verbose {
            print_info(
                &format!("Running command: {clean_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            container_args: crate::utils::config::Config::process_container_args(
                self.container_args.as_ref(),
            ),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success(
                &format!("Successfully cleaned extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            print_error(
                &format!("Failed to clean extension '{}'.", self.extension),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!("Clean command failed"))
        }
    }
}
