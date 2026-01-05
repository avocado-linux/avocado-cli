//! Runtime image signing command implementation.
//!
//! Signs runtime images (extension images) using configured signing keys.

use crate::utils::{
    config::load_config,
    container::{RunConfig, SdkContainer},
    image_signing::{validate_signing_key_for_use, ChecksumAlgorithm},
    output::{print_info, print_success, print_warning, OutputLevel},
    stamps::{
        generate_batch_read_stamps_script, generate_write_stamp_script, resolve_required_stamps,
        validate_stamps_batch, Stamp, StampCommand, StampComponent, StampInputs, StampOutputs,
    },
    target::resolve_target_required,
};
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Command to sign runtime images
pub struct RuntimeSignCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    #[allow(dead_code)] // Included for API consistency with other commands
    container_args: Option<Vec<String>>,
    #[allow(dead_code)] // Included for API consistency with other commands
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    sdk_arch: Option<String>,
}

impl RuntimeSignCommand {
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
            no_stamps: false,
            sdk_arch: None,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        // Validate stamps before proceeding (unless --no-stamps)
        if !self.no_stamps {
            let container_image = config
                .get_sdk_image()
                .context("No SDK container image specified in configuration")?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

            // Sign requires runtime build stamp
            let required = resolve_required_stamps(
                StampCommand::Sign,
                StampComponent::Runtime,
                Some(&self.runtime_name),
                &[],
            );

            // Batch all stamp reads into a single container invocation for performance
            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            // Validate all stamps from batch output
            let validation =
                validate_stamps_batch(&required, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                let error =
                    validation.into_error(&format!("Cannot sign runtime '{}'", self.runtime_name));
                return Err(error.into());
            }
        }

        print_info(
            &format!(
                "Signing runtime images for '{}' (target: {})",
                self.runtime_name, target_arch
            ),
            OutputLevel::Normal,
        );

        // Verify runtime exists
        let runtime_config = parsed
            .get("runtime")
            .context("No runtime configuration found")?;

        runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        // Get the list of required extensions for filtering
        let merged_runtime = config
            .get_merged_runtime_config(&self.runtime_name, &target_arch, &self.config_path)?
            .with_context(|| {
                format!(
                    "Runtime '{}' not found or has no configuration for target '{}'",
                    self.runtime_name, target_arch
                )
            })?;

        let binding = serde_yaml::Mapping::new();
        let runtime_deps = merged_runtime
            .get("dependencies")
            .and_then(|v| v.as_mapping())
            .unwrap_or(&binding);

        let mut required_extensions = HashSet::new();
        for (_dep_name, dep_spec) in runtime_deps {
            if let Some(ext_name) = dep_spec.get("ext").and_then(|v| v.as_str()) {
                required_extensions.insert(ext_name.to_string());
            }
        }

        let all_required_extensions =
            self.find_all_extension_dependencies(&config, &required_extensions, &target_arch)?;

        // Sign images
        self.sign_runtime_images(&config, &target_arch, &all_required_extensions)
            .await?;

        print_success(
            &format!("Successfully signed runtime '{}'", self.runtime_name),
            OutputLevel::Normal,
        );

