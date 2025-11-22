use anyhow::Result;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

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
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        let target = self.resolve_target_architecture(&config)?;
        let extension_location = self.find_extension_in_dependency_tree(&config, &target)?;
        let container_image = self.get_container_image(&parsed)?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        self.execute_dnf_command(
            &parsed,
            &container_image,
            &target,
            repo_url.as_ref(),
            repo_release.as_ref(),
            &merged_container_args,
            &extension_location,
        )
        .await
    }

    fn find_extension_in_dependency_tree(
        &self,
        config: &Config,
        target: &str,
    ) -> Result<ExtensionLocation> {
        match config.find_extension_in_dependency_tree(
            &self.config_path,
            &self.extension,
            target,
        )? {
            Some(location) => {
                if self.verbose {
                    match &location {
                        ExtensionLocation::Local { name, config_path } => {
                            print_info(
                                &format!(
                                    "Found local extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        ExtensionLocation::External { name, config_path } => {
                            print_info(
                                &format!(
                                    "Found external extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                    }
                }
                Ok(location)
            }
            None => {
                print_error(
                    &format!("Extension '{}' not found in configuration.", self.extension),
                    OutputLevel::Normal,
                );
                Err(anyhow::anyhow!("Extension not found"))
            }
        }
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

    #[allow(clippy::too_many_arguments)]
    async fn execute_dnf_command(
        &self,
        parsed: &toml::Value,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
        extension_location: &ExtensionLocation,
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
            merged_container_args,
            extension_location,
        )
        .await?;

        // Build and execute DNF command
        let dnf_command = self.build_dnf_command(extension_location);
        self.run_dnf_command(
            &container_helper,
            container_image,
            target,
            &dnf_command,
            repo_url,
            repo_release,
            merged_container_args,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn setup_extension_environment(
        &self,
        _config: &toml::Value,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
        extension_location: &ExtensionLocation,
    ) -> Result<()> {
        let extension_name = match extension_location {
            ExtensionLocation::Local { name, .. } => name,
            ExtensionLocation::External { name, .. } => name,
        };
        let check_cmd = format!("test -d $AVOCADO_EXT_SYSROOTS/{extension_name}");

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_cmd,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
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
                merged_container_args,
                extension_location,
            )
            .await?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_extension_directory(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
        extension_location: &ExtensionLocation,
    ) -> Result<()> {
        let extension_name = match extension_location {
            ExtensionLocation::Local { name, .. } => name,
            ExtensionLocation::External { name, .. } => name,
        };
        let setup_cmd = format!(
            "mkdir -p $AVOCADO_EXT_SYSROOTS/{extension_name}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm $AVOCADO_EXT_SYSROOTS/{extension_name}/var/lib"
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
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let setup_success = container_helper.run_in_container(config).await?;

        if !setup_success {
            print_error(
                &format!("Failed to set up extension directory for '{extension_name}'."),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Failed to create extension directory"));
        }

        if self.verbose {
            print_info(
                &format!("Created extension directory for '{extension_name}'."),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_dnf_command(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        dnf_command: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
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
            container_args: merged_container_args.clone(),
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

    fn build_dnf_command(&self, extension_location: &ExtensionLocation) -> String {
        let extension_name = match extension_location {
            ExtensionLocation::Local { name, .. } => name,
            ExtensionLocation::External { name, .. } => name,
        };
        let installroot = format!("$AVOCADO_EXT_SYSROOTS/{extension_name}");
        let command_args_str = self.command.join(" ");
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        format!(
            r#"
RPM_NO_CHROOT_FOR_SCRIPTS=1 \
AVOCADO_EXT_INSTALLROOT={installroot} \
PATH=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin:$PATH \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config-scripts \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={installroot} \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {dnf_args_str} \
    {command_args_str}
"#
        )
    }
}
