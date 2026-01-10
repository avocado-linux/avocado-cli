use anyhow::Result;
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct RuntimeCleanCommand {
    runtime: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RuntimeCleanCommand {
    pub fn new(
        runtime: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime,
            config_path,
            verbose,
            target,
            container_args,
            dnf_args,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(
                &self.config_path,
                self.target.as_deref(),
            )?),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        self.validate_runtime_exists(parsed)?;
        let container_image = self.get_container_image(config)?;
        let target = self.resolve_target_architecture(config)?;

        self.clean_runtime(&container_image, &target).await
    }

    fn validate_runtime_exists(&self, parsed: &serde_yaml::Value) -> Result<()> {
        let runtime_section = parsed.get("runtimes").ok_or_else(|| {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No runtime section found")
        })?;

        let runtime_table = runtime_section
            .as_mapping()
            .ok_or_else(|| anyhow::anyhow!("Invalid runtime section format"))?;

        if !runtime_table.contains_key(&self.runtime) {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Runtime not found"));
        }

        Ok(())
    }

    fn get_container_image(&self, config: &Config) -> Result<String> {
        config
            .get_sdk_image()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, config: &crate::utils::config::Config) -> Result<String> {
        resolve_target_required(self.target.as_deref(), config)
    }

    async fn clean_runtime(&self, container_image: &str, target: &str) -> Result<()> {
        print_info(
            &format!("Cleaning runtime '{}'...", self.runtime),
            OutputLevel::Normal,
        );

        let container_helper = SdkContainer::new();

        // Clean runtime directory and stamps
        let clean_command = format!(
            r#"
# Clean runtime build directory
rm -rf "$AVOCADO_PREFIX/runtimes/{runtime}"

# Clean runtime stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/runtime/{runtime}"
"#,
            runtime = self.runtime
        );

        if self.verbose {
            print_info(
                &format!("Running command: {clean_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: clean_command,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: crate::utils::config::Config::process_container_args(
                self.container_args.as_ref(),
            ),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success(
                &format!("Successfully cleaned runtime '{}'.", self.runtime),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            print_error(
                &format!("Failed to clean runtime '{}'.", self.runtime),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!("Clean command failed"))
        }
    }

    /// Generate the clean command script for testing
    #[cfg(test)]
    fn generate_clean_script(&self) -> String {
        format!(
            r#"
# Clean runtime build directory
rm -rf "$AVOCADO_PREFIX/runtimes/{runtime}"

# Clean runtime stamps (install and build)
rm -rf "$AVOCADO_PREFIX/.stamps/runtime/{runtime}"
"#,
            runtime = self.runtime
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = RuntimeCleanCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert_eq!(cmd.target, None);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_clean_script_cleans_runtime_directory() {
        let cmd = RuntimeCleanCommand::new(
            "production".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean runtime build directory
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/runtimes/production""#));
    }

    #[test]
    fn test_clean_script_cleans_stamps() {
        let cmd = RuntimeCleanCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Should clean runtime stamps (install and build)
        assert!(script.contains(r#"rm -rf "$AVOCADO_PREFIX/.stamps/runtime/dev""#));
    }

    #[test]
    fn test_clean_script_includes_all_cleanup_targets() {
        let cmd = RuntimeCleanCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );

        let script = cmd.generate_clean_script();

        // Verify both cleanup targets are present
        assert!(
            script.contains("runtimes/my-runtime"),
            "Should clean runtime directory"
        );
        assert!(
            script.contains(".stamps/runtime/my-runtime"),
            "Should clean stamps"
        );
    }

    // ========================================================================
    // Stamp Lifecycle Tests
    // ========================================================================

    #[test]
    fn test_clean_removes_all_runtime_stamps() {
        use crate::utils::stamps::StampRequirement;

        // Runtime has install, build, sign, provision stamps
        let stamps = vec![
            StampRequirement::runtime_install("dev"),
            StampRequirement::runtime_build("dev"),
            StampRequirement::runtime_sign("dev"),
            StampRequirement::runtime_provision("dev"),
        ];

        // All should be under runtime/<name>/ which clean removes
        for stamp in &stamps {
            let path = stamp.relative_path();
            assert!(
                path.starts_with("runtime/dev/"),
                "Stamp {path} should be under runtime/dev/"
            );
        }

        // Clean command removes the parent directory
        let cmd = RuntimeCleanCommand::new(
            "dev".to_string(),
            "avocado.yaml".to_string(),
            false,
            None,
            None,
            None,
        );
        let script = cmd.generate_clean_script();
        assert!(script.contains(".stamps/runtime/dev"));
    }

    #[test]
    fn test_clean_then_build_requires_reinstall() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
            StampRequirement,
        };

        // Runtime build requirements after cleaning
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::runtime_install("my-runtime"),
        ];

        // Before clean: all satisfied
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let rt_install = Stamp::runtime_install(
            "my-runtime",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let rt_json = serde_json::to_string(&rt_install).unwrap();

        let output_before = format!(
            "sdk/{}/install.stamp:::{}\nruntime/my-runtime/install.stamp:::{}",
            get_local_arch(),
            sdk_json,
            rt_json
        );

        let result_before = validate_stamps_batch(&requirements, &output_before, None);
        assert!(result_before.is_satisfied());

        // After runtime clean: SDK still there, runtime stamps gone
        let output_after = format!(
            "sdk/{}/install.stamp:::{}\nruntime/my-runtime/install.stamp:::null",
            get_local_arch(),
            sdk_json
        );

        let result_after = validate_stamps_batch(&requirements, &output_after, None);
        assert!(!result_after.is_satisfied());
        assert_eq!(result_after.missing.len(), 1);
        assert_eq!(
            result_after.missing[0].relative_path(),
            "runtime/my-runtime/install.stamp"
        );
    }

    #[test]
    fn test_runtime_clean_preserves_sdk_and_ext_stamps() {
        use crate::utils::stamps::{
            get_local_arch, validate_stamps_batch, Stamp, StampInputs, StampOutputs,
            StampRequirement,
        };

        // Requirements that span SDK, extensions, and runtime
        let requirements = vec![
            StampRequirement::sdk_install(),
            StampRequirement::ext_install("my-ext"),
            StampRequirement::ext_build("my-ext"),
            StampRequirement::runtime_install("my-runtime"),
        ];

        // All present before clean
        let sdk_stamp = Stamp::sdk_install(
            get_local_arch(),
            StampInputs::new("hash1".to_string()),
            StampOutputs::default(),
        );
        let ext_install = Stamp::ext_install(
            "my-ext",
            "qemux86-64",
            StampInputs::new("hash2".to_string()),
            StampOutputs::default(),
        );
        let ext_build = Stamp::ext_build(
            "my-ext",
            "qemux86-64",
            StampInputs::new("hash3".to_string()),
            StampOutputs::default(),
        );
        let rt_install = Stamp::runtime_install(
            "my-runtime",
            "qemux86-64",
            StampInputs::new("hash4".to_string()),
            StampOutputs::default(),
        );

        let sdk_json = serde_json::to_string(&sdk_stamp).unwrap();
        let ext_install_json = serde_json::to_string(&ext_install).unwrap();
        let ext_build_json = serde_json::to_string(&ext_build).unwrap();
        // Runtime stamp is intentionally not used - simulating it was cleaned (returns null)
        let _rt_json = serde_json::to_string(&rt_install).unwrap();

        // After runtime clean: only runtime stamp is gone
        // SDK and ext stamps should remain
        let output_after = format!(
            "sdk/{}/install.stamp:::{}\next/my-ext/install.stamp:::{}\next/my-ext/build.stamp:::{}\nruntime/my-runtime/install.stamp:::null",
            get_local_arch(),
            sdk_json,
            ext_install_json,
            ext_build_json
        );

        let result = validate_stamps_batch(&requirements, &output_after, None);
        assert!(!result.is_satisfied());
        // Only runtime stamp should be missing
        assert_eq!(result.satisfied.len(), 3);
        assert_eq!(result.missing.len(), 1);
        assert!(result.missing[0].relative_path().starts_with("runtime/"));
    }
}