        // Write sign stamp (unless --no-stamps)
        if !self.no_stamps {
            let container_image = config
                .get_sdk_image()
                .context("No SDK container image specified")?;
            let container_helper =
                SdkContainer::from_config(&self.config_path, &config)?.verbose(self.verbose);

            let inputs = StampInputs::new("sign".to_string());
            let outputs = StampOutputs::default();
            let stamp = Stamp::runtime_sign(&self.runtime_name, &target_arch, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

            if self.verbose {
                print_info(
                    &format!("Wrote sign stamp for runtime '{}'.", self.runtime_name),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(())
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
        let parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Check if this is a local extension
        if let Some(ext_config) = parsed
            .get("ext")
            .and_then(|e| e.as_mapping())
            .and_then(|table| table.get(ext_name))
        {
            // This is a local extension - check its dependencies
            if let Some(dependencies) = ext_config.get("dependencies").and_then(|d| d.as_mapping())
            {
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
                                    ext_config.get("dependencies").and_then(|d| d.as_mapping())
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
                .and_then(|v| v.as_mapping())
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
                                        ext_config.get("dependencies").and_then(|d| d.as_mapping())
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

    /// Sign runtime images using configured signing key
    pub async fn sign_runtime_images(
        &self,
        config: &crate::utils::config::Config,
        target_arch: &str,
        required_extensions: &HashSet<String>,
    ) -> Result<()> {
        // Check if runtime has signing configuration
        let runtime_signing_key_name = match config.get_runtime_signing_key(&self.runtime_name) {
            Some(keyid) => {
                // Get the key name from signing_keys mapping
                let signing_keys = config.get_signing_keys();
                signing_keys
                    .and_then(|keys| {
                        keys.iter()
                            .find(|(_, v)| *v == &keyid)
                            .map(|(k, _)| k.clone())
                    })
                    .context("Signing key ID not found in signing_keys mapping")?
            }
            None => {
                // No signing configured for this runtime
                print_warning(
                    &format!(
                        "No signing key configured for runtime '{}'. Skipping signing.",
                        self.runtime_name
                    ),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        };

        // Get the keyid for signing
        let keyid = config
            .get_runtime_signing_key(&self.runtime_name)
            .context("Failed to get signing key ID")?;

        // Get checksum algorithm (defaults to sha256)
        let checksum_str = config
            .runtime
            .as_ref()
            .and_then(|r| r.get(&self.runtime_name))
            .and_then(|rc| rc.signing.as_ref())
            .map(|s| s.checksum_algorithm.as_str())
            .unwrap_or("sha256");

        let checksum_algorithm: ChecksumAlgorithm = checksum_str.parse()?;

        print_info(
            &format!(
                "Signing runtime images with key '{}' using {} checksums",
                runtime_signing_key_name,
                checksum_algorithm.name()
            ),
            OutputLevel::Normal,
        );

        // Validate the signing key is usable
        validate_signing_key_for_use(&runtime_signing_key_name, &keyid)?;

        // Multi-pass signing workflow:
        // 1. Run container to generate checksums and save as files
        // 2. Extract checksums from volume
        // 3. Sign checksums on host
        // 4. Write signatures back to volume

        // Get SDK image for checksum generation
        let sdk_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        // Step 1: Generate checksums in container (saved as .sha256 or .blake3 files)
        self.generate_checksums_in_container(
            &checksum_algorithm,
            sdk_image,
            target_arch,
            required_extensions,
        )
        .await?;

        // Step 2: Extract checksums from volume
        let manifest = self
            .extract_checksums_from_volume(&checksum_algorithm, target_arch)
            .await?;

        if manifest.files.is_empty() {
            print_warning(
                &format!(
                    "No image files found to sign. Searched in: {}/runtimes/{}/extensions",
                    target_arch, self.runtime_name
                ),
                OutputLevel::Normal,
            );
            print_info(
                "This may indicate: (1) checksum generation failed, (2) docker cp failed, or (3) no required extensions have .raw images",
                OutputLevel::Normal,
            );
            return Ok(());
        }

        // Step 3: Sign checksums on host
        let signatures = crate::utils::image_signing::sign_hash_manifest(
            &manifest,
            &runtime_signing_key_name,
            &keyid,
        )?;

        // Step 4: Write signatures back to volume
        self.write_signatures_to_volume(&signatures).await?;

        print_success(
            &format!("Signed {} image file(s)", signatures.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Generate checksums for images in container using standard tools
    async fn generate_checksums_in_container(
        &self,
        checksum_algorithm: &ChecksumAlgorithm,
        sdk_image: &str,
        target_arch: &str,
        required_extensions: &HashSet<String>,
    ) -> Result<()> {
        if self.verbose {
            print_info("Generating checksums in container...", OutputLevel::Verbose);
        }

        // Get volume name
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager
            .get_or_create_volume(&std::env::current_dir()?)
            .await?;

        // Determine checksum command and file extension based on algorithm
        let (checksum_cmd, file_ext) = match checksum_algorithm {
            ChecksumAlgorithm::Sha256 => ("sha256sum", "sha256"),
            ChecksumAlgorithm::Blake3 => ("b3sum", "blake3"),
        };

        // Build shell script to generate checksums ONLY for required extension .raw images
        let checksum_ext = file_ext;

        // Build list of extension patterns to checksum
        let mut extension_patterns = Vec::new();
        for ext_name in required_extensions {
            // Match both with and without version: ext-name-*.raw or ext-name.raw
            extension_patterns.push(format!("{ext_name}-*.raw"));
            extension_patterns.push(format!("{ext_name}.raw"));
        }

        let pattern_checks = extension_patterns
            .iter()
            .map(|pattern| {
                format!(
                    r#"
    for file in {pattern}; do
        if [ -f "$file" ] && [ ! -f "$file.{checksum_ext}" ]; then
            echo "  Generating checksum for: $file"
            {checksum_cmd} "$file" | awk '{{print $1}}' > "$file.{checksum_ext}"
            echo "  Created: $file.{checksum_ext}"
        fi
    done"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let script = format!(
            r#"#!/bin/sh
set -e
cd /opt/_avocado/{target}

echo "=== Generating checksums for extension images in runtime-specific directory ==="

# Generate checksums ONLY for required extension .raw images in runtime-specific directory
RUNTIME_EXT_DIR="runtimes/{runtime}/extensions"
if [ -d "$RUNTIME_EXT_DIR" ]; then
    echo "Checking $RUNTIME_EXT_DIR"
    cd "$RUNTIME_EXT_DIR"
    {pattern_checks}
    cd /opt/_avocado/{target}
else
    echo "  Runtime extensions directory not found: $RUNTIME_EXT_DIR"
fi

echo "=== Checksum generation complete ==="
"#,
            target = target_arch,
            runtime = self.runtime_name,
            pattern_checks = pattern_checks
        );

        // Run container with volume mounted read-write
        let container_name = format!("avocado-checksum-gen-{}", uuid::Uuid::new_v4());
        let volume_mount = format!("{}:/opt/_avocado:rw", volume_state.volume_name);

        let run_cmd = [
            "docker",
            "run",
            "--rm",
            "--name",
            &container_name,
            "-v",
            &volume_mount,
            sdk_image,
            "sh",
            "-c",
            &script,
        ];

        let mut cmd = tokio::process::Command::new(run_cmd[0]);
        cmd.args(&run_cmd[1..]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = cmd
            .output()
            .await
            .context("Failed to run checksum generation")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Checksum generation failed: {stderr}");
        }

        if self.verbose {
            let stdout = String::from_utf8_lossy(&output.stdout);
            print_info(
                &format!("Checksum generation output:\n{stdout}"),
                OutputLevel::Verbose,
            );
        }

        Ok(())
    }

    /// Extract checksums from volume by reading checksum files
    async fn extract_checksums_from_volume(
        &self,
        checksum_algorithm: &ChecksumAlgorithm,
        target_arch: &str,
    ) -> Result<crate::utils::image_signing::HashManifest> {
        if self.verbose {
            print_info("Extracting checksums from volume...", OutputLevel::Verbose);
        }

        // Get volume name
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager
            .get_or_create_volume(&std::env::current_dir()?)
            .await?;

        // Determine file extension based on algorithm
        let file_ext = match checksum_algorithm {
            ChecksumAlgorithm::Sha256 => "sha256",
            ChecksumAlgorithm::Blake3 => "blake3",
        };

        // Create temp directory for extracted checksums
        let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

        // Create temporary container to extract checksum files
        let container_name = format!("avocado-checksum-extract-{}", uuid::Uuid::new_v4());
        let volume_mount = format!("{}:/opt/_avocado:ro", volume_state.volume_name);

        let create_cmd = [
            "docker",
            "create",
            "--name",
            &container_name,
            "-v",
            &volume_mount,
            "busybox",
            "true",
        ];

        let mut cmd = tokio::process::Command::new(create_cmd[0]);
        cmd.args(&create_cmd[1..]);
        let output = cmd
            .output()
            .await
            .context("Failed to create extract container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create extract container: {stderr}");
        }

        // Extract checksum files using docker cp
        let mut entries = Vec::new();

        // Copy checksum files from runtime-specific extensions directory
        let extensions_dir = format!("{}/runtimes/{}/extensions", target_arch, self.runtime_name);

        let search_dirs = vec![(extensions_dir.as_str(), "runtimes/extensions")];

        for (source_dir, _) in &search_dirs {
            print_info(
                &format!("Attempting to copy checksums from: /opt/_avocado/{source_dir}"),
                OutputLevel::Normal,
            );

            // Copy the entire directory (without trailing slash to copy the dir itself)
            let container_path = format!("{container_name}:/opt/_avocado/{source_dir}");
            let dest_path = temp_dir.path().to_str().unwrap();

            let cp_cmd = ["docker", "cp", &container_path, dest_path];

            let mut cmd = tokio::process::Command::new(cp_cmd[0]);
            cmd.args(&cp_cmd[1..]);
            let output = cmd.output().await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                print_info(
                    &format!("  ⚠ Docker cp failed: {}", stderr.trim()),
                    OutputLevel::Normal,
                );
            } else {
                print_info("  ✓ Copied successfully", OutputLevel::Normal);
            }
        }

        // Clean up container
        let _ = self.cleanup_container(&container_name).await;

        // Read extracted checksum files from all subdirectories
        print_info(
            &format!("Looking for .{file_ext} files in extracted directories..."),
            OutputLevel::Normal,
        );

        // Docker cp copies just the final directory, not the full path
        // So /opt/_avocado/qemuarm64/runtimes/<runtime>/extensions becomes temp_dir/extensions
        let dir_mapping = vec![(extensions_dir.as_str(), "extensions")];

        for (source_dir, dir_name) in &dir_mapping {
            let search_path = temp_dir.path().join(dir_name);

            print_info(
                &format!("  Scanning: {} -> {}", source_dir, search_path.display()),
                OutputLevel::Normal,
            );

            if !search_path.exists() {
                print_info("    Directory not found", OutputLevel::Normal);
                continue;
            }

            if let Ok(dir_entries) = std::fs::read_dir(&search_path) {
                let mut found_count = 0;
                for entry in dir_entries.flatten() {
                    let path = entry.path();

                    if path.extension().and_then(|e| e.to_str()) == Some(file_ext) {
                        found_count += 1;
                        let checksum = std::fs::read_to_string(&path)?.trim().to_string();
                        let image_name = path.file_stem().unwrap().to_str().unwrap();
                        let size = 0; // Size not needed for signing

                        // All checksums are from runtime-specific extensions directory
                        let container_path = format!(
                            "/opt/_avocado/{}/runtimes/{}/extensions/{}",
                            target_arch, self.runtime_name, image_name
                        );

                        print_info(
                            &format!("    Found checksum: {image_name}"),
                            OutputLevel::Normal,
                        );

                        entries.push(crate::utils::image_signing::HashManifestEntry {
                            container_path,
                            hash: checksum,
                            size,
                        });
                    }
                }

                if found_count == 0 {
                    print_info(
                        &format!("    No .{file_ext} files found in this directory"),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        Ok(crate::utils::image_signing::HashManifest {
            runtime: self.runtime_name.clone(),
            checksum_algorithm: checksum_algorithm.name().to_string(),
            files: entries,
        })
    }

    /// Write signatures to Docker volume
    async fn write_signatures_to_volume(
        &self,
        signatures: &[crate::utils::image_signing::SignatureData],
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!(
                    "Writing {} signature file(s) to volume...",
                    signatures.len()
                ),
                OutputLevel::Verbose,
            );
        }

        // Get volume name
        let volume_manager =
            crate::utils::volume::VolumeManager::new("docker".to_string(), self.verbose);
        let volume_state = volume_manager
            .get_or_create_volume(&std::env::current_dir()?)
            .await?;

        // Use SdkContainer's write_signatures_to_volume method
        let container = SdkContainer::new().verbose(self.verbose);
        container
            .write_signatures_to_volume(&volume_state.volume_name, signatures)
            .await?;

        Ok(())
    }

    /// Clean up a container
    async fn cleanup_container(&self, container_name: &str) -> Result<()> {
        let rm_cmd = ["docker", "rm", "-f", container_name];

        let mut cmd = tokio::process::Command::new(rm_cmd[0]);
        cmd.args(&rm_cmd[1..]);
        let _ = cmd.output().await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeSignCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }
}
