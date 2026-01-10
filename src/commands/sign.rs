//! Sign command implementation that signs runtime images.
//!
//! This is a convenience command that wraps `avocado runtime sign`.
//! It signs all runtimes with signing configuration, or a specific runtime with `-r`.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::commands::runtime::RuntimeSignCommand;
use crate::utils::{
    config::{ComposedConfig, Config},
    output::{print_info, print_success, OutputLevel},
};

/// Implementation of the 'sign' command that signs runtime images.
pub struct SignCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Runtime name to sign (if not provided, signs all runtimes with signing config)
    pub runtime: Option<String>,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl SignCommand {
    /// Create a new SignCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        runtime: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            runtime,
            target,
            container_args,
            dnf_args,
            composed_config: None,
        }
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Execute the sign command
    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;

        // Early target validation and logging - fail fast if target is unsupported
        let target = crate::utils::target::validate_and_log_target(self.target.as_deref(), config)?;

        // If a specific runtime is requested, sign only that runtime
        if let Some(ref runtime_name) = self.runtime {
            return self
                .sign_single_runtime(runtime_name, &target, Arc::clone(&composed))
                .await;
        }

        // Otherwise, sign all runtimes that have signing configuration
        self.sign_all_runtimes(&composed, &target).await
    }

    /// Sign a single runtime
    async fn sign_single_runtime(
        &self,
        runtime_name: &str,
        target: &str,
        composed: Arc<ComposedConfig>,
    ) -> Result<()> {
        print_info(
            &format!("Signing runtime '{runtime_name}' for target '{target}'"),
            OutputLevel::Normal,
        );

        let sign_cmd = RuntimeSignCommand::new(
            runtime_name.to_string(),
            self.config_path.clone(),
            self.verbose,
            Some(target.to_string()),
            self.container_args.clone(),
            self.dnf_args.clone(),
        )
        .with_composed_config(composed);

        sign_cmd
            .execute()
            .await
            .with_context(|| format!("Failed to sign runtime '{runtime_name}'"))?;

        Ok(())
    }

    /// Sign all runtimes that have signing configuration
    async fn sign_all_runtimes(&self, composed: &Arc<ComposedConfig>, target: &str) -> Result<()> {
        let config = &composed.config;
        let parsed = &composed.merged_value;

        let runtime_section = parsed
            .get("runtimes")
            .and_then(|r| r.as_mapping())
            .ok_or_else(|| anyhow::anyhow!("No runtime configuration found"))?;

        // Collect runtimes that have signing configuration
        let mut runtimes_to_sign = Vec::new();
        let mut runtimes_with_unresolved_keys = Vec::new();

        for runtime_name_val in runtime_section.keys() {
            if let Some(runtime_name) = runtime_name_val.as_str() {
                // Check if this runtime declares a signing key
                if let Some(declared_key) = config.get_runtime_signing_key_name(runtime_name) {
                    // Check if the signing key can be resolved
                    if config.get_runtime_signing_key(runtime_name).is_some() {
                        // Check target compatibility
                        let merged_runtime = config.get_merged_runtime_config(
                            runtime_name,
                            target,
                            &self.config_path,
                        )?;

                        if let Some(merged_value) = merged_runtime {
                            // Check if runtime has explicit target
                            if let Some(runtime_target) =
                                merged_value.get("target").and_then(|t| t.as_str())
                            {
                                // Runtime has explicit target - only include if it matches
                                if runtime_target == target {
                                    runtimes_to_sign.push(runtime_name.to_string());
                                }
                            } else {
                                // Runtime has no target specified - include for all targets
                                runtimes_to_sign.push(runtime_name.to_string());
                            }
                        }
                    } else {
                        // Runtime declares a signing key but it can't be resolved
                        runtimes_with_unresolved_keys
                            .push((runtime_name.to_string(), declared_key));
                    }
                }
            }
        }

        // If any runtimes have unresolved signing keys, return an error
        if !runtimes_with_unresolved_keys.is_empty() {
            let runtime_details: Vec<String> = runtimes_with_unresolved_keys
                .iter()
                .map(|(runtime, key)| format!("  - runtime '{runtime}' references key '{key}'"))
                .collect();

            anyhow::bail!(
                "The following runtimes have signing configuration with keys that could not be resolved:\n\
                {}\n\n\
                Please check that:\n\
                  1. A top-level `signing_keys` section exists in your config (note: underscore, not hyphen)\n\
                  2. The referenced keys are defined in the `signing_keys` section\n\
                  3. The keys are available on this host (check with: avocado signing-keys list)",
                runtime_details.join("\n")
            );
        }

        if runtimes_to_sign.is_empty() {
            print_info(
                "No runtimes with signing configuration found.",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        print_info(
            &format!(
                "Signing {} runtime(s) with signing configuration...",
                runtimes_to_sign.len()
            ),
            OutputLevel::Normal,
        );

        for runtime_name in &runtimes_to_sign {
            if self.verbose {
                print_info(
                    &format!("Signing runtime '{runtime_name}'"),
                    OutputLevel::Normal,
                );
            }

            let sign_cmd = RuntimeSignCommand::new(
                runtime_name.clone(),
                self.config_path.clone(),
                self.verbose,
                Some(target.to_string()),
                self.container_args.clone(),
                self.dnf_args.clone(),
            )
            .with_composed_config(Arc::clone(composed));

            sign_cmd
                .execute()
                .await
                .with_context(|| format!("Failed to sign runtime '{runtime_name}'"))?;
        }

        print_success(
            &format!("Successfully signed {} runtime(s)!", runtimes_to_sign.len()),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = SignCommand::new(
            "avocado.yaml".to_string(),
            true,
            Some("my-runtime".to_string()),
            Some("x86_64".to_string()),
            Some(vec!["--privileged".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(cmd.verbose);
        assert_eq!(cmd.runtime, Some("my-runtime".to_string()));
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.container_args, Some(vec!["--privileged".to_string()]));
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_new_all_runtimes() {
        let cmd = SignCommand::new("config.toml".to_string(), false, None, None, None, None);

        assert_eq!(cmd.config_path, "config.toml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.runtime, None);
        assert_eq!(cmd.target, None);
        assert_eq!(cmd.container_args, None);
        assert_eq!(cmd.dnf_args, None);
    }
}
