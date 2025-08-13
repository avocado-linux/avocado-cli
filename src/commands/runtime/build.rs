use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target,
};
use anyhow::{Context, Result};
use std::collections::HashSet;

pub struct RuntimeBuildCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeBuildCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;
        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

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

        // Build var image
        let build_script = self.create_build_script(&parsed, &target_arch)?;

        if self.verbose {
            print_info(
                "Executing complete image build script.",
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: build_script,
            verbose: self.verbose,
            source_environment: true, // need environment for build
            interactive: false,       // build script runs non-interactively
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: self.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let complete_result = container_helper
            .run_in_container(config)
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

        // Extract extension names and any type overrides from runtime dependencies
        let mut required_extensions = HashSet::new();
        let mut extension_type_overrides: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        for (_dep_name, dep_spec) in runtime_deps {
            if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                required_extensions.insert(ext_name.to_string());

                // Check if the runtime dependency specifies custom types
                if let Some(types) = dep_spec.get("types").and_then(|v| v.as_array()) {
                    let type_strings: Vec<String> = types
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                    if !type_strings.is_empty() {
                        extension_type_overrides.insert(ext_name.to_string(), type_strings);
                    }
                }
            }
        }

        // Build extension symlink commands from config
        let mut symlink_commands = Vec::new();

        if let Some(ext_config) = parsed.get("ext").and_then(|v| v.as_table()) {
            for (ext_name, _ext_data) in ext_config {
                // Only process extensions that are required by this runtime
                if required_extensions.contains(ext_name) {
                    symlink_commands.push(format!(
                        r#"
OUTPUT_EXT=$AVOCADO_PREFIX/output/extensions/{ext_name}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{ext_name}.raw

if [ -f "$OUTPUT_EXT" ]; then
    if ! cmp -s "$OUTPUT_EXT" "$RUNTIMES_EXT" 2>/dev/null; then
        ln -f $OUTPUT_EXT $RUNTIMES_EXT
    fi
else
    echo "Missing image for extension {ext_name}."
fi"#
                    ));
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
# Set common variables
RUNTIME_NAME="{}"
TARGET_ARCH="{}"

VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging
mkdir -p "$VAR_DIR/lib/avocado/extensions"

OUTPUT_DIR="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME"
mkdir -p $OUTPUT_DIR

{}

# Potential future SDK target hook.
# echo "Run: avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME"
# avocado-pre-image-var-$TARGET_ARCH $RUNTIME_NAME

# Create btrfs image with extensions and confexts subvolumes
mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/avocado/extensions \
    -f "$OUTPUT_DIR/avocado-image-var-$TARGET_ARCH.btrfs"

echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '$TARGET_ARCH'."
avocado-build-$TARGET_ARCH $RUNTIME_NAME
"#,
            self.runtime_name, target_arch, symlink_section
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
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.toml");
        assert!(!cmd.verbose);
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
            Some("x86_64".to_string()),
            None,
            None,
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        assert!(script.contains("RUNTIME_NAME=\"test-runtime\""));
        assert!(script.contains("TARGET_ARCH=\"x86_64\""));
        assert!(script.contains("VAR_DIR=$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"));
        assert!(script.contains("avocado-build-$TARGET_ARCH $RUNTIME_NAME"));
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
types = ["sysext"]
"#;
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        assert!(script.contains("test-ext.raw"));
        // Extension should be copied to avocado extensions directory but not symlinked to systemd directories
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions/test-ext.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext.raw"));
    }

    #[test]
    fn test_create_build_script_with_type_overrides() {
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
test-dep = { ext = "test-ext", types = ["sysext"] }

[ext.test-ext]
version = "1.0"
types = ["sysext", "confext"]
"#;
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        // Extension should be copied to avocado extensions directory but not symlinked to systemd directories
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions/test-ext.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext.raw"));
        // Should NOT include symlinks to systemd directories (runtime will handle this)
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext.raw $SYSEXT"));
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext.raw $CONFEXT"));
    }

    #[test]
    fn test_create_build_script_no_type_override_uses_extension_defaults() {
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
test-dep = { ext = "test-ext" }

[ext.test-ext]
version = "1.0"
types = ["confext"]
"#;
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            "avocado.toml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        let script = cmd.create_build_script(&parsed, "x86_64").unwrap();

        // Extension should be copied to avocado extensions directory but not symlinked to systemd directories
        assert!(script.contains("$AVOCADO_PREFIX/output/extensions/test-ext.raw"));
        assert!(script.contains("$VAR_DIR/lib/avocado/extensions/test-ext.raw"));
        // Should NOT include symlinks to systemd directories (runtime will handle this)
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext.raw $SYSEXT"));
        assert!(!script.contains("ln -sf /var/lib/avocado/extensions/test-ext.raw $CONFEXT"));
    }
}
