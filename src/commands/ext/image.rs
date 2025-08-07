use anyhow::Result;

use crate::utils::config::load_config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtImageCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
}

impl ExtImageCommand {
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

        if ext_types.is_empty() {
            return Err(anyhow::anyhow!(
                "Extension '{}' has sysext=false and confext=false. At least one must be true to create image.",
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

        // Create images based on configuration
        let mut overall_success = true;

        for ext_type in ext_types {
            print_info(
                &format!(
                    "Creating {} image for extension '{}'.",
                    ext_type, self.extension
                ),
                OutputLevel::Normal,
            );

            let result = self
                .create_image(&container_helper, container_image, &target_arch, ext_type)
                .await?;

            if result {
                print_success(
                    &format!(
                        "Successfully created {} image for extension '{}'.",
                        ext_type, self.extension
                    ),
                    OutputLevel::Normal,
                );
            } else {
                print_error(
                    &format!(
                        "Failed to create {} image for extension '{}'.",
                        ext_type, self.extension
                    ),
                    OutputLevel::Normal,
                );
                overall_success = false;
            }
        }

        if !overall_success {
            return Err(anyhow::anyhow!(
                "Failed to create one or more extension images"
            ));
        }

        Ok(())
    }

    async fn create_image(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        extension_type: &str,
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
