use anyhow::Result;

use crate::utils::{
    config::Config,
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
        }
    }

    pub async fn execute(&self) -> Result<()> {
        // Load configuration
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let config_toml: toml::Value = toml::from_str(&content)?;

        // Resolve target architecture
        let target_arch = resolve_target_required(self.target.as_deref(), &config)?;

        // Get container configuration
        let container_image = config_toml
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .ok_or_else(|| anyhow::anyhow!("No SDK container image specified in configuration."))?;

        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Initialize container helper
        let container_helper = SdkContainer::new();

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
                self.fetch_extension_metadata(&config_toml, extension, &container_config)
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
                self.fetch_runtime_metadata(&config_toml, runtime, &container_config)
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
                self.fetch_all_metadata(&config_toml, &container_config)
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
        config_toml: &toml::Value,
        extension: &str,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info(
            &format!("Fetching repository metadata for extension '{extension}'"),
            OutputLevel::Normal,
        );

        // Check if extension exists in configuration
        if config_toml
            .get("ext")
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
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
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
        config_toml: &toml::Value,
        runtime: &str,
        container_config: &ContainerConfig<'_>,
    ) -> Result<()> {
        print_info(
            &format!("Fetching repository metadata for runtime '{runtime}'"),
            OutputLevel::Normal,
        );

        // Check if runtime exists in configuration
        if config_toml
            .get("runtime")
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
        config_toml: &toml::Value,
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

        // 4. Fetch all extension metadata
        if let Some(extensions) = config_toml.get("ext").and_then(|ext| ext.as_table()) {
            for extension_name in extensions.keys() {
                if let Err(e) = self
                    .fetch_extension_metadata(config_toml, extension_name, container_config)
                    .await
                {
                    print_error(
                        &format!("Failed to fetch metadata for extension '{extension_name}': {e}"),
                        OutputLevel::Normal,
                    );
                    // Continue with other extensions instead of failing completely
                }
            }
        }

        // 5. Fetch all runtime metadata
        if let Some(runtimes) = config_toml.get("runtime").and_then(|rt| rt.as_table()) {
            for runtime_name in runtimes.keys() {
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
        let check_command = "[ -d $AVOCADO_SDK_PREFIX/target-sysroot ]";
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
    --installroot=$AVOCADO_SDK_PREFIX/target-sysroot \
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
}
