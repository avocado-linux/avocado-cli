// Allow deprecated variants for backward compatibility during migration
#![allow(deprecated)]

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::process::Command as AsyncCommand;

use crate::commands::install::ExtensionDependency;
use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    output::{print_error, print_info, print_success, OutputLevel},
    target::resolve_target_required,
};

/// Container configuration for fetch operations
struct ContainerConfig<'a> {
    helper: &'a SdkContainer,
    image: &'a str,
    target_arch: &'a str,
    repo_url: Option<&'a String>,
    repo_release: Option<&'a String>,
    container_args: &'a Option<Vec<String>>,
}

/// Command to fetch and refresh repository metadata for sysroots
pub struct FetchCommand {
    config_path: String,
    verbose: bool,
    extension: Option<String>,
    runtime: Option<String>,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl FetchCommand {
    pub fn new(
        config_path: String,
        verbose: bool,
        extension: Option<String>,
        runtime: Option<String>,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            extension,
            runtime,
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
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;
        let config_toml = &composed.merged_value;

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        // Get container configuration from interpolated config
        let container_image = config
            .get_sdk_image()
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Initialize container helper
        let container_helper = SdkContainer::new();

        // Pull the latest SDK container image
        self.pull_sdk_image(container_image).await?;

        // Get repo configuration from config
        let repo_url = config_toml
            .get("sdk")
            .and_then(|sdk| sdk.get("repo_url"))
            .and_then(|url| url.as_str())
            .map(|s| s.to_string());

        let repo_release = config_toml
            .get("sdk")
            .and_then(|sdk| sdk.get("repo_release"))
            .and_then(|release| release.as_str())
            .map(|s| s.to_string());

        // Determine what to fetch based on arguments
        match (&self.extension, &self.runtime) {
            (Some(extension), None) => {
                // Fetch for specific extension
                let container_config = ContainerConfig {
                    helper: &container_helper,
                    image: container_image,
                    target_arch: &target_arch,
                    repo_url: repo_url.as_ref(),
                    repo_release: repo_release.as_ref(),
                    container_args: &merged_container_args,
                };
                self.fetch_extension_metadata(config_toml, extension, &container_config)
                    .await?;
            }
            (None, Some(runtime)) => {
                // Fetch for specific runtime
                let container_config = ContainerConfig {
                    helper: &container_helper,
                    image: container_image,
                    target_arch: &target_arch,
                    repo_url: repo_url.as_ref(),
                    repo_release: repo_release.as_ref(),
                    container_args: &merged_container_args,
                };
                self.fetch_runtime_metadata(config_toml, runtime, &container_config)
                    .await?;
            }
            (None, None) => {
                // Fetch for all sysroots
                let container_config = ContainerConfig {
                    helper: &container_helper,
                    image: container_image,
                    target_arch: &target_arch,
                    repo_url: repo_url.as_ref(),
                    repo_release: repo_release.as_ref(),
                    container_args: &merged_container_args,
                };
                self.fetch_all_metadata(config_toml, &container_config)
                    .await?;
            }
            (Some(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "Cannot specify both --extension and --runtime. Choose one or neither for all sysroots."
                ));
            }
        }

        print_success("Repository metadata fetch completed.", OutputLevel::Normal);
        Ok(())
    }

    async fn fetch_extension_metadata(
        &self,
        config_toml: &serde_yaml::Value,
        extension: &str,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info(
            &format!("Fetching repository metadata for extension '{extension}'"),
            OutputLevel::Normal,
        );

        // Check if extension exists in configuration
        if config_toml
            .get("extensions")
            .and_then(|ext| ext.get(extension))
            .is_none()
        {
            return Err(anyhow::anyhow!(
                "Extension '{extension}' not found in configuration"
            ));
        }

        // Check if extension sysroot exists
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{extension} ]");
        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let sysroot_exists = container_config.helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            print_error(
                &format!("Extension sysroot '{extension}' does not exist. Run 'avocado ext install {extension}' first."),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Extension sysroot not found"));
        }

