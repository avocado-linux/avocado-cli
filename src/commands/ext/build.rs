use anyhow::{Context, Result};

use crate::commands::sdk::SdkCompileCommand;
use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target_required;

#[derive(Debug, Clone)]
struct OverlayConfig {
    dir: String,
    mode: OverlayMode,
}

#[derive(Debug, Clone, PartialEq)]
enum OverlayMode {
    Merge,  // Default: rsync -a (safe merging)
    Opaque, // cp -r (replace directory contents)
}

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
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let _parsed: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Merge container args from config and CLI (similar to SDK commands)
        let processed_container_args =
            config.merge_sdk_container_args(self.container_args.as_ref());
        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        // Find extension using comprehensive lookup
        let extension_location = config
            .find_extension_in_dependency_tree(&self.config_path, &self.extension, &target)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Get the config path where this extension is actually defined
        let ext_config_path = match &extension_location {
            ExtensionLocation::Local { config_path, .. } => config_path.clone(),
            ExtensionLocation::External { config_path, .. } => config_path.clone(),
        };

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

        // Get merged extension configuration with target-specific overrides
        // Use the config path where the extension is actually defined for proper interpolation
        let ext_config = config
            .get_merged_ext_config(&self.extension, &target, &ext_config_path)?
            .ok_or_else(|| {
                anyhow::anyhow!("Extension '{}' not found in configuration.", self.extension)
            })?;

        // Handle compile dependencies with install scripts before building the extension
        self.handle_compile_dependencies(&config, &ext_config, &target)
            .await?;

        // Get extension types from the types array (defaults to ["sysext", "confext"])
        let ext_types = ext_config
            .get("types")
            .and_then(|v| v.as_sequence())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["sysext", "confext"]);

        // Get enable_services from configuration
        let enable_services = ext_config
            .get("enable_services")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Get modprobe modules from configuration
        let modprobe_modules = ext_config
            .get("modprobe")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Get on_merge commands from configuration
        let on_merge_commands = ext_config
            .get("on_merge")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Get reload_service_manager configuration (defaults to false)
        let reload_service_manager = ext_config
            .get("reload_service_manager")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Get users and groups configuration
        let users_config = ext_config.get("users").and_then(|v| v.as_mapping());
        let groups_config = ext_config.get("groups").and_then(|v| v.as_mapping());

        // Validate that confext is present if enable_services is used
        if !enable_services.is_empty() && !ext_types.contains(&"confext") {
            print_error(
                &format!(
                    "Warning: Extension '{}' has enable_services configured but 'confext' is not in types. \
                    Service linking requires a confext. Please add 'confext' to the types array.",
                    self.extension
                ),
                OutputLevel::Normal,
            );
        }

        let ext_scopes = ext_config
            .get("scopes")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["system".to_string()]);

        let sysext_scopes = ext_config
            .get("sysext_scopes")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| ext_scopes.clone());

        let confext_scopes = ext_config
            .get("confext_scopes")
            .and_then(|v| v.as_sequence())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| ext_scopes.clone());

        // Get extension version
        let ext_version = ext_config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0");

        // Validate semver format
        Self::validate_semver(ext_version).with_context(|| {
            format!(
                "Extension '{}' has invalid version '{}'. Version must be in semantic versioning format (e.g., '1.0.0', '2.1.3')",
                self.extension, ext_version
            )
        })?;

        // Get overlay configuration
        let overlay_config = ext_config.get("overlay").map(|v| {
            if let Some(dir_str) = v.as_str() {
                // Simple string format: overlay = "directory"
                OverlayConfig {
                    dir: dir_str.to_string(),
                    mode: OverlayMode::Merge, // Default to merge mode
                }
            } else if let Some(table) = v.as_mapping() {
                // Table format: overlay = {dir = "directory", mode = "opaque"}
                let dir = table
                    .get("dir")
                    .and_then(|d| d.as_str())
                    .unwrap_or("overlay")
                    .to_string();

                let mode = match table.get("mode").and_then(|m| m.as_str()) {
                    Some("opaque") => OverlayMode::Opaque,
                    _ => OverlayMode::Merge, // Default to merge mode
                };

                OverlayConfig { dir, mode }
            } else {
                // Fallback for invalid format
                OverlayConfig {
                    dir: "overlay".to_string(),
                    mode: OverlayMode::Merge,
                }
            }
        });

        // Get SDK configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        // Resolve target with proper precedence
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        // Initialize SDK container helper
        let container_helper = SdkContainer::from_config(&self.config_path, &config)?;

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
                        overlay_config.as_ref(),
                        repo_url.as_ref(),
                        repo_release.as_ref(),
                        &processed_container_args,
                        &modprobe_modules,
                        &on_merge_commands,
                        users_config,
                        groups_config,
                        reload_service_manager,
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
                        overlay_config.as_ref(),
                        repo_url.as_ref(),
                        repo_release.as_ref(),
                        &processed_container_args,
                        &enable_services,
                        &on_merge_commands,
                        users_config,
                        groups_config,
                        reload_service_manager,
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
        overlay_config: Option<&OverlayConfig>,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        processed_container_args: &Option<Vec<String>>,
        modprobe_modules: &[String],
        on_merge_commands: &[String],
        users_config: Option<&serde_yaml::Mapping>,
        groups_config: Option<&serde_yaml::Mapping>,
        reload_service_manager: bool,
    ) -> Result<bool> {
        // Create the build script for sysext extension
        let build_script = self.create_sysext_build_script(
            ext_version,
            ext_scopes,
            overlay_config,
            modprobe_modules,
            on_merge_commands,
            users_config,
            groups_config,
            reload_service_manager,
        );

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
        overlay_config: Option<&OverlayConfig>,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        processed_container_args: &Option<Vec<String>>,
        enable_services: &[String],
        on_merge_commands: &[String],
        users_config: Option<&serde_yaml::Mapping>,
        groups_config: Option<&serde_yaml::Mapping>,
        reload_service_manager: bool,
    ) -> Result<bool> {
        // Create the build script for confext extension
        let build_script = self.create_confext_build_script(
            ext_version,
            ext_scopes,
            overlay_config,
            enable_services,
            on_merge_commands,
            users_config,
            groups_config,
            reload_service_manager,
        );

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

    #[allow(clippy::too_many_arguments)]
    fn create_sysext_build_script(
        &self,
        ext_version: &str,
        ext_scopes: &[String],
        overlay_config: Option<&OverlayConfig>,
        modprobe_modules: &[String],
        on_merge_commands: &[String],
        users_config: Option<&serde_yaml::Mapping>,
        groups_config: Option<&serde_yaml::Mapping>,
        reload_service_manager: bool,
    ) -> String {
        let overlay_section = if let Some(overlay_config) = overlay_config {
            match overlay_config.mode {
                OverlayMode::Merge => format!(
                    r#"
# Merge overlay directory into extension sysroot
if [ -d "/opt/src/{}" ]; then
    echo "Merging overlay directory '{}' into extension sysroot with root:root ownership"
    # Use rsync to merge directories and set ownership during copy
    rsync -a --chown=root:root /opt/src/{}/ "$AVOCADO_EXT_SYSROOTS/{}/"
else
    echo "Error: Overlay directory '{}' not found in source"
    exit 1
fi
"#,
                    overlay_config.dir,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir
                ),
                OverlayMode::Opaque => format!(
                    r#"
# Copy overlay directory to extension sysroot (opaque mode)
if [ -d "/opt/src/{}" ]; then
    echo "Copying overlay directory '{}' to extension sysroot (opaque mode)"
    # Use cp -r to replace directory contents completely
    cp -r /opt/src/{}/* "$AVOCADO_EXT_SYSROOTS/{}/"
    # Fix ownership to root:root for copied overlay files only
    echo "Setting ownership to root:root for overlay files"
    find "/opt/src/{}" -mindepth 1 | while IFS= read -r srcpath; do
        relpath="$(echo "$srcpath" | sed "s|^/opt/src/{}||" | sed "s|^/||")"
        if [ -n "$relpath" ]; then
            destpath="$AVOCADO_EXT_SYSROOTS/{}/$relpath"
            if [ -e "$destpath" ]; then
                chown root:root "$destpath" 2>/dev/null || true
            fi
        fi
    done
else
    echo "Error: Overlay directory '{}' not found in source"
    exit 1
fi
"#,
                    overlay_config.dir,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir
                ),
            }
        } else {
            String::new()
        };

        // Create modprobe commands section for AVOCADO_ON_MERGE
        let modprobe_section = if !modprobe_modules.is_empty() {
            modprobe_modules
                .iter()
                .map(|module| {
                    format!(
                        r#"    echo "AVOCADO_ON_MERGE=\"modprobe {module}\"" >> "$release_file"
    echo "[INFO] Added AVOCADO_ON_MERGE=\"modprobe {module}\" to release file""#
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        // Create custom on_merge commands section
        let custom_on_merge_section = if !on_merge_commands.is_empty() {
            on_merge_commands
                .iter()
                .map(|command| {
                    format!(
                        r#"echo "AVOCADO_ON_MERGE=\"{command}\"" >> "$release_file"
echo "[INFO] Added custom on_merge command to release file: {command}""#
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        let users_section = self.create_users_script_section(users_config, groups_config);

        format!(
            r#"
set -e
{}{}
release_dir="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/extension-release.d"
release_file="$release_dir/extension-release.{}-{}"
modules_dir="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/modules"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER={}" >> "$release_file"
echo "SYSEXT_SCOPE={}" >> "$release_file"

# Check if extension includes kernel modules and add AVOCADO_ON_MERGE if needed
if [ -d "$modules_dir" ] && [ -n "$(find "$modules_dir" -name "*.ko" -o -name "*.ko.xz" -o -name "*.ko.gz" 2>/dev/null | head -n 1)" ]; then
    echo "AVOCADO_ON_MERGE=\"depmod\"" >> "$release_file"
    echo "[INFO] Found kernel modules in extension '{}', added AVOCADO_ON_MERGE=\"depmod\" to release file"
    {}
fi

# Check if extension includes sysusers.d config files and add systemd-sysusers to AVOCADO_ON_MERGE if needed
sysusers_dir1="$AVOCADO_EXT_SYSROOTS/{}/usr/local/lib/sysusers.d"
sysusers_dir2="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/sysusers.d"
if ([ -d "$sysusers_dir1" ] && [ -n "$(find "$sysusers_dir1" -name "*.conf" 2>/dev/null | head -n 1)" ]) || \
   ([ -d "$sysusers_dir2" ] && [ -n "$(find "$sysusers_dir2" -name "*.conf" 2>/dev/null | head -n 1)" ]); then
    echo "AVOCADO_ON_MERGE=\"systemd-sysusers\"" >> "$release_file"
    echo "[INFO] Found sysusers.d config files in extension '{}', added AVOCADO_ON_MERGE=\"systemd-sysusers\" to release file"
fi

# Add custom AVOCADO_ON_MERGE commands if specified
{}
"#,
            overlay_section,
            users_section,
            self.extension,
            self.extension,
            ext_version,
            self.extension,
            if reload_service_manager { "1" } else { "0" },
            ext_scopes.join(" "),
            self.extension,
            modprobe_section,
            self.extension,
            self.extension,
            self.extension,
            custom_on_merge_section
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_confext_build_script(
        &self,
        ext_version: &str,
        ext_scopes: &[String],
        overlay_config: Option<&OverlayConfig>,
        enable_services: &[String],
        on_merge_commands: &[String],
        users_config: Option<&serde_yaml::Mapping>,
        groups_config: Option<&serde_yaml::Mapping>,
        reload_service_manager: bool,
    ) -> String {
        let overlay_section = if let Some(overlay_config) = overlay_config {
            match overlay_config.mode {
                OverlayMode::Merge => format!(
                    r#"
# Merge overlay directory into extension sysroot
if [ -d "/opt/src/{}" ]; then
    echo "Merging overlay directory '{}' into extension sysroot with root:root ownership"
    # Use rsync to merge directories and set ownership during copy
    rsync -a --chown=root:root /opt/src/{}/ "$AVOCADO_EXT_SYSROOTS/{}/"
else
    echo "Error: Overlay directory '{}' not found in source"
    exit 1
fi
"#,
                    overlay_config.dir,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir
                ),
                OverlayMode::Opaque => format!(
                    r#"
# Copy overlay directory to extension sysroot (opaque mode)
if [ -d "/opt/src/{}" ]; then
    echo "Copying overlay directory '{}' to extension sysroot (opaque mode)"
    # Use cp -r to replace directory contents completely
    cp -r /opt/src/{}/* "$AVOCADO_EXT_SYSROOTS/{}/"
    # Fix ownership to root:root for copied overlay files only
    echo "Setting ownership to root:root for overlay files"
    find "/opt/src/{}" -mindepth 1 | while IFS= read -r srcpath; do
        relpath="$(echo "$srcpath" | sed "s|^/opt/src/{}||" | sed "s|^/||")"
        if [ -n "$relpath" ]; then
            destpath="$AVOCADO_EXT_SYSROOTS/{}/$relpath"
            if [ -e "$destpath" ]; then
                chown root:root "$destpath" 2>/dev/null || true
            fi
        fi
    done
else
    echo "Error: Overlay directory '{}' not found in source"
    exit 1
fi
"#,
                    overlay_config.dir,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir,
                    overlay_config.dir,
                    self.extension,
                    overlay_config.dir
                ),
            }
        } else {
            String::new()
        };

        // Create service linking section
        let service_linking_section = if !enable_services.is_empty() {
            let mut service_commands = Vec::new();
            for service in enable_services {
                service_commands.push(format!(
                    r#"
# Link service file for {}
service_file="$AVOCADO_EXT_SYSROOTS/{}/usr/lib/systemd/system/{}"
service_link_dir="$AVOCADO_EXT_SYSROOTS/{}/etc/systemd/system/multi-user.target.upholds"
service_link="$service_link_dir/{}"

if [ -f "$service_file" ]; then
    echo "Found service file: $service_file"
    mkdir -p "$service_link_dir"
    ln -sf "/usr/lib/systemd/system/{}" "$service_link"
    echo "Created systemd service link: $service_link -> /usr/lib/systemd/system/{}"
else
    echo "Warning: Service file {} not found in extension sysroot"
fi"#,
                    service,
                    self.extension,
                    service,
                    self.extension,
                    service,
                    service,
                    service,
                    service
                ));
            }
            service_commands.join("\n")
        } else {
            String::new()
        };

        let users_section = self.create_users_script_section(users_config, groups_config);

        // Create custom on_merge commands section for confext
        let custom_on_merge_section = if !on_merge_commands.is_empty() {
            on_merge_commands
                .iter()
                .map(|command| {
                    format!(
                        r#"echo "AVOCADO_ON_MERGE=\"{command}\"" >> "$release_file"
echo "[INFO] Added custom on_merge command to release file: {command}""#
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        format!(
            r#"
set -e
{}{}
release_dir="$AVOCADO_EXT_SYSROOTS/{}/etc/extension-release.d"
release_file="$release_dir/extension-release.{}-{}"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER={}" >> "$release_file"
echo "CONFEXT_SCOPE={}" >> "$release_file"

# Check if extension includes sysusers.d config files and add systemd-sysusers to AVOCADO_ON_MERGE if needed
sysusers_etc_dir="$AVOCADO_EXT_SYSROOTS/{}/etc/sysusers.d"
if [ -d "$sysusers_etc_dir" ] && [ -n "$(find "$sysusers_etc_dir" -name "*.conf" 2>/dev/null | head -n 1)" ]; then
    echo "AVOCADO_ON_MERGE=\"systemd-sysusers\"" >> "$release_file"
    echo "[INFO] Found sysusers.d config files in extension '{}', added AVOCADO_ON_MERGE=\"systemd-sysusers\" to release file"
fi

# Check if extension includes ld.so.conf.d config files and add ldconfig to AVOCADO_ON_MERGE if needed
ldso_etc_dir="$AVOCADO_EXT_SYSROOTS/{}/etc/ld.so.conf.d"
if [ -d "$ldso_etc_dir" ] && [ -n "$(find "$ldso_etc_dir" -name "*.conf" 2>/dev/null | head -n 1)" ]; then
    echo "AVOCADO_ON_MERGE=\"ldconfig\"" >> "$release_file"
    echo "[INFO] Found ld.so.conf.d config files in extension '{}', added AVOCADO_ON_MERGE=\"ldconfig\" to release file"
fi

# Add custom AVOCADO_ON_MERGE commands if specified
{}
{}
"#,
            overlay_section,
            users_section,
            self.extension,
            self.extension,
            ext_version,
            if reload_service_manager { "1" } else { "0" },
            ext_scopes.join(" "),
            self.extension,
            self.extension,
            self.extension,
            self.extension,
            custom_on_merge_section,
            service_linking_section
        )
    }

    /// Creates a script section for handling user and group configuration
    /// This will copy passwd/shadow/group files and create/modify users and groups
    fn create_users_script_section(
        &self,
        users_config: Option<&serde_yaml::Mapping>,
        groups_config: Option<&serde_yaml::Mapping>,
    ) -> String {
        // If neither users nor groups are configured, return empty string
        if users_config.is_none() && groups_config.is_none() {
            return String::new();
        }

        let mut script_lines = Vec::new();
        let mut has_valid_users = false;
        script_lines.push("\n# Copy and manage user authentication files".to_string());

        // Copy authentication files from rootfs
        script_lines.push(format!(
            r#"
# Copy authentication files from rootfs to extension
echo "Copying /etc/passwd, /etc/shadow, and /etc/group from rootfs to extension"
mkdir -p "$AVOCADO_EXT_SYSROOTS/{}/etc"
cp "$AVOCADO_PREFIX/rootfs/etc/passwd" "$AVOCADO_EXT_SYSROOTS/{}/etc/passwd"
cp "$AVOCADO_PREFIX/rootfs/etc/shadow" "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow"
cp "$AVOCADO_PREFIX/rootfs/etc/group" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
"#,
            self.extension, self.extension, self.extension, self.extension
        ));

        // Auto-incrementing counters for uid/gid starting at 1000
        script_lines.push(
            "# Auto-incrementing counters for uid/gid\nCURRENT_UID=1000\nCURRENT_GID=1000\n"
                .to_string(),
        );

        // Process groups first (they might be referenced by users)
        if let Some(groups) = groups_config {
            script_lines.push("\n# Create groups".to_string());

            for (groupname_val, group_config) in groups {
                // Convert groupname from Value to String
                let groupname = match groupname_val.as_str() {
                    Some(name) => name,
                    None => continue, // Skip if groupname is not a string
                };

                if let Some(group_table) = group_config.as_mapping() {
                    // Parse comprehensive group configuration with defaults
                    let gid = if let Some(gid_value) = group_table.get("gid") {
                        if let Some(gid_num) = gid_value.as_i64() {
                            gid_num.to_string()
                        } else if let Some(gid_num) = gid_value.as_u64() {
                            gid_num.to_string()
                        } else {
                            "$CURRENT_GID".to_string()
                        }
                    } else {
                        "$CURRENT_GID".to_string()
                    };

                    let system_group = group_table
                        .get("system")
                        .and_then(|s| s.as_bool())
                        .unwrap_or(false);

                    let password = group_table
                        .get("password")
                        .and_then(|p| p.as_str())
                        .unwrap_or(""); // Default: no group password

                    let members = if let Some(members_value) = group_table.get("members") {
                        if let Some(members_array) = members_value.as_sequence() {
                            members_array
                                .iter()
                                .filter_map(|m| m.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        } else {
                            "".to_string()
                        }
                    } else {
                        "".to_string()
                    };

                    let _admins = if let Some(admins_value) = group_table.get("admins") {
                        if let Some(admins_array) = admins_value.as_sequence() {
                            admins_array
                                .iter()
                                .filter_map(|a| a.as_str())
                                .collect::<Vec<_>>()
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    // Escape password for potential gshadow entry
                    let _escaped_group_password = password.replace("/", "\\/").replace("&", "\\&");

                    let system_type = if system_group { " (system group)" } else { "" };
                    let password_note = if !password.is_empty() {
                        " with password"
                    } else {
                        ""
                    };
                    let members_msg = if !members.is_empty() {
                        format!(" and members: {members}")
                    } else {
                        "".to_string()
                    };
                    let password_config = if !password.is_empty() {
                        format!("\n# Set group password for '{groupname}'\necho \"Note: Group password configured for '{groupname}'\"")
                    } else {
                        "".to_string()
                    };

                    script_lines.push(format!(
                        r#"
# Create group '{}'{}
echo "Creating group '{}'"{}
if ! grep -q "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"; then
    echo "{}:x:{}:{}" >> "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
    echo "Group '{}' created with GID {}{}"
    if [ "{}" = "$CURRENT_GID" ]; then
        CURRENT_GID=$((CURRENT_GID + 1))
    fi
else
    echo "Group '{}' already exists, updating members"
    # Update members if specified
    if [ -n "{}" ]; then
        sed -i "s|^{}:x:{}:.*$|{}:x:{}:{}|" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
        echo "Updated members for group '{}'"
    fi
fi{}"#,
                        groupname,
                        system_type,
                        groupname,
                        password_note,
                        groupname,
                        self.extension,
                        groupname,
                        gid,
                        members,
                        self.extension,
                        groupname,
                        gid,
                        members_msg,
                        gid,
                        groupname,
                        members,
                        groupname,
                        gid,
                        groupname,
                        gid,
                        members,
                        self.extension,
                        groupname,
                        password_config
                    ));
                } else {
                    // Simple group with just GID auto-assignment
                    script_lines.push(format!(
                        r#"
# Create group '{}'
echo "Creating group '{}'"
if ! grep -q "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"; then
    echo "{}:x:$CURRENT_GID:" >> "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
    echo "Group '{}' created with GID $CURRENT_GID"
    CURRENT_GID=$((CURRENT_GID + 1))
else
    echo "Group '{}' already exists"
fi"#,
                        groupname,
                        groupname,
                        groupname,
                        self.extension,
                        groupname,
                        self.extension,
                        groupname,
                        groupname
                    ));
                }
            }
        }

        // Process users
        if let Some(users) = users_config {
            let mut user_script_lines = Vec::new();

            for (username_val, user_config) in users {
                // Convert username from Value to String
                let username = match username_val.as_str() {
                    Some(name) => name,
                    None => continue, // Skip if username is not a string
                };

                if let Some(user_table) = user_config.as_mapping() {
                    // Check if user has password field - if not, create with disabled login
                    let password = user_table
                        .get("password")
                        .and_then(|p| p.as_str())
                        .unwrap_or("*"); // Default to no login allowed

                    has_valid_users = true;

                    // Parse comprehensive user configuration with defaults
                    let uid = if let Some(uid_value) = user_table.get("uid") {
                        if let Some(uid_num) = uid_value.as_i64() {
                            uid_num.to_string()
                        } else {
                            "$CURRENT_UID".to_string()
                        }
                    } else {
                        "$CURRENT_UID".to_string()
                    };

                    let gid = if let Some(gid_value) = user_table.get("gid") {
                        if let Some(gid_num) = gid_value.as_i64() {
                            gid_num.to_string()
                        } else {
                            "$CURRENT_UID".to_string() // Default to same as UID for user private groups
                        }
                    } else {
                        "$CURRENT_UID".to_string()
                    };

                    let gecos = user_table
                        .get("gecos")
                        .and_then(|g| g.as_str())
                        .unwrap_or(username); // Default to username

                    let default_home = format!("/home/{username}");
                    let home = user_table
                        .get("home")
                        .and_then(|h| h.as_str())
                        .unwrap_or(&default_home); // Default to /home/username

                    let shell = user_table
                        .get("shell")
                        .and_then(|s| s.as_str())
                        .unwrap_or("/bin/sh"); // Default shell

                    let groups = if let Some(groups_value) = user_table.get("groups") {
                        if let Some(groups_array) = groups_value.as_sequence() {
                            groups_array
                                .iter()
                                .filter_map(|g| g.as_str())
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        } else {
                            vec![username.to_string()] // Default to user's own group
                        }
                    } else {
                        vec![username.to_string()] // Default to user's own group
                    };

                    let _primary_group = groups.first().map(|s| s.as_str()).unwrap_or(username);

                    // Shadow file attributes with defaults
                    let last_change = user_table
                        .get("last_change")
                        .and_then(|l| l.as_i64())
                        .unwrap_or(19000); // Default to a reasonable epoch day

                    let min_days = user_table
                        .get("min_days")
                        .and_then(|m| m.as_i64())
                        .unwrap_or(0); // Default: no minimum

                    let max_days = user_table
                        .get("max_days")
                        .and_then(|m| m.as_i64())
                        .unwrap_or(99999); // Default: no maximum

                    let warn_days = user_table
                        .get("warn_days")
                        .and_then(|w| w.as_i64())
                        .unwrap_or(7); // Default: warn 7 days before

                    let inactive_days = user_table
                        .get("inactive_days")
                        .and_then(|i| i.as_i64())
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "".to_string()); // Default: no inactive period

                    let expire_date = user_table
                        .get("expire_date")
                        .and_then(|e| e.as_i64())
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "".to_string()); // Default: no expiration

                    let disabled = user_table
                        .get("disabled")
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false);

                    let system_user = user_table
                        .get("system")
                        .and_then(|s| s.as_bool())
                        .unwrap_or(false);

                    // Escape special characters in password for sed
                    // Note: We use | as sed delimiter to avoid conflicts with / in passwords
                    // We only need to escape characters that have special meaning in sed replacement strings
                    let escaped_password = password
                        .replace("\\", "\\\\") // Escape backslashes first
                        .replace("&", "\\&") // Escape ampersands (sed replacement reference)
                        .replace("$", "\\$"); // Escape dollar signs (sed end-of-line anchor)

                    let warning_message = if password.is_empty() {
                        format!("\necho \"[WARNING] User '{username}' will be able to login with NO PASSWORD\"")
                    } else {
                        String::new()
                    };

                    // Create user in passwd file
                    user_script_lines.push(format!(
                        r#"
# Create user '{}'
echo "Creating user '{}'{}"{}
if ! grep -q "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/passwd"; then
    # Add user to passwd file with comprehensive attributes
    echo "{}:x:{}:{}:{}:{}:{}" >> "$AVOCADO_EXT_SYSROOTS/{}/etc/passwd"
    echo "User '{}' created with UID {}, GID {}, home '{}', shell '{}'"

    if [ "{}" = "$CURRENT_UID" ]; then
        CURRENT_UID=$((CURRENT_UID + 1))
    fi
else
    echo "User '{}' already exists, updating attributes"
fi"#,
                        username,
                        username,
                        if system_user { " (system user)" } else { "" },
                        warning_message,
                        username,
                        self.extension,
                        username,
                        uid,
                        gid,
                        gecos,
                        home,
                        shell,
                        self.extension,
                        username,
                        uid,
                        gid,
                        home,
                        shell,
                        uid,
                        username
                    ));

                    // Create/update user in shadow file with comprehensive attributes
                    user_script_lines.push(format!(
                        r#"
# Set password and shadow attributes for user '{}'
echo "Setting password and aging policy for user '{}'"
if grep -q "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow"; then
    # Update existing user's shadow entry completely
    sed -i "s|^{}:.*$|{}:{}:{}:{}:{}:{}:{}:{}:|" "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow"
    echo "Updated shadow entry for existing user '{}'"
else
    # Add new user to shadow file with full attributes
    echo "{}:{}:{}:{}:{}:{}:{}:{}:" >> "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow"
    echo "Added new user '{}' to shadow file"
fi{}"#,
                        username,
                        username,
                        username,
                        self.extension,
                        username,
                        username,
                        escaped_password,
                        last_change,
                        min_days,
                        max_days,
                        warn_days,
                        inactive_days,
                        expire_date,
                        self.extension,
                        username,
                        username,
                        escaped_password,
                        last_change,
                        min_days,
                        max_days,
                        warn_days,
                        inactive_days,
                        expire_date,
                        self.extension,
                        username,
                        if disabled {
                            "\necho \"Note: User account is marked as disabled\""
                        } else {
                            ""
                        }
                    ));

                    // Add user to additional groups if specified
                    if groups.len() > 1 {
                        user_script_lines.push(format!(
                            r#"
# Add user '{username}' to additional groups"#
                        ));

                        for group in &groups[1..] {
                            // Skip primary group
                            user_script_lines.push(format!(
                                r#"
if grep -q "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"; then
    # Add user to group if not already present
    if ! grep "^{}:" "$AVOCADO_EXT_SYSROOTS/{}/etc/group" | grep -q "{}"; then
        sed -i "s|^{}:\([^:]*\):\([^:]*\):\(.*\)$|{}:\1:\2:\3,{}|" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
        echo "Added user '{}' to group '{}'"
    fi
else
    echo "Warning: Group '{}' not found, cannot add user '{}'"
fi"#,
                                group, self.extension, group, self.extension, username, group, group, username, self.extension, username, group, group, username
                            ));
                        }
                    }
                }
            }

            // Add user scripts to main script if there are valid users
            if has_valid_users {
                script_lines.push("\n# Create and configure users".to_string());
                script_lines.extend(user_script_lines);
            }
        }

        // Set proper permissions only if we processed any users or groups
        if groups_config.is_some() || has_valid_users {
            script_lines.push(format!(
                r#"
# Set proper ownership and permissions for authentication files
chown root:root "$AVOCADO_EXT_SYSROOTS/{}/etc/passwd" "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow" "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
chmod 644 "$AVOCADO_EXT_SYSROOTS/{}/etc/passwd"
chmod 640 "$AVOCADO_EXT_SYSROOTS/{}/etc/shadow"
chmod 644 "$AVOCADO_EXT_SYSROOTS/{}/etc/group"
echo "Set proper permissions on authentication files""#,
                self.extension, self.extension, self.extension, self.extension, self.extension, self.extension
            ));
        }

        script_lines.join("")
    }
    /// Handle compile dependencies with install scripts
    async fn handle_compile_dependencies(
        &self,
        config: &Config,
        ext_config: &serde_yaml::Value,
        target: &str,
    ) -> Result<()> {
        // Get dependencies from extension configuration
        let dependencies = ext_config.get("dependencies").and_then(|v| v.as_mapping());

        let Some(deps_table) = dependencies else {
            return Ok(());
        };

        // Find compile dependencies with install scripts
        let mut compile_install_deps = Vec::new();

        for (dep_name_val, dep_spec) in deps_table {
            if let Some(dep_name) = dep_name_val.as_str() {
                if let serde_yaml::Value::Mapping(spec_map) = dep_spec {
                    // Check for the new syntax: { compile = "section-name", install = "script.sh" }
                    if let (
                        Some(serde_yaml::Value::String(compile_section)),
                        Some(serde_yaml::Value::String(install_script)),
                    ) = (spec_map.get("compile"), spec_map.get("install"))
                    {
                        compile_install_deps.push((
                            dep_name.to_string(),
                            compile_section.clone(),
                            install_script.clone(),
                        ));
                    }
                }
            }
        }

        if compile_install_deps.is_empty() {
            return Ok(());
        }

        print_info(
            &format!(
                "Processing {} compile dependencies with install scripts",
                compile_install_deps.len()
            ),
            OutputLevel::Normal,
        );

        // Get SDK configuration for container setup
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Initialize SDK container helper
        let container_helper = SdkContainer::from_config(&self.config_path, config)?;

        // Process each compile dependency
        for (dep_name, compile_section, install_script) in compile_install_deps {
            print_info(
                &format!(
                    "Processing compile dependency '{dep_name}' -> compile: '{compile_section}', install: '{install_script}'"
                ),
                OutputLevel::Normal,
            );

            // First, run the SDK compile for the specified section
            let compile_command = SdkCompileCommand::new(
                self.config_path.clone(),
                self.verbose,
                vec![compile_section.clone()],
                Some(target.to_string()),
                self.container_args.clone(),
                self.dnf_args.clone(),
            );

            compile_command.execute().await.with_context(|| {
                format!(
                    "Failed to compile SDK section '{compile_section}' for dependency '{dep_name}'"
                )
            })?;

            // Then, run the install script
            // Note: install_script is already relative to /opt/src (the mounted src_dir in the container)
            // so we don't need to prepend src_dir here - just use it directly like compile scripts do
            let install_command = format!(
                r#"if [ -f '{install_script}' ]; then echo 'Running install script: {install_script}'; export AVOCADO_BUILD_EXT_SYSROOT="$AVOCADO_EXT_SYSROOTS/{extension_name}"; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{install_script}'; else echo 'Install script {install_script} not found.'; ls -la; exit 1; fi"#,
                extension_name = self.extension
            );

            if self.verbose {
                print_info(
                    &format!(
                        "Running install script for dependency '{dep_name}': {install_script}"
                    ),
                    OutputLevel::Normal,
                );
                print_info(
                    &format!(
                        "Setting AVOCADO_BUILD_EXT_SYSROOT=$AVOCADO_EXT_SYSROOTS/{}",
                        self.extension
                    ),
                    OutputLevel::Normal,
                );
            }

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: install_command,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                ..Default::default()
            };

            let success = container_helper.run_in_container(run_config).await?;

            if success {
                print_success(
                    &format!("Successfully processed compile dependency '{dep_name}'"),
                    OutputLevel::Normal,
                );
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to run install script '{install_script}' for compile dependency '{dep_name}'"
                ));
            }
        }

        Ok(())
    }

    /// Validate semantic versioning format (X.Y.Z where X, Y, Z are non-negative integers)
    fn validate_semver(version: &str) -> Result<()> {
        let parts: Vec<&str> = version.split('.').collect();

        if parts.len() < 3 {
            return Err(anyhow::anyhow!(
                "Version must follow semantic versioning format with at least MAJOR.MINOR.PATCH components (e.g., '1.0.0', '2.1.3')"
            ));
        }

        // Validate the first 3 components (MAJOR.MINOR.PATCH)
        for (i, part) in parts.iter().take(3).enumerate() {
            // Handle pre-release and build metadata (e.g., "1.0.0-alpha" or "1.0.0+build")
            let component = part.split(&['-', '+'][..]).next().unwrap_or(part);

            component.parse::<u32>().with_context(|| {
                let component_name = match i {
                    0 => "MAJOR",
                    1 => "MINOR",
                    2 => "PATCH",
                    _ => "component",
                };
                format!(
                    "{component_name} version component '{component}' must be a non-negative integer in semantic versioning format"
                )
            })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_sysext_build_script_basic() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Print the actual script for debugging
        // println!("Generated sysext build script:\n{}", script);

        assert!(script.contains(
            "release_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/extension-release.d\""
        ));
        assert!(script.contains("release_file=\"$release_dir/extension-release.test-ext-1.0\""));
        assert!(script.contains("modules_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/modules\""));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=0\" >> \"$release_file\""));
        assert!(script.contains("echo \"SYSEXT_SCOPE=system\" >> \"$release_file\""));
        assert!(script.contains(
            "if [ -d \"$modules_dir\" ] && [ -n \"$(find \"$modules_dir\" -name \"*.ko\""
        ));
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found kernel modules in extension 'test-ext'"));

        // Check for sysusers.d functionality
        assert!(script
            .contains("sysusers_dir1=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/local/lib/sysusers.d\""));
        assert!(
            script.contains("sysusers_dir2=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/sysusers.d\"")
        );
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"systemd-sysusers\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found sysusers.d config files in extension 'test-ext'"));

        // Check for custom on_merge functionality (should be present but not activated)
        assert!(script.contains("# Add custom AVOCADO_ON_MERGE commands if specified"));
        // With no custom commands, no AVOCADO_ON_MERGE entries should be added for custom commands
        assert!(!script.contains("[INFO] Added custom on_merge command to release file:"));
    }

    #[test]
    fn test_create_confext_build_script_basic() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        assert!(script
            .contains("release_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/etc/extension-release.d\""));
        assert!(script.contains("release_file=\"$release_dir/extension-release.test-ext-1.0\""));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=0\" >> \"$release_file\""));
        assert!(script.contains("echo \"CONFEXT_SCOPE=system\" >> \"$release_file\""));
        // Confext should NOT include kernel module detection
        assert!(!script.contains("modules_dir"));
        assert!(!script.contains("AVOCADO_ON_MERGE=\\\"depmod\\\""));
        assert!(!script.contains("Found kernel modules"));

        // Check for sysusers.d functionality in confext
        assert!(
            script.contains("sysusers_etc_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/etc/sysusers.d\"")
        );
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"systemd-sysusers\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found sysusers.d config files in extension 'test-ext'"));

        // Check for ld.so.conf.d functionality in confext
        assert!(script.contains("ldso_etc_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/etc/ld.so.conf.d\""));
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"ldconfig\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found ld.so.conf.d config files in extension 'test-ext'"));

        // Check for custom on_merge functionality (should be present but not activated)
        assert!(script.contains("# Add custom AVOCADO_ON_MERGE commands if specified"));
        // With no custom commands, no AVOCADO_ON_MERGE entries should be added for custom commands
        assert!(!script.contains("[INFO] Added custom on_merge command to release file:"));
    }

    #[test]
    fn test_create_sysext_build_script_multiple_scopes() {
        let cmd = ExtBuildCommand {
            extension: "multi-scope-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "2.0",
            &["system".to_string(), "portable".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        assert!(script.contains("echo \"SYSEXT_SCOPE=system portable\" >> \"$release_file\""));
        assert!(script.contains("AVOCADO_EXT_SYSROOTS/multi-scope-ext/usr/lib/extension-release.d"));
    }

    #[test]
    fn test_create_confext_build_script_multiple_scopes() {
        let cmd = ExtBuildCommand {
            extension: "multi-scope-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script(
            "2.0",
            &["system".to_string(), "portable".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        assert!(script.contains("echo \"CONFEXT_SCOPE=system portable\" >> \"$release_file\""));
        assert!(script.contains("AVOCADO_EXT_SYSROOTS/multi-scope-ext/etc/extension-release.d"));
    }

    #[test]
    fn test_create_confext_build_script_with_services() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let enable_services = vec!["peridiod.service".to_string(), "test.service".to_string()];
        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &enable_services,
            &[],
            None,
            None,
            false,
        );

        // Check that service linking commands are present
        assert!(script.contains("# Link service file for peridiod.service"));
        assert!(script.contains("service_file=\"$AVOCADO_EXT_SYSROOTS/test-ext/usr/lib/systemd/system/peridiod.service\""));
        assert!(script.contains("service_link_dir=\"$AVOCADO_EXT_SYSROOTS/test-ext/etc/systemd/system/multi-user.target.upholds\""));
        assert!(script.contains("ln -sf \"/usr/lib/systemd/system/peridiod.service\""));
        assert!(script.contains("# Link service file for test.service"));
        assert!(script.contains(
            "echo \"Warning: Service file peridiod.service not found in extension sysroot\""
        ));
    }

    #[test]
    fn test_kernel_module_detection_pattern() {
        let cmd = ExtBuildCommand {
            extension: "kernel-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify the find command looks for common kernel module extensions
        assert!(script.contains("-name \"*.ko\""));
        assert!(script.contains("-name \"*.ko.xz\""));
        assert!(script.contains("-name \"*.ko.gz\""));
        // Verify the conditional structure
        assert!(script.contains("if [ -d \"$modules_dir\" ] && [ -n \"$(find"));
        assert!(script.contains("2>/dev/null | head -n 1)\" ]; then"));
    }

    #[test]
    fn test_sysext_overlay_functionality() {
        let cmd = ExtBuildCommand {
            extension: "overlay-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let overlay_config = OverlayConfig {
            dir: "peridio".to_string(),
            mode: OverlayMode::Merge,
        };
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            Some(&overlay_config),
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify overlay merging commands are present
        assert!(script.contains("# Merge overlay directory into extension sysroot"));
        assert!(script.contains("if [ -d \"/opt/src/peridio\" ]; then"));
        assert!(script.contains("echo \"Merging overlay directory 'peridio' into extension sysroot with root:root ownership\""));
        assert!(script.contains(
            "rsync -a --chown=root:root /opt/src/peridio/ \"$AVOCADO_EXT_SYSROOTS/overlay-ext/\""
        ));
        assert!(script.contains("echo \"Error: Overlay directory 'peridio' not found in source\""));
        assert!(script.contains("exit 1"));
    }

    #[test]
    fn test_confext_overlay_functionality() {
        let cmd = ExtBuildCommand {
            extension: "overlay-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let overlay_config = OverlayConfig {
            dir: "peridio".to_string(),
            mode: OverlayMode::Merge,
        };
        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            Some(&overlay_config),
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify overlay merging commands are present
        assert!(script.contains("# Merge overlay directory into extension sysroot"));
        assert!(script.contains("if [ -d \"/opt/src/peridio\" ]; then"));
        assert!(script.contains("echo \"Merging overlay directory 'peridio' into extension sysroot with root:root ownership\""));
        assert!(script.contains(
            "rsync -a --chown=root:root /opt/src/peridio/ \"$AVOCADO_EXT_SYSROOTS/overlay-ext/\""
        ));
        assert!(script.contains("echo \"Error: Overlay directory 'peridio' not found in source\""));
        assert!(script.contains("exit 1"));
    }

    #[test]
    fn test_sysext_overlay_opaque_mode() {
        let cmd = ExtBuildCommand {
            extension: "opaque-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let overlay_config = OverlayConfig {
            dir: "peridio".to_string(),
            mode: OverlayMode::Opaque,
        };
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            Some(&overlay_config),
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify overlay opaque mode commands are present
        assert!(script.contains("# Copy overlay directory to extension sysroot (opaque mode)"));
        assert!(script.contains("if [ -d \"/opt/src/peridio\" ]; then"));
        assert!(script.contains(
            "echo \"Copying overlay directory 'peridio' to extension sysroot (opaque mode)\""
        ));
        assert!(script.contains("cp -r /opt/src/peridio/* \"$AVOCADO_EXT_SYSROOTS/opaque-ext/\""));
        assert!(script.contains("echo \"Setting ownership to root:root for overlay files\""));
        assert!(script.contains("find \"/opt/src/peridio\" -mindepth 1"));
        assert!(script.contains("echo \"Error: Overlay directory 'peridio' not found in source\""));
        assert!(script.contains("exit 1"));
    }

    #[test]
    fn test_confext_overlay_opaque_mode() {
        let cmd = ExtBuildCommand {
            extension: "opaque-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let overlay_config = OverlayConfig {
            dir: "peridio".to_string(),
            mode: OverlayMode::Opaque,
        };
        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            Some(&overlay_config),
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify overlay opaque mode commands are present
        assert!(script.contains("# Copy overlay directory to extension sysroot (opaque mode)"));
        assert!(script.contains("if [ -d \"/opt/src/peridio\" ]; then"));
        assert!(script.contains(
            "echo \"Copying overlay directory 'peridio' to extension sysroot (opaque mode)\""
        ));
        assert!(script.contains("cp -r /opt/src/peridio/* \"$AVOCADO_EXT_SYSROOTS/opaque-ext/\""));
        assert!(script.contains("echo \"Setting ownership to root:root for overlay files\""));
        assert!(script.contains("find \"/opt/src/peridio\" -mindepth 1"));
        assert!(script.contains("echo \"Error: Overlay directory 'peridio' not found in source\""));
        assert!(script.contains("exit 1"));
    }

    #[test]
    fn test_no_overlay_functionality() {
        let cmd = ExtBuildCommand {
            extension: "no-overlay-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script_sysext = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );
        let script_confext = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify no overlay merging commands are present
        assert!(!script_sysext.contains("Merge overlay directory"));
        assert!(!script_sysext.contains("Copy overlay directory"));
        assert!(!script_sysext.contains("/opt/src/"));
        assert!(!script_confext.contains("Merge overlay directory"));
        assert!(!script_confext.contains("Copy overlay directory"));
        assert!(!script_confext.contains("/opt/src/"));
    }

    #[test]
    fn test_create_sysext_build_script_with_modprobe() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let modprobe_modules = vec!["nfs".to_string(), "ext4".to_string()];
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &modprobe_modules,
            &[],
            None,
            None,
            false,
        );

        // Verify separate AVOCADO_ON_MERGE entries are added for kernel modules and modprobe commands
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        assert!(script.contains("[INFO] Found kernel modules in extension 'test-ext', added AVOCADO_ON_MERGE=\\\"depmod\\\" to release file"));

        // Verify separate modprobe commands are added
        assert!(
            script.contains("echo \"AVOCADO_ON_MERGE=\\\"modprobe nfs\\\"\" >> \"$release_file\"")
        );
        assert!(
            script.contains("[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe nfs\\\" to release file")
        );
        assert!(
            script.contains("echo \"AVOCADO_ON_MERGE=\\\"modprobe ext4\\\"\" >> \"$release_file\"")
        );
        assert!(
            script.contains("[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe ext4\\\" to release file")
        );
    }

    #[test]
    fn test_create_sysext_build_script_with_sysusers() {
        let cmd = ExtBuildCommand {
            extension: "sysusers-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify sysusers.d detection logic is present
        assert!(script.contains(
            "sysusers_dir1=\"$AVOCADO_EXT_SYSROOTS/sysusers-ext/usr/local/lib/sysusers.d\""
        ));
        assert!(script
            .contains("sysusers_dir2=\"$AVOCADO_EXT_SYSROOTS/sysusers-ext/usr/lib/sysusers.d\""));
        assert!(script.contains("find \"$sysusers_dir1\" -name \"*.conf\""));
        assert!(script.contains("find \"$sysusers_dir2\" -name \"*.conf\""));
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"systemd-sysusers\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found sysusers.d config files in extension 'sysusers-ext'"));
    }

    #[test]
    fn test_create_confext_build_script_with_sysusers() {
        let cmd = ExtBuildCommand {
            extension: "sysusers-confext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify sysusers.d detection logic is present for confext
        assert!(script.contains(
            "sysusers_etc_dir=\"$AVOCADO_EXT_SYSROOTS/sysusers-confext/etc/sysusers.d\""
        ));
        assert!(script.contains("find \"$sysusers_etc_dir\" -name \"*.conf\""));
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"systemd-sysusers\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found sysusers.d config files in extension 'sysusers-confext'"));
    }

    #[test]
    fn test_create_confext_build_script_with_ldso_conf_d() {
        let cmd = ExtBuildCommand {
            extension: "ldso-confext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify ld.so.conf.d detection logic is present for confext
        assert!(
            script.contains("ldso_etc_dir=\"$AVOCADO_EXT_SYSROOTS/ldso-confext/etc/ld.so.conf.d\"")
        );
        assert!(script.contains("find \"$ldso_etc_dir\" -name \"*.conf\""));
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"ldconfig\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found ld.so.conf.d config files in extension 'ldso-confext'"));
    }

    #[test]
    fn test_create_sysext_build_script_with_custom_on_merge() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let on_merge_commands = vec![
            "systemctl restart sshd.socket".to_string(),
            "echo test".to_string(),
        ];
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &on_merge_commands,
            None,
            None,
            false,
        );

        // Verify custom on_merge commands are added as separate entries
        assert!(script.contains(
            "echo \"AVOCADO_ON_MERGE=\\\"systemctl restart sshd.socket\\\"\" >> \"$release_file\""
        ));
        assert!(script.contains(
            "[INFO] Added custom on_merge command to release file: systemctl restart sshd.socket"
        ));
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"echo test\\\"\" >> \"$release_file\""));
        assert!(script.contains("[INFO] Added custom on_merge command to release file: echo test"));
    }

    #[test]
    fn test_create_confext_build_script_with_custom_on_merge() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let on_merge_commands = vec!["systemctl restart sshd.socket".to_string()];
        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &on_merge_commands,
            None,
            None,
            false,
        );

        // Verify custom on_merge commands are added as separate entries
        assert!(script.contains(
            "echo \"AVOCADO_ON_MERGE=\\\"systemctl restart sshd.socket\\\"\" >> \"$release_file\""
        ));
        assert!(script.contains(
            "[INFO] Added custom on_merge command to release file: systemctl restart sshd.socket"
        ));
    }

    #[test]
    fn test_create_sysext_build_script_with_both_kernel_modules_and_sysusers() {
        let cmd = ExtBuildCommand {
            extension: "combined-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify both kernel modules and sysusers.d are handled correctly with separate lines
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"systemd-sysusers\\\"\" >> \"$release_file\""));
        assert!(script.contains("Found kernel modules in extension 'combined-ext'"));
        assert!(script.contains("Found sysusers.d config files in extension 'combined-ext'"));
    }

    #[test]
    fn test_create_sysext_build_script_without_modprobe() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            false,
        );

        // Verify that without modprobe modules, only depmod is added when kernel modules are found
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        // Verify no modprobe commands are added when no modules are specified
        assert!(!script.contains("modprobe"));
    }

    #[test]
    fn test_create_sysext_build_script_with_user_example_modules() {
        let cmd = ExtBuildCommand {
            extension: "gpio-test".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Test with the example modules from the user's request
        let modprobe_modules = vec!["gpio-pca953x".to_string(), "rtc-pcf8563w".to_string()];
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &modprobe_modules,
            &[],
            None,
            None,
            false,
        );

        // Verify separate AVOCADO_ON_MERGE entries are added
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        assert!(script.contains("[INFO] Found kernel modules in extension 'gpio-test', added AVOCADO_ON_MERGE=\\\"depmod\\\" to release file"));

        // Verify separate modprobe commands are added
        assert!(script.contains(
            "echo \"AVOCADO_ON_MERGE=\\\"modprobe gpio-pca953x\\\"\" >> \"$release_file\""
        ));
        assert!(script.contains(
            "[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe gpio-pca953x\\\" to release file"
        ));
        assert!(script.contains(
            "echo \"AVOCADO_ON_MERGE=\\\"modprobe rtc-pcf8563w\\\"\" >> \"$release_file\""
        ));
        assert!(script.contains(
            "[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe rtc-pcf8563w\\\" to release file"
        ));

        // Verify AVOCADO_MODPROBE is no longer used
        assert!(!script.contains("AVOCADO_MODPROBE"));
    }

    #[test]
    fn test_separate_avocado_on_merge_entries() {
        let cmd = ExtBuildCommand {
            extension: "comprehensive-test".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Test with both modprobe modules and custom commands
        let modprobe_modules = vec!["module1".to_string(), "module2".to_string()];
        let custom_commands = vec![
            "systemctl restart service1".to_string(),
            "echo 'test'".to_string(),
        ];
        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &modprobe_modules,
            &custom_commands,
            None,
            None,
            false,
        );

        // Verify that each command gets its own separate AVOCADO_ON_MERGE entry
        // Kernel modules section
        assert!(script.contains("echo \"AVOCADO_ON_MERGE=\\\"depmod\\\"\" >> \"$release_file\""));
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"modprobe module1\\\"\" >> \"$release_file\""));
        assert!(script
            .contains("echo \"AVOCADO_ON_MERGE=\\\"modprobe module2\\\"\" >> \"$release_file\""));

        // Custom commands section
        assert!(script.contains(
            "echo \"AVOCADO_ON_MERGE=\\\"systemctl restart service1\\\"\" >> \"$release_file\""
        ));
        assert!(
            script.contains("echo \"AVOCADO_ON_MERGE=\\\"echo 'test'\\\"\" >> \"$release_file\"")
        );

        // Verify no semicolon-separated commands
        assert!(!script.contains("depmod; modprobe"));
        assert!(!script.contains("systemctl restart service1; echo"));

        // Verify individual log messages
        assert!(script
            .contains("[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe module1\\\" to release file"));
        assert!(script
            .contains("[INFO] Added AVOCADO_ON_MERGE=\\\"modprobe module2\\\" to release file"));
        assert!(script.contains(
            "[INFO] Added custom on_merge command to release file: systemctl restart service1"
        ));
        assert!(
            script.contains("[INFO] Added custom on_merge command to release file: echo 'test'")
        );
    }

    #[test]
    fn test_create_users_script_section_with_empty_password_user() {
        let cmd = ExtBuildCommand {
            extension: "avocado-dev".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create users config matching the example in the user request
        let mut users_config = serde_yaml::Mapping::new();
        let mut root_user = serde_yaml::Mapping::new();
        root_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("root".to_string()),
            serde_yaml::Value::Mapping(root_user),
        );

        let script = cmd.create_users_script_section(Some(&users_config), None);

        // Verify the users script section contains the expected commands
        assert!(script.contains("# Copy and manage user authentication files"));
        assert!(script
            .contains("Copying /etc/passwd, /etc/shadow, and /etc/group from rootfs to extension"));
        assert!(script.contains("mkdir -p \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/passwd\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/passwd\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/shadow\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/shadow\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/group\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/group\""));
        assert!(script.contains("Creating user 'root'"));
        assert!(script.contains("[WARNING] User 'root' will be able to login with NO PASSWORD"));
        assert!(script.contains("Setting password and aging policy for user 'root'"));
        assert!(script.contains("chown root:root"));
        assert!(script.contains("chmod 644"));
        assert!(script.contains("chmod 640"));
    }

    #[test]
    fn test_create_users_script_section_without_users() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_users_script_section(None, None);

        // Should return empty string when no users config is provided
        assert_eq!(script, "");
    }

    #[test]
    fn test_create_users_script_section_with_non_empty_password_user() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create users config with a user that has a non-empty password
        let mut users_config = serde_yaml::Mapping::new();
        let mut user = serde_yaml::Mapping::new();
        user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$salt$hashedpassword".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("testuser".to_string()),
            serde_yaml::Value::Mapping(user),
        );

        let script = cmd.create_users_script_section(Some(&users_config), None);

        // Should now generate script for any password value
        assert!(script.contains("# Copy and manage user authentication files"));
        assert!(script.contains("Creating user 'testuser'"));
        assert!(script.contains("Setting password and aging policy for user 'testuser'"));
        assert!(script.contains("testuser:\\$6\\$salt\\$hashedpassword:"));
        // Should NOT contain warning for hashed passwords
        assert!(
            !script.contains("[WARNING] User 'testuser' will be able to login with NO PASSWORD")
        );
    }

    #[test]
    fn test_create_users_script_section_with_invalid_password_type() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create users config with a user that has a non-string password
        let mut users_config = serde_yaml::Mapping::new();
        let mut user = serde_yaml::Mapping::new();
        user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::Number(123.into()),
        );
        users_config.insert(
            serde_yaml::Value::String("testuser".to_string()),
            serde_yaml::Value::Mapping(user),
        );

        let script = cmd.create_users_script_section(Some(&users_config), None);

        // Should still create the basic structure and the user with default password
        assert!(script.contains("# Copy and manage user authentication files"));
        // Should create the user with default password "*" (no login allowed)
        assert!(script.contains("Creating user 'testuser'"));
        assert!(script.contains("testuser:*:19000:0:99999:7:::"));
    }

    #[test]
    fn test_sysext_build_script_with_users() {
        let cmd = ExtBuildCommand {
            extension: "avocado-dev".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create users config matching the example in the user request
        let mut users_config = serde_yaml::Mapping::new();
        let mut root_user = serde_yaml::Mapping::new();
        root_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("root".to_string()),
            serde_yaml::Value::Mapping(root_user),
        );

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            Some(&users_config),
            None,
            false,
        );

        // Verify the complete build script includes users functionality
        assert!(script.contains("set -e"));
        assert!(script.contains("# Copy and manage user authentication files"));
        assert!(script.contains("mkdir -p \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/passwd\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/passwd\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/shadow\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/shadow\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/group\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/group\""));
        assert!(script.contains("Creating user 'root'"));
        assert!(script.contains("[WARNING] User 'root' will be able to login with NO PASSWORD"));
        assert!(script.contains(
            "release_dir=\"$AVOCADO_EXT_SYSROOTS/avocado-dev/usr/lib/extension-release.d\""
        ));
        // Note: Script generation uses ext_version parameter which is "0.1.0" in create_sysext_build_script call
        assert!(script.contains("release_file=\"$release_dir/extension-release.avocado-dev-"));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"SYSEXT_SCOPE=system\" >> \"$release_file\""));
    }

    #[test]
    fn test_confext_build_script_with_users() {
        let cmd = ExtBuildCommand {
            extension: "avocado-dev".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create users config matching the example in the user request
        let mut users_config = serde_yaml::Mapping::new();
        let mut root_user = serde_yaml::Mapping::new();
        root_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("root".to_string()),
            serde_yaml::Value::Mapping(root_user),
        );

        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            Some(&users_config),
            None,
            false,
        );

        // Verify the complete build script includes users functionality
        assert!(script.contains("set -e"));
        assert!(script.contains("# Copy and manage user authentication files"));
        assert!(script.contains("mkdir -p \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/passwd\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/passwd\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/shadow\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/shadow\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/group\" \"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/group\""));
        assert!(script.contains("Creating user 'root'"));
        assert!(script
            .contains("release_dir=\"$AVOCADO_EXT_SYSROOTS/avocado-dev/etc/extension-release.d\""));
        // Note: Script generation uses ext_version parameter
        assert!(script.contains("release_file=\"$release_dir/extension-release.avocado-dev-"));
        assert!(script.contains("echo \"ID=_any\" > \"$release_file\""));
        assert!(script.contains("echo \"CONFEXT_SCOPE=system\" >> \"$release_file\""));
    }

    #[test]
    fn test_warning_for_empty_password() {
        let cmd = ExtBuildCommand {
            extension: "warning-test".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Test empty password - should show warning
        let mut empty_users_config = serde_yaml::Mapping::new();
        let mut empty_user = serde_yaml::Mapping::new();
        empty_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        empty_users_config.insert(
            serde_yaml::Value::String("testuser".to_string()),
            serde_yaml::Value::Mapping(empty_user),
        );

        let empty_script = cmd.create_users_script_section(Some(&empty_users_config), None);
        assert!(empty_script
            .contains("[WARNING] User 'testuser' will be able to login with NO PASSWORD"));

        // Test hashed password - should NOT show warning
        let mut hashed_users_config = serde_yaml::Mapping::new();
        let mut hashed_user = serde_yaml::Mapping::new();
        hashed_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$salt$hash".to_string()),
        );
        hashed_users_config.insert(
            serde_yaml::Value::String("testuser".to_string()),
            serde_yaml::Value::Mapping(hashed_user),
        );

        let hashed_script = cmd.create_users_script_section(Some(&hashed_users_config), None);
        assert!(!hashed_script
            .contains("[WARNING] User 'testuser' will be able to login with NO PASSWORD"));
        // Should contain escaped password
        assert!(hashed_script.contains("testuser:\\$6\\$salt\\$hash:"));
    }

    #[test]
    fn test_full_users_and_groups_functionality() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create comprehensive groups config
        let mut groups_config = serde_yaml::Mapping::new();
        let mut avocado_group = serde_yaml::Mapping::new();
        avocado_group.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(1000.into()),
        );
        groups_config.insert(
            serde_yaml::Value::String("avocado".to_string()),
            serde_yaml::Value::Mapping(avocado_group),
        );

        // Create comprehensive users config
        let mut users_config = serde_yaml::Mapping::new();

        // Root user with empty password
        let mut root_user = serde_yaml::Mapping::new();
        root_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("root".to_string()),
            serde_yaml::Value::Mapping(root_user),
        );

        // Avocado user with UID, groups, and hashed password
        let mut avocado_user = serde_yaml::Mapping::new();
        avocado_user.insert(
            serde_yaml::Value::String("uid".to_string()),
            serde_yaml::Value::Number(1000.into()),
        );
        avocado_user.insert(
            serde_yaml::Value::String("groups".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("avocado".to_string())]),
        );
        avocado_user.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$salt$hash".to_string()),
        );
        users_config.insert(
            serde_yaml::Value::String("avocado".to_string()),
            serde_yaml::Value::Mapping(avocado_user),
        );

        let script = cmd.create_users_script_section(Some(&users_config), Some(&groups_config));

        // Test group creation
        assert!(script.contains("# Create groups"));
        assert!(script.contains("Creating group 'avocado'"));
        assert!(script.contains("avocado:x:1000:"));
        assert!(script.contains("Group 'avocado' created with GID 1000"));

        // Test user creation
        assert!(script.contains("# Create and configure users"));
        assert!(script.contains("Creating user 'root'"));
        assert!(script.contains("Creating user 'avocado'"));

        // Test UID handling
        assert!(script.contains("avocado:x:1000:"));

        // Test password warnings and settings
        assert!(script.contains("[WARNING] User 'root' will be able to login with NO PASSWORD"));
        assert!(!script.contains("[WARNING] User 'avocado'"));
        assert!(script.contains("avocado:\\$6\\$salt\\$hash:"));

        // Test file permissions
        assert!(script.contains("chown root:root"));
        assert!(script.contains("chmod 644"));
        assert!(script.contains("chmod 640"));
    }

    #[test]
    fn test_comprehensive_users_and_groups_schema() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Create comprehensive users configuration using mixed approach
        let mut users_table = serde_yaml::Mapping::new();

        // Simple users use inline-style tables (represented as TOML tables in tests)
        let mut root_table = serde_yaml::Mapping::new();
        root_table.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("".to_string()),
        );
        users_table.insert(
            serde_yaml::Value::String("root".to_string()),
            serde_yaml::Value::Mapping(root_table),
        );

        // Complex users would use table syntax in real TOML (but represented as nested tables in tests)
        let mut alice_table = serde_yaml::Mapping::new();
        alice_table.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$salt$hash".to_string()),
        );
        alice_table.insert(
            serde_yaml::Value::String("uid".to_string()),
            serde_yaml::Value::Number(1001.into()),
        );
        alice_table.insert(
            serde_yaml::Value::String("gecos".to_string()),
            serde_yaml::Value::String("Alice Developer".to_string()),
        );
        alice_table.insert(
            serde_yaml::Value::String("shell".to_string()),
            serde_yaml::Value::String("/bin/zsh".to_string()),
        );
        alice_table.insert(
            serde_yaml::Value::String("groups".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("users".to_string()),
                serde_yaml::Value::String("developers".to_string()),
            ]),
        );
        users_table.insert(
            serde_yaml::Value::String("alice".to_string()),
            serde_yaml::Value::Mapping(alice_table),
        );

        // User with comprehensive passwd attributes
        let mut bob_table = serde_yaml::Mapping::new();
        bob_table.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$anothersalt$anotherhash".to_string()),
        );
        bob_table.insert(
            serde_yaml::Value::String("uid".to_string()),
            serde_yaml::Value::Number(1002.into()),
        );
        bob_table.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(1002.into()),
        );
        bob_table.insert(
            serde_yaml::Value::String("gecos".to_string()),
            serde_yaml::Value::String("Bob Smith,DevOps,Room 123,555-1234,555-5678".to_string()),
        );
        bob_table.insert(
            serde_yaml::Value::String("home".to_string()),
            serde_yaml::Value::String("/home/bob".to_string()),
        );
        bob_table.insert(
            serde_yaml::Value::String("shell".to_string()),
            serde_yaml::Value::String("/bin/bash".to_string()),
        );
        bob_table.insert(
            serde_yaml::Value::String("groups".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("users".to_string()),
                serde_yaml::Value::String("admins".to_string()),
            ]),
        );
        users_table.insert(
            serde_yaml::Value::String("bob".to_string()),
            serde_yaml::Value::Mapping(bob_table),
        );

        // User with comprehensive shadow attributes for password aging
        let mut charlie_table = serde_yaml::Mapping::new();
        charlie_table.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("$6$salt3$hash3".to_string()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("uid".to_string()),
            serde_yaml::Value::Number(1003.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("gecos".to_string()),
            serde_yaml::Value::String("Charlie Security".to_string()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("last_change".to_string()),
            serde_yaml::Value::Number(19000.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("min_days".to_string()),
            serde_yaml::Value::Number(7.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("max_days".to_string()),
            serde_yaml::Value::Number(90.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("warn_days".to_string()),
            serde_yaml::Value::Number(7.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("inactive_days".to_string()),
            serde_yaml::Value::Number(14.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("expire_date".to_string()),
            serde_yaml::Value::Number(20000.into()),
        );
        charlie_table.insert(
            serde_yaml::Value::String("groups".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("users".to_string())]),
        );
        users_table.insert(
            serde_yaml::Value::String("charlie".to_string()),
            serde_yaml::Value::Mapping(charlie_table),
        );

        // System service user
        let mut nginx_table = serde_yaml::Mapping::new();
        nginx_table.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String("*".to_string()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("uid".to_string()),
            serde_yaml::Value::Number(33.into()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(33.into()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("gecos".to_string()),
            serde_yaml::Value::String("nginx web server".to_string()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("home".to_string()),
            serde_yaml::Value::String("/var/www".to_string()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("shell".to_string()),
            serde_yaml::Value::String("/usr/sbin/nologin".to_string()),
        );
        nginx_table.insert(
            serde_yaml::Value::String("system".to_string()),
            serde_yaml::Value::Bool(true),
        );
        users_table.insert(
            serde_yaml::Value::String("nginx".to_string()),
            serde_yaml::Value::Mapping(nginx_table),
        );

        // Create comprehensive groups configuration
        let mut groups_table = serde_yaml::Mapping::new();

        // Basic group
        let mut users_group_table = serde_yaml::Mapping::new();
        users_group_table.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(1000.into()),
        );
        groups_table.insert(
            serde_yaml::Value::String("users".to_string()),
            serde_yaml::Value::Mapping(users_group_table),
        );

        // Group with members
        let mut developers_group_table = serde_yaml::Mapping::new();
        developers_group_table.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(2000.into()),
        );
        developers_group_table.insert(
            serde_yaml::Value::String("members".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("alice".to_string()),
                serde_yaml::Value::String("bob".to_string()),
            ]),
        );
        groups_table.insert(
            serde_yaml::Value::String("developers".to_string()),
            serde_yaml::Value::Mapping(developers_group_table),
        );

        // System group
        let mut admins_group_table = serde_yaml::Mapping::new();
        admins_group_table.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(27.into()),
        );
        admins_group_table.insert(
            serde_yaml::Value::String("system".to_string()),
            serde_yaml::Value::Bool(true),
        );
        admins_group_table.insert(
            serde_yaml::Value::String("members".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("bob".to_string())]),
        );
        groups_table.insert(
            serde_yaml::Value::String("admins".to_string()),
            serde_yaml::Value::Mapping(admins_group_table),
        );

        let script = cmd.create_users_script_section(Some(&users_table), Some(&groups_table));

        // Verify the script contains expected basic setup
        assert!(script.contains("mkdir -p \"$AVOCADO_EXT_SYSROOTS/test-ext/etc\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/passwd\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/shadow\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/group\""));

        // Check group creation with various attributes
        assert!(script.contains("# Create groups"));
        assert!(script.contains("Creating group 'users'"));
        assert!(script.contains("users:x:1000:"));
        assert!(script.contains("Creating group 'developers'"));
        assert!(script.contains("developers:x:2000:alice,bob"));
        assert!(script.contains("Creating group 'admins'"));
        assert!(script.contains("(system group)"));
        assert!(script.contains("admins:x:27:bob"));

        // Check user creation with comprehensive attributes
        assert!(script.contains("# Create and configure users"));
        assert!(script.contains("Creating user 'root'"));
        assert!(script.contains("[WARNING] User 'root' will be able to login with NO PASSWORD"));
        assert!(script.contains("root:x:$CURRENT_UID:$CURRENT_UID:root:/home/root:/bin/sh"));

        assert!(script.contains("Creating user 'alice'"));
        assert!(script.contains("alice:x:1001:$CURRENT_UID:Alice Developer:/home/alice:/bin/zsh"));

        assert!(script.contains("Creating user 'bob'"));
        assert!(script.contains(
            "bob:x:1002:1002:Bob Smith,DevOps,Room 123,555-1234,555-5678:/home/bob:/bin/bash"
        ));

        assert!(script.contains("Creating user 'nginx' (system user)"));
        assert!(script.contains("nginx:x:33:33:nginx web server:/var/www:/usr/sbin/nologin"));

        // Check shadow file updates with comprehensive attributes (escaped for sed)
        assert!(script.contains("root::19000:0:99999:7:::"));
        assert!(script.contains("alice:\\$6\\$salt\\$hash:19000:0:99999:7:::"));
        assert!(script.contains("bob:\\$6\\$anothersalt\\$anotherhash:19000:0:99999:7:::"));
        assert!(script.contains("charlie:\\$6\\$salt3\\$hash3:19000:7:90:7:14:20000:"));
        assert!(script.contains("nginx:*:19000:0:99999:7:::"));

        // Check group membership
        assert!(script.contains("Add user 'alice' to additional groups"));
        assert!(script.contains("Add user 'bob' to additional groups"));

        // Check permissions
        assert!(script.contains("chmod 644 \"$AVOCADO_EXT_SYSROOTS/test-ext/etc/passwd\""));
        assert!(script.contains("chmod 640 \"$AVOCADO_EXT_SYSROOTS/test-ext/etc/shadow\""));
        assert!(script.contains("chmod 644 \"$AVOCADO_EXT_SYSROOTS/test-ext/etc/group\""));
    }

    #[test]
    fn test_minimal_user_defaults() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        // Test user with just name (no fields at all)
        let mut users_table = serde_yaml::Mapping::new();
        let empty_table = serde_yaml::Mapping::new();
        users_table.insert(
            serde_yaml::Value::String("testuser".to_string()),
            serde_yaml::Value::Mapping(empty_table),
        );

        let script = cmd.create_users_script_section(Some(&users_table), None);

        // Should use all defaults
        assert!(
            script.contains("testuser:x:$CURRENT_UID:$CURRENT_UID:testuser:/home/testuser:/bin/sh")
        );
        assert!(script.contains("testuser:*:19000:0:99999:7:::")); // Default password "*" (no login)
    }

    #[test]
    fn test_create_sysext_build_script_with_reload_service_manager_true() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_sysext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            true,
        );

        // Verify that reload_service_manager = true sets EXTENSION_RELOAD_MANAGER=1
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=1\" >> \"$release_file\""));
    }

    #[test]
    fn test_create_confext_build_script_with_reload_service_manager_true() {
        let cmd = ExtBuildCommand {
            extension: "test-ext".to_string(),
            config_path: "avocado.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let script = cmd.create_confext_build_script(
            "1.0",
            &["system".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            true,
        );

        // Verify that reload_service_manager = true sets EXTENSION_RELOAD_MANAGER=1
        assert!(script.contains("echo \"EXTENSION_RELOAD_MANAGER=1\" >> \"$release_file\""));
    }

    #[test]
    fn test_handle_compile_dependencies_parsing() {
        // Test that the new compile dependency syntax is properly parsed
        let config_content = r#"
ext:
  my-extension:
    types:
      - sysext
    dependencies:
      my-app:
        compile: my-app
        install: ext-install.sh
      regular-package: "1.0.0"
      old-compile-dep:
        compile: old-section

sdk:
  compile:
    my-app:
      compile: ext-compile.sh
    old-section:
      compile: ext-compile.sh
"#;

        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let ext_config = parsed
            .get(serde_yaml::Value::String("ext".to_string()))
            .unwrap()
            .get(serde_yaml::Value::String("my-extension".to_string()))
            .unwrap();
        let dependencies = ext_config
            .get(serde_yaml::Value::String("dependencies".to_string()))
            .unwrap()
            .as_mapping()
            .unwrap();

        // Check that we can identify compile dependencies with install scripts
        let mut compile_install_deps = Vec::new();
        for (dep_name, dep_spec) in dependencies {
            if let serde_yaml::Value::Mapping(spec_map) = dep_spec {
                if let (
                    Some(serde_yaml::Value::String(compile_section)),
                    Some(serde_yaml::Value::String(install_script)),
                ) = (
                    spec_map.get(serde_yaml::Value::String("compile".to_string())),
                    spec_map.get(serde_yaml::Value::String("install".to_string())),
                ) {
                    compile_install_deps.push((
                        dep_name.as_str().unwrap_or("").to_string(),
                        compile_section.clone(),
                        install_script.clone(),
                    ));
                }
            }
        }

        // Should find exactly one compile dependency with install script
        assert_eq!(compile_install_deps.len(), 1);
        assert_eq!(compile_install_deps[0].0, "my-app");
        assert_eq!(compile_install_deps[0].1, "my-app");
        assert_eq!(compile_install_deps[0].2, "ext-install.sh");
    }

    #[test]
    fn test_install_command_includes_sysroot_env() {
        // Test that the install command includes the AVOCADO_BUILD_EXT_SYSROOT environment variable
        let cmd = ExtBuildCommand {
            extension: "test-extension".to_string(),
            config_path: "test.yaml".to_string(),
            verbose: false,
            target: None,
            container_args: None,
            dnf_args: None,
        };

        let src_dir = "src";
        let install_script = "test-install.sh";
        let install_script_path = format!("{src_dir}/{install_script}");

        let expected_command = format!(
            r#"if [ -f '{install_script_path}' ]; then echo 'Running install script: {install_script}'; export AVOCADO_BUILD_EXT_SYSROOT="$AVOCADO_EXT_SYSROOTS/{extension_name}"; AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash '{install_script_path}'; else echo 'Install script {install_script} not found.' && ls -la {src_dir}; exit 1; fi"#,
            extension_name = cmd.extension
        );

        // Verify the command includes the environment variable export with proper quoting
        assert!(expected_command.contains(
            r#"export AVOCADO_BUILD_EXT_SYSROOT="$AVOCADO_EXT_SYSROOTS/test-extension""#
        ));

        // The command should look like this (for demonstration):
        // if [ -f 'src/test-install.sh' ]; then echo 'Running install script: test-install.sh';
        // export AVOCADO_BUILD_EXT_SYSROOT="$AVOCADO_EXT_SYSROOTS/test-extension";
        // AVOCADO_SDK_PREFIX=$AVOCADO_SDK_PREFIX bash 'src/test-install.sh';
        // else echo 'Install script test-install.sh not found.' && ls -la src; exit 1; fi
    }
}
