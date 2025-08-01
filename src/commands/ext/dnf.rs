use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::SdkContainer;
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtDnfCommand {
    config_path: String,
    extension: String,
    dnf_args: Vec<String>,
    verbose: bool,
    target: Option<String>,
}

impl ExtDnfCommand {
    pub fn new(
        config_path: String,
        extension: String,
        dnf_args: Vec<String>,
        verbose: bool,
        target: Option<String>,
    ) -> Self {
        Self {
            config_path,
            extension,
            dnf_args,
            verbose,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if ext section exists
        let ext_section = match parsed.get("ext") {
            Some(ext) => ext,
            None => {
                print_error(
                    &format!("Extension '{}' not found in configuration.", self.extension),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        };

        // Check if the specific extension exists
        if !ext_section
            .as_table()
            .unwrap()
            .contains_key(&self.extension)
        {
            print_error(
                &format!("Extension '{}' not found in configuration.", self.extension),
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Get the SDK image from configuration
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })?;

        // Resolve target architecture
        let config_target = parsed
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
            .map(|s| s.to_string());
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());
        let target = resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })?;

        // Use the container helper
        let container_helper = SdkContainer::new();

        // Perform extension setup first
        if !self
            .do_extension_setup(&parsed, &container_helper, container_image, &target)
            .await?
        {
            return Err(anyhow::anyhow!("Failed to set up extension environment"));
        }

        // Build DNF command
        let dnf_command = self.build_dnf_command();

        if self.verbose {
            print_info(
                &format!("Running DNF command: {}", dnf_command),
                OutputLevel::Normal,
            );
        }

        // Execute the DNF command
        let success = container_helper
            .run_in_container(
                container_image,
                &target,
                &dnf_command,
                self.verbose,
                false, // don't source environment
                true,  // interactive
            )
            .await?;

        if success {
            print_success("DNF command completed successfully.", OutputLevel::Normal);
        } else {
            print_error("DNF command failed.", OutputLevel::Normal);
            return Err(anyhow::anyhow!("DNF command failed"));
        }

        Ok(())
    }

    async fn do_extension_setup(
        &self,
        _config: &toml::Value,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
    ) -> Result<bool> {
        // Check if extension directory structure exists, create it if not
        let check_cmd = format!(
            "test -d ${{AVOCADO_SDK_SYSROOTS}}/extensions/{}",
            self.extension
        );
        let dir_exists = container_helper
            .run_in_container(
                container_image,
                target,
                &check_cmd,
                self.verbose,
                false, // don't source environment
                false, // not interactive
            )
            .await?;

        if !dir_exists {
            let setup_cmd = format!(
                "mkdir -p ${{AVOCADO_EXT_SYSROOTS}}/{}/var/lib && cp -rf ${{AVOCADO_PREFIX}}/rootfs/var/lib/rpm ${{AVOCADO_EXT_SYSROOTS}}/{}/var/lib",
                self.extension, self.extension
            );

            let setup_success = container_helper
                .run_in_container(
                    container_image,
                    target,
                    &setup_cmd,
                    self.verbose,
                    false, // don't source environment
                    false, // not interactive
                )
                .await?;

            if !setup_success {
                print_error(
                    &format!(
                        "Failed to set up extension directory for '{}'.",
                        self.extension
                    ),
                    OutputLevel::Normal,
                );
                return Ok(false);
            }

            if self.verbose {
                print_info(
                    &format!("Created extension directory for '{}'.", self.extension),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(true)
    }

    fn build_dnf_command(&self) -> String {
        let installroot = format!("${{AVOCADO_EXT_SYSROOTS}}/{}", self.extension);

        // Join the DNF arguments
        let dnf_args_str = self.dnf_args.join(" ");

        format!(
            r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={} \
    {}
"#,
            installroot, dnf_args_str
        )
    }
}