        // Run DNF makecache for the extension sysroot
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot=$AVOCADO_EXT_SYSROOTS/{extension} \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch metadata for extension '{extension}'"
            ));
        }

        print_success(
            &format!("Successfully fetched metadata for extension '{extension}'"),
            OutputLevel::Normal,
        );
        Ok(())
    }

    async fn fetch_runtime_metadata(
        &self,
        config_toml: &serde_yaml::Value,
        runtime: &str,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info(
            &format!("Fetching repository metadata for runtime '{runtime}'"),
            OutputLevel::Normal,
        );

        // Check if runtime exists in configuration
        if config_toml
            .get("runtimes")
            .and_then(|rt| rt.get(runtime))
            .is_none()
        {
            return Err(anyhow::anyhow!(
                "Runtime '{runtime}' not found in configuration"
            ));
        }

        // Check if runtime sysroot exists
        let installroot_path = format!("$AVOCADO_PREFIX/runtimes/{runtime}");
        let check_command = format!("[ -d {installroot_path} ]");
        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let sysroot_exists = container_config.helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            print_error(
                &format!("Runtime sysroot '{runtime}' does not exist. Run 'avocado runtime install {runtime}' first."),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Runtime sysroot not found"));
        }

        // Run DNF makecache for the runtime sysroot
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot={installroot_path} \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch metadata for runtime '{runtime}'"
            ));
        }

        print_success(
            &format!("Successfully fetched metadata for runtime '{runtime}'"),
            OutputLevel::Normal,
        );
        Ok(())
    }

    async fn fetch_all_metadata(
        &self,
        config_toml: &serde_yaml::Value,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info(
            "Fetching repository metadata for all sysroots",
            OutputLevel::Normal,
        );

        // 1. Fetch SDK host metadata
        self.fetch_sdk_host_metadata(container_config).await?;

        // 2. Fetch rootfs metadata
        self.fetch_rootfs_metadata(container_config).await?;

        // 3. Fetch SDK target sysroot metadata
        self.fetch_sdk_target_metadata(container_config).await?;

        // 4. Fetch all extension metadata (including nested external extensions)
        if let Some(extensions) = config_toml
            .get("extensions")
            .and_then(|ext| ext.as_mapping())
        {
            for extension_name_val in extensions.keys() {
                if let Some(extension_name) = extension_name_val.as_str() {
                    if let Err(e) = self
                        .fetch_extension_metadata(config_toml, extension_name, container_config)
                        .await
                    {
                        print_error(
                            &format!(
                                "Failed to fetch metadata for extension '{extension_name}': {e}"
                            ),
                            OutputLevel::Normal,
                        );
                        // Continue with other extensions instead of failing completely
                    }
                }
            }
        }

        // 4.5. Fetch metadata for all external extensions (including nested ones)
        let config = Config::load(&self.config_path)?;
        let all_external_extensions =
            self.discover_all_external_extensions(&config, config_toml)?;
        for ext_dep in all_external_extensions {
            if let Err(e) = self
                .fetch_external_extension_metadata(&ext_dep, container_config)
                .await
            {
                let ext_name = match &ext_dep {
                    ExtensionDependency::External { name, .. } => name,
                    _ => continue,
                };
                print_error(
                    &format!("Failed to fetch metadata for external extension '{ext_name}': {e}"),
                    OutputLevel::Normal,
                );
                // Continue with other extensions instead of failing completely
            }
        }

        // 5. Fetch all runtime metadata
        if let Some(runtimes) = config_toml.get("runtimes").and_then(|rt| rt.as_mapping()) {
            for runtime_name_val in runtimes.keys() {
                if let Some(runtime_name) = runtime_name_val.as_str() {
                    if let Err(e) = self
                        .fetch_runtime_metadata(config_toml, runtime_name, container_config)
                        .await
                    {
                        print_error(
                            &format!("Failed to fetch metadata for runtime '{runtime_name}': {e}"),
                            OutputLevel::Normal,
                        );
                        // Continue with other runtimes instead of failing completely
                    }
                }
            }
        }

        Ok(())
    }

    async fn fetch_sdk_host_metadata(&self, container_config: &ContainerConfig<'_>) -> Result<()> {
        print_info("Fetching SDK host metadata", OutputLevel::Normal);

        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
$DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!("Failed to fetch SDK host metadata"));
        }

        print_success(
            "Successfully fetched SDK host metadata",
            OutputLevel::Normal,
        );
        Ok(())
    }

    async fn fetch_rootfs_metadata(&self, container_config: &ContainerConfig<'_>) -> Result<()> {
        print_info("Fetching rootfs metadata", OutputLevel::Normal);

        // Check if rootfs exists
        let check_command = "[ -d $AVOCADO_PREFIX/rootfs ]";
        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: check_command.to_string(),
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let rootfs_exists = container_config.helper.run_in_container(run_config).await?;

        if !rootfs_exists {
            print_error(
                "Rootfs sysroot does not exist. Run 'avocado sdk install' first.",
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Rootfs sysroot not found"));
        }

        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot=$AVOCADO_PREFIX/rootfs \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!("Failed to fetch rootfs metadata"));
        }

        print_success("Successfully fetched rootfs metadata", OutputLevel::Normal);
        Ok(())
    }

    async fn fetch_sdk_target_metadata(
        &self,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info("Fetching SDK target sysroot metadata", OutputLevel::Normal);

        // Check if SDK target sysroot exists
        let check_command = "[ -d $AVOCADO_PREFIX/sdk/target-sysroot ]";
        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: check_command.to_string(),
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let target_sysroot_exists = container_config.helper.run_in_container(run_config).await?;

        if !target_sysroot_exists {
            print_error(
                "SDK target sysroot does not exist. Run 'avocado sdk install' first.",
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("SDK target sysroot not found"));
        }

        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot=$AVOCADO_PREFIX/sdk/target-sysroot \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch SDK target sysroot metadata"
            ));
        }

        print_success(
            "Successfully fetched SDK target sysroot metadata",
            OutputLevel::Normal,
        );
        Ok(())
    }

    async fn pull_sdk_image(&self, container_image: &str) -> Result<()> {
        print_info(
            &format!("Pulling latest SDK container image: {container_image}"),
            OutputLevel::Normal,
        );

        // Determine the container tool to use (docker or podman)
        let container_tool =
            std::env::var("AVOCADO_CONTAINER_TOOL").unwrap_or_else(|_| "docker".to_string());

        if self.verbose {
            print_info(
                &format!("Running command: {container_tool} pull {container_image}"),
                OutputLevel::Normal,
            );
        }

        let mut cmd = AsyncCommand::new(&container_tool);
        cmd.arg("pull").arg(container_image);

        let output = cmd.output().await?;

        if output.status.success() {
            print_success(
                &format!("Successfully pulled SDK container image: {container_image}"),
                OutputLevel::Normal,
            );
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            print_error(
                &format!("Failed to pull SDK container image: {stderr}"),
                OutputLevel::Normal,
            );
            Err(anyhow::anyhow!(
                "Failed to pull SDK container image: {stderr}"
            ))
        }
    }

    /// Discover all external extensions (including nested ones) that have sysroots
    fn discover_all_external_extensions(
        &self,
        config: &Config,
        config_toml: &serde_yaml::Value,
    ) -> Result<Vec<ExtensionDependency>> {
        let mut all_external_extensions = HashSet::new();
        let mut visited = HashSet::new();

        // Find external extensions from main config
        if let Some(extensions) = config_toml
            .get("extensions")
            .and_then(|ext| ext.as_mapping())
        {
            for (ext_name_val, ext_config) in extensions {
                if let Some(ext_name) = ext_name_val.as_str() {
                    if let Some(dependencies) =
                        ext_config.get("packages").and_then(|d| d.as_mapping())
                    {
                        for (_dep_name, dep_spec) in dependencies {
                            // Check for external extension dependency
                            if let Some(external_config) =
                                dep_spec.get("config").and_then(|v| v.as_str())
                            {
                                let ext_dep = ExtensionDependency::External {
                                    name: ext_name.to_string(),
                                    config_path: external_config.to_string(),
                                };
                                all_external_extensions.insert(ext_dep.clone());

                                // Recursively find nested external extension dependencies
                                self.find_nested_external_extensions(
                                    config,
                                    &ext_dep,
                                    &mut all_external_extensions,
                                    &mut visited,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        Ok(all_external_extensions.into_iter().collect())
    }

    /// Recursively find nested external extension dependencies
    fn find_nested_external_extensions(
        &self,
        config: &Config,
        ext_dep: &ExtensionDependency,
        all_external_extensions: &mut HashSet<ExtensionDependency>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        let (ext_name, ext_config_path) = match ext_dep {
            ExtensionDependency::External { name, config_path } => (name, config_path),
            ExtensionDependency::Local(_) => return Ok(()), // Local extensions don't have nested external deps
            ExtensionDependency::Versioned { .. } => return Ok(()), // Versioned extensions don't have nested deps
        };

        // Cycle detection: check if we've already processed this extension
        let ext_key = format!("{ext_name}:{ext_config_path}");
        if visited.contains(&ext_key) {
            if self.verbose {
                print_info(
                    &format!("Skipping already processed extension '{ext_name}' to avoid cycles"),
                    OutputLevel::Normal,
                );
            }
            return Ok(());
        }
        visited.insert(ext_key);

        // Load the external extension configuration
        let resolved_external_config_path =
            config.resolve_path_relative_to_src_dir(&self.config_path, ext_config_path);
        let external_extensions =
            config.load_external_extensions(&self.config_path, ext_config_path)?;

        let extension_config = external_extensions.get(ext_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{ext_name}' not found in external config file '{ext_config_path}'"
            )
        })?;

        // Load the nested config file to get its src_dir setting
        let nested_config_content = std::fs::read_to_string(&resolved_external_config_path)
            .with_context(|| {
                format!(
                    "Failed to read nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;
        let nested_config: serde_yaml::Value = serde_yaml::from_str(&nested_config_content)
            .with_context(|| {
                format!(
                    "Failed to parse nested config file: {}",
                    resolved_external_config_path.display()
                )
            })?;

        // Create a temporary Config object for the nested config to handle its src_dir
        let nested_config_obj = serde_yaml::from_value::<Config>(nested_config.clone())?;

        // Check if this external extension has dependencies
        if let Some(dependencies) = extension_config
            .get("packages")
            .and_then(|d| d.as_mapping())
        {
            for (_dep_name, dep_spec) in dependencies {
                // Check for nested extension dependency
                if let Some(nested_ext_name) = dep_spec.get("extensions").and_then(|v| v.as_str()) {
                    // Check if this is a nested external extension (has config field)
                    if let Some(nested_external_config) =
                        dep_spec.get("config").and_then(|v| v.as_str())
                    {
                        // Resolve the nested config path relative to the nested config's src_dir
                        let nested_config_path = nested_config_obj
                            .resolve_path_relative_to_src_dir(
                                &resolved_external_config_path,
                                nested_external_config,
                            );

                        let nested_ext_dep = ExtensionDependency::External {
                            name: nested_ext_name.to_string(),
                            config_path: nested_config_path.to_string_lossy().to_string(),
                        };

                        // Add the nested extension to all extensions
                        all_external_extensions.insert(nested_ext_dep.clone());

                        if self.verbose {
                            print_info(
                                &format!("Found nested external extension '{nested_ext_name}' required by '{ext_name}' at '{}'", nested_config_path.display()),
                                OutputLevel::Normal,
                            );
                        }

                        // Recursively process the nested extension
                        self.find_nested_external_extensions(
                            config,
                            &nested_ext_dep,
                            all_external_extensions,
                            visited,
                        )?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Fetch metadata for an external extension
    async fn fetch_external_extension_metadata(
        &self,
        ext_dep: &ExtensionDependency,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        let (ext_name, _ext_config_path) = match ext_dep {
            ExtensionDependency::External { name, config_path } => (name, config_path),
            _ => return Ok(()), // Only handle external extensions
        };

        print_info(
            &format!("Fetching repository metadata for external extension '{ext_name}'"),
            OutputLevel::Normal,
        );

        // Check if extension sysroot exists
        let check_command = format!("[ -d $AVOCADO_EXT_SYSROOTS/{ext_name} ]");
        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: check_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let sysroot_exists = container_config.helper.run_in_container(run_config).await?;

        if !sysroot_exists {
            if self.verbose {
                print_info(
                    &format!(
                        "Extension sysroot '{ext_name}' does not exist, skipping metadata fetch"
                    ),
                    OutputLevel::Normal,
                );
            }
            return Ok(());
        }

        // Run DNF makecache for the extension sysroot
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let makecache_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/ext-rpm-config \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --setopt=sslcacert=${{SSL_CERT_FILE}} \
    --installroot=$AVOCADO_EXT_SYSROOTS/{ext_name} \
    {dnf_args_str} \
    makecache
"#
        );

        if self.verbose {
            print_info(
                &format!("Running command: {makecache_command}"),
                OutputLevel::Normal,
            );
        }

        let run_config = RunConfig {
            container_image: container_config.image.to_string(),
            target: container_config.target_arch.to_string(),
            command: makecache_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: container_config.repo_url.cloned(),
            repo_release: container_config.repo_release.cloned(),
            container_args: container_config.container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let success = container_config.helper.run_in_container(run_config).await?;

        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch metadata for external extension '{ext_name}'"
            ));
        }

        print_success(
            &format!("Successfully fetched metadata for external extension '{ext_name}'"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}
