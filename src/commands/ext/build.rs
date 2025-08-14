use anyhow::Result;

use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct ExtBuildCommand {
    extension: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl ExtBuildCommand {
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
        let config = crate::utils::config::Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Merge container args from config and CLI (similar to SDK commands)
        let processed_container_args =
            config.merge_sdk_container_args(self.container_args.as_ref());
        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Get extension configuration
        let ext_config = parsed
            .get("ext")
            .and_then(|ext| ext.get(&self.extension))
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Get extension types from the types array
        let ext_types = ext_config
            .get("types")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

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
                "Extension '{}' has no types specified. The 'types' array must contain at least one of: 'sysext', 'confext'.",
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
                        repo_url,
                        repo_release,
                        &processed_container_args,
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
                        repo_url,
                        repo_release,
                        &processed_container_args,
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

    #[allow(clippy::too_many_arguments)]
    async fn build_sysext_extension(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        ext_version: &str,
        ext_scopes: &[String],
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        processed_container_args: &Option<Vec<String>>,
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
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: processed_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
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

    #[allow(clippy::too_many_arguments)]
    async fn build_confext_extension(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target_arch: &str,
        ext_version: &str,
        ext_scopes: &[String],
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        processed_container_args: &Option<Vec<String>>,
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
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: processed_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
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
modules_dir="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/modules"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
echo "SYSEXT_SCOPE={}" >> "$release_file"

# Check if extension includes kernel modules and add AVOCADO_ON_MERGE if needed
if [ -d "$modules_dir" ] && [ -n "$(find "$modules_dir" -name "*.ko" -o -name "*.ko.xz" -o -name "*.ko.gz" 2>/dev/null | head -n 1)" ]; then
    echo "AVOCADO_ON_MERGE=depmod" >> "$release_file"
    echo "[INFO] Found kernel modules in extension '{}', added AVOCADO_ON_MERGE=depmod to release file"
fi
"#,
            self.extension,
            self.extension,
            self.extension,
            ext_scopes.join(" "),
            self.extension
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_sysext_build_script_basic() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script("1.0", &["system".to_string()]);

        // Print the actual script for debugging
        // println!("Generated sysext build script:\n{}", script);

        assert!(script.contains(
            "release_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/extension-release.d\""
        ));
        assert!(script.contains("release_file=\"$release_dir/extension-release.test-ext\""));
        assert!(script.contains("modules_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/modules\""));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=1\" >> \"$release_file\""));
        assert!(script.contains("echo \"SYSEXT_SCOPE=system\" >> \"$release_file\""));
        assert!(script.contains(
            "if [ -d \"$modules_dir\" ] && [ -n \"$(find \"$modules_dir\" -name \"*.ko\""
        ));
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=depmod\" >> \"$release_file\""));
        assert!(script.contains("Found kernel modules in extension 'test-ext'"));
    }

    #[test]
    fn test_create_confext_build_script_basic() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script("1.0", &["system".to_string()]);

        assert!(script
            .contains("release_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/etc/extension-release.d\""));
        assert!(script.contains("release_file=\"$release_dir/extension-release.test-ext\""));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=1\" >> \"$release_file\""));
        assert!(script.contains("echo \"CONFEXT_SCOPE=system\" >> \"$release_file\""));
        // Confext should NOT include kernel module detection
        assert!(!script.contains("modules_dir"));
        assert!(!script.contains("AVOCADO_ON_MERGE=depmod"));
        assert!(!script.contains("Found kernel modules"));
    }

    #[test]
    fn test_create_sysext_build_script_multiple_scopes() {
        let cmd = ExtBuildCommand {
            extension: "multi-scope-ext".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script =
            cmd.create_sysext_build_script("2.0", &["system".to_string(), "portable".to_string()]);

        assert!(script.contains("echo \"SYSEXT_SCOPE=system portable\" >> \"$release_file\""));
        assert!(script.contains("AVOCADO_EXT_SYSROOTS/multi-scope-ext/usr/lib/extension-release.d"));
    }

    #[test]
    fn test_create_confext_build_script_multiple_scopes() {
        let cmd = ExtBuildCommand {
            extension: "multi-scope-ext".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script =
            cmd.create_confext_build_script("2.0", &["system".to_string(), "portable".to_string()]);

        assert!(script.contains("echo \"CONFEXT_SCOPE=system portable\" >> \"$release_file\""));
        assert!(script.contains("AVOCADO_EXT_SYSROOTS/multi-scope-ext/etc/extension-release.d"));
    }

    #[test]
    fn test_kernel_module_detection_pattern() {
        let cmd = ExtBuildCommand {
            extension: "kernel-ext".to_string(),
            config_path: "avocado.toml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script("1.0", &["system".to_string()]);

        // Verify the find command looks for common kernel module extensions
        assert!(script.contains("-name \"*.ko\""));
        assert!(script.contains("-name \"*.ko.xz\""));
        assert!(script.contains("-name \"*.ko.gz\""));
        // Verify the conditional structure
        assert!(script.contains("if [ -d \"$modules_dir\" ] && [ -n \"$(find"));
        assert!(script.contains("2>/dev/null | head -n 1)\" ]; then"));
    }
}
