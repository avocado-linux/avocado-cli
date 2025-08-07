use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtBuildCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
}

impl ExtBuildCommand {
    pub fn new(
        extension: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
    ) -> Self {
        Self {
            extension,
            config_path,
            verbose,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Get extension configuration
        let ext_config = parsed
            .get("ext")
            .and_then(|ext| ext.get(&self.extension))
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Get extension types (sysext, confext) from boolean flags
        let mut ext_types = Vec::new();
        if ext_config
            .get("sysext")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            ext_types.push("sysext");
        }
        if ext_config
            .get("confext")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            ext_types.push("confext");
        }

        let ext_scopes = ext_config
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["system".to_string()]);

        let sysext_scopes = ext_config
            .get("sysext_scopes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| ext_scopes.clone());

        let confext_scopes = ext_config
            .get("confext_scopes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| ext_scopes.clone());

        if ext_types.is_empty() {
            return Err(anyhow::anyhow!(
                "Extension '{}' has sysext=false and confext=false. At least one must be true to build.",
                self.extension
            ));
        }

        // Get extension version
        let ext_version = ext_config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0");

        // Get SDK configuration
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        // Use resolved target (from CLI/env) if available, otherwise fall back to config
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
        let target_arch = resolved_target.ok_or_else(|| {
            anyhow::anyhow!("No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
        })?;

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Build extensions based on configuration
        let mut overall_success = true;

        for ext_type in ext_types {
            print_info(
                &format!("Building {} extension '{}'.", ext_type, self.extension),
                OutputLevel::Normal,
            );

            let build_result = match ext_type {
                "sysext" => {
                    self.build_sysext_extension(
                        &container_helper,
                        container_image,
                        &target_arch,
                        ext_version,
                        &sysext_scopes,
                    )
                    .await?
                }
                "confext" => {
                    self.build_confext_extension(
                        &container_helper,
                        container_image,
                        &target_arch,
                        ext_version,
                        &confext_scopes,
                    )
                    .await?
                }
                _ => false,
            };

            if build_result {
                print_success(
                    &format!(
                        "Successfully built {} extension '{}'.",
                        ext_type, self.extension
                    ),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!(
                        "Failed to build {} extension '{}'.",
                        ext_type, self.extension
                    ),
                    OutputLevel::Normal,
                );
                overall_success = false;
            }
        }

        if !overall_success {
            return Err(anyhow::anyhow!(
                "Failed to build one or more extension types"
            ));
        }

        Ok(())
    }

    async fn build_sysext_extension(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        ext_version: &str,
        ext_scopes: &[String],
    ) -> Result<bool> {
        // Create the build script for sysext extension
        let build_script = self.create_sysext_build_script(ext_version, ext_scopes);

        // Execute the build script in the SDK container
        if self.verbose {
            print_info(
                "Executing sysext extension build script.",
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            ..Default::default()
        };
        let result = container_helper.run_in_container(config).await?;

        if self.verbose {
            print_info(
                &format!("Sysext build script execution returned: {result}."),
                OutputLevel::Normal,
            );
        }

        Ok(result)
    }

    async fn build_confext_extension(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        ext_version: &str,
        ext_scopes: &[String],
    ) -> Result<bool> {
        // Create the build script for confext extension
        let build_script = self.create_confext_build_script(ext_version, ext_scopes);

        // Execute the build script in the SDK container
        if self.verbose {
            print_info(
                "Executing confext extension build script.",
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            ..Default::default()
        };
        let result = container_helper.run_in_container(config).await?;

        if self.verbose {
            print_info(
                &format!("Confext build script execution returned: {result}."),
                OutputLevel::Normal,
            );
        }

        Ok(result)
    }

    fn create_sysext_build_script(&self, _ext_version: &str, ext_scopes: &[String]) -> String {
        format!(
            r#"
set -e

release_dir="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/extension-release.d"
release_file="$release_dir/extension-release.{}"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
echo "SYSEXT_SCOPE={}" >> "$release_file"
"#,
            self.extension,
            self.extension,
            ext_scopes.join(" ")
        )
    }

    fn create_confext_build_script(&self, _ext_version: &str, ext_scopes: &[String]) -> String {
        format!(
            r#"
set -e

release_dir="$AVOCADO_EXT_SYSROOTS/{}/etc/extension-release.d"
release_file="$release_dir/extension-release.{}"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
echo "CONFEXT_SCOPE={}" >> "$release_file"
"#,
            self.extension,
            self.extension,
            ext_scopes.join(" ")
        )
    }
}
