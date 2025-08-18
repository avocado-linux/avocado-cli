use anyhow::Result;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtImageCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl ExtImageCommand {
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
        // Load configuration and parse raw TOML
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Merge container args from config and CLI
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Find extension using comprehensive lookup
        let extension_location = config.find_extension_in_dependency_tree(&self.config_path, &self.extension, &target)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        if self.verbose {
            match &extension_location {
                ExtensionLocation::Local { name, config_path } => {
                    print_info(
                        &format!("Found local extension '{name}' in config '{config_path}'"),
                        OutputLevel::Normal,
                    );
                }
                ExtensionLocation::External { name, config_path } => {
                    print_info(
                        &format!("Found external extension '{name}' in config '{config_path}'"),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        // Get extension configuration (for now, we still need to get it from local config for image logic)
        let ext_config = parsed
            .get("ext")
            .and_then(|ext| ext.get(&self.extension))
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in local configuration. External extension images not yet supported.", self.extension)
            })?;

        // Get extension types from the types array
        let ext_types = ext_config
            .get("types")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        if ext_types.is_empty() {
            return Err(anyhow::anyhow!(
                "Extension '{}' has no types specified. The 'types' array must contain at least one of: 'sysext', 'confext'.",
                self.extension
            ));
        }

        // Get SDK configuration
        let container_image = parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        // Use resolved target (from CLI/env) if available, otherwise fall back to config
        let _config_target = parsed
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
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // Create a single image for the extension
        // The runtime will decide whether to use it as sysext, confext, or both
        print_info(
            &format!(
                "Creating image for extension '{}' (types: {}).",
                self.extension,
                ext_types.join(", ")
            ),
            OutputLevel::Normal,
        );

        let result = self
            .create_image(
                &container_helper,
                container_image,
                &target_arch,
                &ext_types.join(","), // Pass types for potential future use
                repo_url,
                repo_release,
                &merged_container_args,
            )
            .await?;

        if result {
            print_success(
                &format!(
                    "Successfully created image for extension '{}' (types: {}).",
                    self.extension,
                    ext_types.join(", ")
                ),
                OutputLevel::Normal,
            );
        } else {
            return Err(anyhow::anyhow!(
                "Failed to create extension image for '{}'",
                self.extension
            ));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_image(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        extension_type: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<bool> {
        // Create the build script
        let build_script = self.create_build_script(extension_type);

        // Execute the build script in the SDK container
        if self.verbose {
            print_info("Executing image build script.", OutputLevel::Normal);
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.to_string(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let result = container_helper.run_in_container(config).await?;

        Ok(result)
    }

    fn create_build_script(&self, _extension_type: &str) -> String {
        format!(
            r#"
set -e

# Common variables
EXT_NAME="{}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/extensions"
OUTPUT_FILE="$OUTPUT_DIR/$EXT_NAME.raw"

# Create output directory
mkdir -p $OUTPUT_DIR

# Remove existing file if it exists
rm -f "$OUTPUT_FILE"

# Check if extension directory exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_EXT_SYSROOTS/$EXT_NAME."
    exit 1
fi

# Create squashfs image
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs
"#,
            self.extension
        )
    }
}
