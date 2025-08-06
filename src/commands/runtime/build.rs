use crate::utils::{
    config::load_config,
    container::SdkContainer,
    output::{print_info, print_success, OutputLevel},
    target::resolve_target,
};
use anyhow::{Context, Result};
use std::collections::HashSet;

pub struct RuntimeBuildCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    force: bool,
    target: Option<String>,
}

impl RuntimeBuildCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            force,
            target,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Get SDK configuration
        let sdk_config = parsed.get("sdk").context("No SDK configuration found")?;

        let container_image = sdk_config
            .get("image")
            .and_then(|v| v.as_str())
            .context("No SDK container image specified in configuration")?;

        // Get runtime configuration
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        // Check if runtime exists
        let runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        // Get target from runtime config
        let config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target(self.target.as_deref(), config_target.as_deref())
            .with_context(|| {
                format!(
                    "No target architecture specified for runtime '{}'. Use --target, AVOCADO_TARGET env var, or config under 'runtime.{}.target'",
                    self.runtime_name,
                    self.runtime_name
                )
            })?;

        print_info(
            &format!("Building runtime images for '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Initialize SDK container helper
        let container_helper = SdkContainer::new();

        // First check if the required images package is already installed (silent check)
        let dnf_check_script = format!(
            r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
$DNF_SDK_HOST_OPTS \
$DNF_SDK_TARGET_REPO_CONF \
--installroot=$AVOCADO_PREFIX/runtimes/{} \
list installed avocado-pkg-images >/dev/null 2>&1
"#,
            self.runtime_name
        );

        // Use container helper to check package status
        let package_installed = container_helper
            .run_in_container(
                container_image,
                &target_arch,
                &dnf_check_script,
                self.verbose, // verbose
                false,        // source_environment (simple check doesn't need full env)
                false,        // interactive (non-interactive check)
            )
            .await
            .unwrap_or(false);

        if !package_installed {
            print_info(
                "Installing avocado-pkg-images package.",
                OutputLevel::Normal,
            );
            let yes = if self.force { "-y" } else { "" };

            // Create DNF install script
            let dnf_install_script = format!(
                r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot=$AVOCADO_PREFIX/runtimes/{} \
    install \
    {} \
    avocado-pkg-images
"#,
                self.runtime_name, yes
            );

            // Run the DNF install command
            let install_result = container_helper
                .run_in_container(
                    container_image,
                    &target_arch,
                    &dnf_install_script,
                    self.verbose, // verbose
                    true,         // source_environment (need environment for DNF)
                    !self.force,  // interactive (opposite of force)
                )
                .await
                .context("Failed to install avocado-pkg-images package")?;

            if !install_result {
                return Err(anyhow::anyhow!(
                    "Failed to install avocado-pkg-images package"
                ));
            }

            print_success(
                "Successfully installed avocado-pkg-images package.",
                OutputLevel::Normal,
            );
        } else {
            print_info("avocado-pkg-images already installed.", OutputLevel::Normal);
        }

        // Build var image
        let build_script = self.create_build_script(&parsed, &target_arch)?;

        if self.verbose {
            print_info(
                "Executing complete image build script.",
                OutputLevel::Normal,
            );
        }

        let complete_result = container_helper
            .run_in_container(
                container_image,
                &target_arch,
                &build_script,
                self.verbose, // verbose
                true,         // source_environment (need environment for build)
                false,        // interactive (build script runs non-interactively)
            )
            .await
            .context("Failed to build complete image")?;

        if !complete_result {
            return Err(anyhow::anyhow!("Failed to build complete image"));
        }

        print_success(
            &format!("Successfully built runtime '{}'", self.runtime_name),
            OutputLevel::Normal,
        );
        Ok(())
    }

    fn create_build_script(&self, parsed: &toml::Value, target_arch: &str) -> Result<String> {
        // Get runtime dependencies to identify required extensions
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        let runtime_spec = runtime_config
            .get(&self.runtime_name)
            .with_context(|| format!("Runtime '{}' not found", self.runtime_name))?;

        let binding = toml::map::Map::new();
        let runtime_deps = runtime_spec
            .get("dependencies")
            .and_then(|v| v.as_table())
            .unwrap_or(&binding);

        // Extract extension names from runtime dependencies
        let mut required_extensions = HashSet::new();
        for (_dep_name, dep_spec) in runtime_deps {
            if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                required_extensions.insert(ext_name.to_string());
            }
        }

        // Build extension symlink commands from config
        let mut symlink_commands = Vec::new();

        if let Some(ext_config) = parsed.get("ext").and_then(|v| v.as_table()) {
            for (ext_name, ext_data) in ext_config {
                // Only process extensions that are required by this runtime
                if required_extensions.contains(ext_name) {
                    let is_sysext = ext_data
                        .get("sysext")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let is_confext = ext_data
                        .get("confext")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    symlink_commands.push(format!(
                        r#"
OUTPUT_EXT=$AVOCADO_PREFIX/output/extensions/{ext_name}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{ext_name}.raw
SYSEXT=$VAR_DIR/lib/extensions/{ext_name}.raw
CONFEXT=$VAR_DIR/lib/confexts/{ext_name}.raw

if [ -f "$OUTPUT_EXT" ]; then
    if ! cmp -s "$OUTPUT_EXT" "$RUNTIMES_EXT" 2>/dev/null; then
        ln -f $OUTPUT_EXT $RUNTIMES_EXT
    fi
else
    echo "Missing image for extension {ext_name}."
fi"#
                    ));

                    if is_sysext {
                        symlink_commands.push(format!(
                            "ln -sf /var/lib/avocado/extensions/{ext_name}.raw $SYSEXT"
                        ));
                    }

                    if is_confext {
                        symlink_commands.push(format!(
                            "ln -sf /var/lib/avocado/extensions/{ext_name}.raw $CONFEXT"
                        ));
                    }
                }
            }
        }

        let symlink_section = if symlink_commands.is_empty() {
            "# No extensions configured for symlinking".to_string()
        } else {
            symlink_commands.join("\n")
        };

        let script = format!(
            r#"
VAR_DIR=$AVOCADO_PREFIX/runtimes/{}/var-staging
mkdir -p "$VAR_DIR/lib/extensions"
mkdir -p "$VAR_DIR/lib/confexts"
mkdir -p "$VAR_DIR/lib/avocado/extensions"

OUTPUT_DIR="$AVOCADO_PREFIX/output/runtimes/{}"
mkdir -p $OUTPUT_DIR

{}

# Potential future SDK target hook.
# echo "Run: avocado-pre-image-var-{} {}"
# avocado-pre-image-var-{} {}

# Create btrfs image with extensions and confexts subvolumes
mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/extensions \
    --subvol rw:lib/confexts \
    -f "$OUTPUT_DIR/avocado-image-var.btrfs"

echo -e "\033[34m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '{}'."
avocado-build-{} {}
"#,
            self.runtime_name,
            self.runtime_name,
            symlink_section,
            target_arch,
            self.runtime_name,
            target_arch,
            self.runtime_name,
            target_arch,
            target_arch,
            self.runtime_name
        );

        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_create_build_script() {
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
test-dep = { ext = "test-ext" }
"#;
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        assert!(script.contains("VAR_DIR=$AVOCADO_PREFIX/runtimes/test-runtime/var-staging"));
        assert!(script.contains("avocado-build-x86_64 test-runtime"));
        assert!(script.contains("mkfs.btrfs"));
    }

    #[test]
    fn test_create_build_script_with_extensions() {
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
test-dep = { ext = "test-ext" }

[ext.test-ext]
version = "1.0"
sysext = true
confext = false
"#;
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            false,
            Some("x86_64".to_string()),
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        assert!(script.contains("test-ext.raw"));
        assert!(script.contains("ln -sf /var/lib/avocado/extensions/test-ext.raw"));
    }
}
