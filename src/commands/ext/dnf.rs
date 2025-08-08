use anyhow::Result;

use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtDnfCommand {
    config_path: String,
    extension: String,
    command: Vec<String>,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl ExtDnfCommand {
    pub fn new(
        config_path: String,
        extension: String,
        command: Vec<String>,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            extension,
            command,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_extension_exists(&parsed)?;
        let container_image = self.get_container_image(&parsed)?;
        let target = self.resolve_target_architecture(&parsed)?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        self.execute_dnf_command(&parsed, &container_image, &target, repo_url, repo_release)
            .await
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

    fn resolve_target_architecture(&self, parsed: &toml::Value) -> Result<String> {
        let config_target = self.extract_config_target(parsed);
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());

        resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })
    }

    fn extract_config_target(&self, parsed: &toml::Value) -> Option<String> {
        parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_table())
            .and_then(|runtime_table| {
                if runtime_table.len() == 1 {
                    runtime_table.values().next()
                } else {
                    None
                }
            })
            .and_then(|runtime_config| runtime_config.get("target"))
            .and_then(|target| target.as_str())
            .map(|s| s.to_string())
    }

    async fn execute_dnf_command(
        &self,
        parsed: &toml::Value,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
    ) -> Result<()> {
        let container_helper = SdkContainer::new();

        // Perform extension setup first
        self.setup_extension_environment(
            parsed,
            &container_helper,
            container_image,
            target,
            repo_url,
            repo_release,
        )
        .await?;

        // Build and execute DNF command
        let dnf_command = self.build_dnf_command();
        self.run_dnf_command(
            &container_helper,
            container_image,
            target,
            &dnf_command,
            repo_url,
            repo_release,
        )
        .await
    }

    async fn setup_extension_environment(
        &self,
        _config: &toml::Value,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
    ) -> Result<()> {
        let check_cmd = format!(
            "test -d $AVOCADO_SDK_SYSROOTS/extensions/{}",
            self.extension
        );

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_cmd,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let dir_exists = container_helper.run_in_container(config).await?;

        if !dir_exists {
            // TODO: does this actually need the repo release + url ??
            self.create_extension_directory(
                container_helper,
                container_image,
                target,
                repo_url,
                repo_release,
            )
            .await?;
        }

        Ok(())
    }

    async fn create_extension_directory(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
    ) -> Result<()> {
        let setup_cmd = format!(
            "mkdir -p $AVOCADO_EXT_SYSROOTS/{}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm $AVOCADO_EXT_SYSROOTS/{}/var/lib",
            self.extension, self.extension
        );

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: setup_cmd,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let setup_success = container_helper.run_in_container(config).await?;

        if !setup_success {
            print_error(
                &format!(
                    "Failed to set up extension directory for '{}'.",
                    self.extension
                ),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Failed to create extension directory"));
        }

        if self.verbose {
            print_info(
                &format!("Created extension directory for '{}'.", self.extension),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    async fn run_dnf_command(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        dnf_command: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Running DNF command: {dnf_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: dnf_command.to_string(),
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: true,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success("DNF command completed successfully.", OutputLevel::Normal);
            Ok(())
        } else {
            print_error("DNF command failed.", OutputLevel::Normal);
            Err(anyhow::anyhow!("DNF command failed"))
        }
    }

    fn build_dnf_command(&self) -> String {
        let installroot = format!("$AVOCADO_EXT_SYSROOTS/{}", self.extension);
        let command_args_str = self.command.join(" ");
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        format!(
            r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={installroot} \
    {dnf_args_str} \
    {command_args_str}
"#
        )
    }
}
