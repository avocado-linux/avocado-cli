use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    target::resolve_target_required,
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

        // Process container args with environment variable expansion
        let processed_container_args =
            crate::utils::config::Config::process_container_args(self.container_args.as_ref());

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
        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

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
            repo_url,
            repo_release,
            container_args: processed_container_args.clone(),
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
        // Get merged runtime configuration including target-specific dependencies
        let config = crate::utils::config::Config::load(&self.config_path)?;
        let merged_runtime = config
            .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
            .with_context(|| {
                format!(
                    "Runtime '{}' not found or has no configuration for target '{}'",
                    self.runtime_name, target_arch
                )
            })?;

        let binding = toml::map::Map::new();
        let runtime_deps = merged_runtime
            .get("dependencies")
            .and_then(|v| v.as_table())
            .unwrap_or(&binding);

        // Extract extension names and any type overrides from runtime dependencies
        let mut required_extensions = HashSet::new();
        let mut extension_type_overrides: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        // First, collect direct runtime dependencies
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

        // Recursively discover all extension dependencies (including nested external extensions)
        let all_required_extensions =
            self.find_all_extension_dependencies(&config, &required_extensions, target_arch)?;

        // Build extension symlink commands from config
        let mut symlink_commands = Vec::new();
        let mut processed_extensions = HashSet::new();

        // Process local extensions defined in [ext.*] sections
        if let Some(ext_config) = parsed.get("ext").and_then(|v| v.as_table()) {
            for (ext_name, _ext_data) in ext_config {
                // Only process extensions that are required by this runtime
                if all_required_extensions.contains(ext_name) {
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
                    processed_extensions.insert(ext_name.clone());
                }
            }
        }

        // Process external extensions (those required but not defined locally)
        for ext_name in &all_required_extensions {
            if !processed_extensions.contains(ext_name) {
                // This is an external extension - add symlink command for it
                symlink_commands.push(format!(
                    r#"
OUTPUT_EXT=$AVOCADO_PREFIX/output/extensions/{ext_name}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{ext_name}.raw

if [ -f "$OUTPUT_EXT" ]; then
    if ! cmp -s "$OUTPUT_EXT" "$RUNTIMES_EXT" 2>/dev/null; then
        ln -f $OUTPUT_EXT $RUNTIMES_EXT
    fi
else
    echo "Missing image for external extension {ext_name}."
fi"#
                ));
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

    /// Recursively find all extension dependencies, including nested external extensions
    fn find_all_extension_dependencies(
        &self,
        config: &crate::utils::config::Config,
        direct_extensions: &HashSet<String>,
        target_arch: &str,
    ) -> Result<HashSet<String>> {
        let mut all_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Process each direct extension dependency
        for ext_name in direct_extensions {
            self.collect_extension_dependencies(
                config,
                ext_name,
                &mut all_extensions,
                &mut visited,
                target_arch,
            )?;
        }

        Ok(all_extensions)
    }

    /// Recursively collect all dependencies for a single extension
    fn collect_extension_dependencies(
        &self,
        config: &crate::utils::config::Config,
        ext_name: &str,
        all_extensions: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        target_arch: &str,
    ) -> Result<()> {
        // Avoid infinite loops
        if visited.contains(ext_name) {
            return Ok(());
        }
        visited.insert(ext_name.to_string());

        // Add this extension to the result set
        all_extensions.insert(ext_name.to_string());

        // Load the main config to check for local extensions
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if this is a local extension
        if let Some(ext_config) = parsed
            .get("ext")
            .and_then(|e| e.as_table())
            .and_then(|table| table.get(ext_name))
        {
            // This is a local extension - check its dependencies
            if let Some(dependencies) = ext_config.get("dependencies").and_then(|d| d.as_table()) {
                for (_dep_name, dep_spec) in dependencies {
                    if let Some(nested_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                        // Check if this is an external extension dependency
                        if let Some(external_config_path) =
                            dep_spec.get("config").and_then(|v| v.as_str())
                        {
                            // This is an external extension - load its config and process recursively
                            let external_extensions = config.load_external_extensions(
                                &self.config_path,
                                external_config_path,
                            )?;

                            // Add the external extension itself
                            self.collect_extension_dependencies(
                                config,
                                nested_ext_name,
                                all_extensions,
                                visited,
                                target_arch,
                            )?;

                            // Process its dependencies from the external config
                            if let Some(ext_config) = external_extensions.get(nested_ext_name) {
                                if let Some(nested_deps) =
                                    ext_config.get("dependencies").and_then(|d| d.as_table())
                                {
                                    for (_nested_dep_name, nested_dep_spec) in nested_deps {
                                        if let Some(nested_nested_ext_name) =
                                            nested_dep_spec.get("ext").and_then(|v| v.as_str())
                                        {
                                            self.collect_extension_dependencies(
                                                config,
                                                nested_nested_ext_name,
                                                all_extensions,
                                                visited,
                                                target_arch,
                                            )?;
                                        }
                                    }
                                }
                            }
                        } else {
                            // This is a local extension dependency
                            self.collect_extension_dependencies(
                                config,
                                nested_ext_name,
                                all_extensions,
                                visited,
                                target_arch,
                            )?;
                        }
                    }
                }
            }
        } else {
            // This might be an external extension - we need to find it in the runtime dependencies
            // to get its config path, then process its dependencies
            let merged_runtime = config
                .get_merged_runtime_config(&self.runtime_name, target_arch, &self.config_path)?
                .with_context(|| {
                    format!(
                        "Runtime '{}' not found or has no configuration for target '{}'",
                        self.runtime_name, target_arch
                    )
                })?;

            if let Some(runtime_deps) = merged_runtime
                .get("dependencies")
                .and_then(|v| v.as_table())
            {
                for (_dep_name, dep_spec) in runtime_deps {
                    if let Some(dep_ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                        if dep_ext_name == ext_name {
                            if let Some(external_config_path) =
                                dep_spec.get("config").and_then(|v| v.as_str())
                            {
                                // Found the external extension - process its dependencies
                                let external_extensions = config.load_external_extensions(
                                    &self.config_path,
                                    external_config_path,
                                )?;

                                if let Some(ext_config) = external_extensions.get(ext_name) {
                                    if let Some(nested_deps) =
                                        ext_config.get("dependencies").and_then(|d| d.as_table())
                                    {
                                        for (_nested_dep_name, nested_dep_spec) in nested_deps {
                                            if let Some(nested_ext_name) =
                                                nested_dep_spec.get("ext").and_then(|v| v.as_str())
                                            {
                                                self.collect_extension_dependencies(
                                                    config,
                                                    nested_ext_name,
                                                    all_extensions,
                                                    visited,
                                                    target_arch,
                                                )?;
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config_file(temp_dir: &TempDir, content: &str) -> String {
        let config_path = temp_dir.path().join("avocado.toml");
        fs::write(&config_path, content).unwrap();
        config_path.to_string_lossy().to_string()
    }

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
        let temp_dir = TempDir::new().unwrap();
        let config_content = r#"
[sdk]
image = "test-image"

[runtime.test-runtime]
target = "x86_64"

[runtime.test-runtime.dependencies]
test-dep = { ext = "test-ext" }
"#;
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
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
        let temp_dir = TempDir::new().unwrap();
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
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
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
        let temp_dir = TempDir::new().unwrap();
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
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
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
        let temp_dir = TempDir::new().unwrap();
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
        let config_path = create_test_config_file(&temp_dir, config_content);
        let parsed: toml::Value = toml::from_str(config_content).unwrap();
        let cmd = RuntimeBuildCommand::new(
            "test-runtime".to_string(),
            config_path,
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
