//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    lockfile::{build_package_spec_with_lock, LockFile, SysrootType},
    output::{print_error, print_info, print_success, OutputLevel},
    runs_on::RunsOnContext,
    stamps::{
        compute_sdk_input_hash, generate_write_sdk_stamp_script_dynamic_arch,
        generate_write_stamp_script, get_local_arch, Stamp, StampOutputs,
    },
    target::validate_and_log_target,
};

/// Implementation of the 'sdk install' command.
pub struct SdkInstallCommand {
    /// Path to configuration file
    pub config_path: String,
    /// Enable verbose output
    pub verbose: bool,
    /// Force operation without prompts
    pub force: bool,
    /// Global target architecture
    pub target: Option<String>,
    /// Additional arguments to pass to the container runtime
    pub container_args: Option<Vec<String>>,
    /// Additional arguments to pass to DNF commands
    pub dnf_args: Option<Vec<String>>,
    /// Disable stamp validation and writing
    pub no_stamps: bool,
    /// Remote host to run on (format: user@host)
    pub runs_on: Option<String>,
    /// NFS port for remote execution
    pub nfs_port: Option<u16>,
    /// SDK container architecture for cross-arch emulation
    pub sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl SdkInstallCommand {
    /// Create a new SdkInstallCommand instance
    pub fn new(
        config_path: String,
        verbose: bool,
        force: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            verbose,
            force,
            target,
            container_args,
            dnf_args,
            no_stamps: false,
            runs_on: None,
            nfs_port: None,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Set remote execution options
    pub fn with_runs_on(mut self, runs_on: Option<String>, nfs_port: Option<u16>) -> Self {
        self.runs_on = runs_on;
        self.nfs_port = nfs_port;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    /// Execute the sdk install command
    pub async fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref()).with_context(
                    || format!("Failed to load composed config from {}", self.config_path),
                )?,
            ),
        };

        let config = &composed.config;
        let target = validate_and_log_target(self.target.as_deref(), config)?;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        print_info("Installing SDK dependencies.", OutputLevel::Normal);

        // Get SDK dependencies from the composed config (already has external deps merged)
        let sdk_dependencies = config
            .get_sdk_dependencies_for_target(&self.config_path, &target)
            .with_context(|| "Failed to get SDK dependencies with target interpolation")?;

        // Note: extension_sdk_dependencies is computed inside execute_install after
        // fetching remote extensions, since we need SDK repos to be available first

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Use the container helper to run the installation
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Create shared RunsOnContext if running on remote host
        // This allows reusing the NFS server and volumes for all container runs
        let mut runs_on_context: Option<RunsOnContext> = if let Some(ref runs_on) = self.runs_on {
            Some(
                container_helper
                    .create_runs_on_context(runs_on, self.nfs_port, container_image, self.verbose)
                    .await?,
            )
        } else {
            None
        };

        // Execute the main installation logic, ensuring cleanup on error
        let result = self
            .execute_install(
                config,
                &target,
                container_image,
                &sdk_dependencies,
                repo_url.as_deref(),
                repo_release.as_deref(),
                &container_helper,
                merged_container_args.as_ref(),
                runs_on_context.as_ref(),
            )
            .await;

        // Always teardown the context if it was created
        if let Some(ref mut context) = runs_on_context {
            if let Err(e) = context.teardown().await {
                print_error(
                    &format!("Warning: Failed to cleanup remote resources: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        result
    }

    /// Fetch remote extensions after SDK bootstrap
    ///
    /// This discovers extensions with a `source` field and fetches them
    /// using the SDK environment where repos are already configured.
    async fn fetch_remote_extensions_in_sdk(
        &self,
        target: &str,
        merged_container_args: Option<&Vec<String>>,
    ) -> Result<()> {
        use crate::commands::ext::ExtFetchCommand;

        // Discover remote extensions (with target interpolation for extension names)
        let remote_extensions =
            Config::discover_remote_extensions(&self.config_path, Some(target))?;

        if remote_extensions.is_empty() {
            return Ok(());
        }

        print_info(
            &format!(
                "Fetching {} remote extension(s)...",
                remote_extensions.len()
            ),
            OutputLevel::Normal,
        );

        // Use ExtFetchCommand to fetch extensions with SDK environment
        let mut fetch_cmd = ExtFetchCommand::new(
            self.config_path.clone(),
            None, // Fetch all remote extensions
            self.verbose,
            false, // Don't force re-fetch
            Some(target.to_string()),
            merged_container_args.cloned(),
        )
        .with_sdk_arch(self.sdk_arch.clone());

        // Pass through the runs_on context for remote execution
        if let Some(runs_on) = &self.runs_on {
            fetch_cmd = fetch_cmd.with_runs_on(runs_on.clone(), self.nfs_port);
        }

        fetch_cmd.execute().await?;

        Ok(())
    }

    /// Internal implementation of the install logic
    #[allow(clippy::too_many_arguments)]
    async fn execute_install(
        &self,
        config: &Config,
        target: &str,
        container_image: &str,
        sdk_dependencies: &Option<HashMap<String, serde_yaml::Value>>,
        repo_url: Option<&str>,
        repo_release: Option<&str>,
        container_helper: &SdkContainer,
        merged_container_args: Option<&Vec<String>>,
        runs_on_context: Option<&RunsOnContext>,
    ) -> Result<()> {
        // Determine host architecture for SDK package tracking
        // For remote execution, query the remote host; for local, use local arch
        let host_arch = if let Some(context) = runs_on_context {
            context
                .get_host_arch()
                .await
                .with_context(|| "Failed to get remote host architecture")?
        } else {
            get_local_arch().to_string()
        };

        // Create SDK sysroot type with the host architecture
        let sdk_sysroot = SysrootType::Sdk(host_arch.clone());

        if self.verbose {
            print_info(
                &format!("Using host architecture '{host_arch}' for SDK package tracking."),
                OutputLevel::Normal,
            );
        }

        // Load lock file for reproducible builds
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                PathBuf::from(&self.config_path)
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            });
        let mut lock_file = LockFile::load(&src_dir).with_context(|| "Failed to load lock file")?;

        if self.verbose && !lock_file.is_empty() {
            print_info(
                "Using existing lock file for version pinning.",
                OutputLevel::Normal,
            );
        }

        // Initialize SDK environment first (creates directories, copies configs, sets up wrappers)
        print_info("Initializing SDK environment.", OutputLevel::Normal);

        let sdk_init_command = r#"
echo "[INFO] Initializing Avocado SDK."
mkdir -p $AVOCADO_SDK_PREFIX/etc
mkdir -p $AVOCADO_EXT_SYSROOTS
cp /etc/rpmrc $AVOCADO_SDK_PREFIX/etc
cp -r /etc/rpm $AVOCADO_SDK_PREFIX/etc
cp -r /etc/dnf $AVOCADO_SDK_PREFIX/etc
cp -r /etc/yum.repos.d $AVOCADO_SDK_PREFIX/etc

mkdir -p $AVOCADO_SDK_PREFIX/usr/lib/rpm
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/usr/lib/rpm/

# Before calling DNF, $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros needs to be updated to point:
#   - /usr -> $AVOCADO_SDK_PREFIX/usr
#   - /var -> $AVOCADO_SDK_PREFIX/var
sed -i "s|^%_usr[[:space:]]*/usr$|%_usr                   $AVOCADO_SDK_PREFIX/usr|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros
sed -i "s|^%_var[[:space:]]*/var$|%_var                   $AVOCADO_SDK_PREFIX/var|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros

# Create separate rpm config for versioned extensions with custom %_dbpath
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/ext-rpm-config/
# Update macros for versioned extensions to use extension.d/rpm database location
sed -i "s|^%_dbpath[[:space:]]*%{_var}/lib/rpm$|%_dbpath                %{_var}/lib/extension.d/rpm|" $AVOCADO_SDK_PREFIX/ext-rpm-config/macros

# Create separate rpm config for extension scriptlets with selective execution
# This allows only update-alternatives and opkg to run, blocking other scriptlet commands
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts
cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/

# Create a bin directory for command wrappers
mkdir -p $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin

# Create update-alternatives wrapper that uses OPKG_OFFLINE_ROOT
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/update-alternatives << 'UAWRAPPER_EOF'
#!/bin/bash
# update-alternatives wrapper for extension scriptlets
# Sets OPKG_OFFLINE_ROOT to manage alternatives within the extension sysroot

if [ -n "$AVOCADO_EXT_INSTALLROOT" ]; then
    case "$1" in
        --install|--remove|--config|--auto|--display|--list|--query|--set)
            # Debug: Show what we're doing
            echo "update-alternatives: OPKG_OFFLINE_ROOT=$AVOCADO_EXT_INSTALLROOT"
            echo "update-alternatives: executing: update-alternatives $*"

            # Set OPKG_OFFLINE_ROOT to the extension's installroot
            # This tells opkg-update-alternatives to operate within that root
            # Also ensure alternatives directory is created
            /usr/bin/mkdir -p "${AVOCADO_EXT_INSTALLROOT}/var/lib/opkg/alternatives" 2>/dev/null || true

            # Set clean PATH and call update-alternatives with OPKG_OFFLINE_ROOT
            export OPKG_OFFLINE_ROOT="$AVOCADO_EXT_INSTALLROOT"
            PATH="${AVOCADO_SDK_PREFIX}/usr/bin:/usr/bin:/bin" \
                exec ${AVOCADO_SDK_PREFIX}/usr/bin/update-alternatives "$@"
            ;;
    esac
fi

# If called without AVOCADO_EXT_INSTALLROOT, fail safely
exit 0
UAWRAPPER_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/update-alternatives

# Create opkg wrapper
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/opkg << 'OPKGWRAPPER_EOF'
#!/bin/bash
# opkg wrapper for extension scriptlets
exec ${AVOCADO_SDK_PREFIX}/usr/bin/opkg "$@"
OPKGWRAPPER_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/opkg

# Create generic noop wrapper for commands we don't want to execute
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/noop-command << 'NOOP_EOF'
#!/bin/bash
# Generic noop wrapper - always succeeds
exit 0
NOOP_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/noop-command

# Create a smart grep wrapper that pretends users/groups exist
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/grep << 'GREP_EOF'
#!/bin/bash
# Smart grep wrapper for scriptlet user/group validation
# When checking /etc/passwd or /etc/group, pretend the user/group exists
# For everything else, use the real grep

# Check if this looks like a user/group existence check
if [[ "$*" =~ /etc/passwd ]] || [[ "$*" =~ /etc/group ]]; then
    # Pretend we found a match - output a fake line and exit 0
    echo "placeholder:x:1000:1000::/:/bin/false"
    exit 0
fi

# For everything else, use real grep (find it in original PATH, not our wrapper dir)
# Remove our wrapper directory from PATH to find the real grep
ORIGINAL_PATH="${PATH#${AVOCADO_SDK_PREFIX}/ext-rpm-config-scripts/bin:}"
exec env PATH="$ORIGINAL_PATH" grep "$@"
GREP_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/grep

# Create symlinks for common scriptlet commands that should noop
# Allowlist approach: we create wrappers for what we DON'T want, not for what we DO want
for cmd in useradd groupadd usermod groupmod userdel groupdel chown chmod chgrp \
           flock systemctl systemd-tmpfiles ldconfig depmod udevadm \
           dbus-send killall service update-rc.d invoke-rc.d \
           gtk-update-icon-cache glib-compile-schemas update-desktop-database \
           fc-cache mkfontdir mkfontscale install-info update-mime-database \
           passwd chpasswd gpasswd newusers \
           systemd-sysusers systemd-hwdb kmod insmod modprobe \
           setcap getcap chcon restorecon selinuxenabled getenforce \
           rpm-helper gtk-query-immodules-3.0 \
           gdk-pixbuf-query-loaders gio-querymodules \
           dconf gsettings glib-compile-resources \
           bbnote bbfatal bbwarn bbdebug; do
    ln -sf noop-command $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/bin/$cmd
done

# Create shell wrapper for scriptlet interpreter
cat > $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh << 'SHELL_EOF'
#!/bin/bash
# Shell wrapper for RPM scriptlets
# Set OPT=--opt to make Yocto scriptlets skip user/group management
# This is the proper way to tell Yocto scripts we're in a sysroot environment

# Set PATH to find our command wrappers first, but explicitly exclude the installroot
# Only include: wrapper bin, SDK utilities, and container system paths
export PATH="${AVOCADO_SDK_PREFIX}/ext-rpm-config-scripts/bin:${AVOCADO_SDK_PREFIX}/usr/bin:/usr/bin:/bin"

# Tell Yocto scriptlets we're in OPT mode (skip user/group creation)
export OPT="--opt"

exec ${AVOCADO_SDK_PREFIX}/usr/bin/bash "$@"
SHELL_EOF
chmod +x $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh

# Update macros for extension scriptlets
sed -i "s|^%_dbpath[[:space:]]*%{_var}/lib/rpm$|%_dbpath                %{_var}/lib/rpm|" $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/macros

# Add macro overrides for shell interpreter only
cat >> $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/macros << 'MACROS_EOF'

# Override shell interpreter for scriptlets to use our custom shell
%__bash                 $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh
%__sh                   $AVOCADO_SDK_PREFIX/ext-rpm-config-scripts/scriptlet-shell.sh
MACROS_EOF
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: sdk_init_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        let init_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        if init_success {
            print_success("Initialized SDK environment.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to initialize SDK environment."));
        }

        // Install avocado-sdk-{target} with version from distro.version
        print_info(
            &format!("Installing SDK for target '{target}'."),
            OutputLevel::Normal,
        );

        // Build package name and spec with lock file support
        let sdk_target_pkg_name = format!("avocado-sdk-{target}");
        let sdk_target_config_version = config
            .get_distro_version()
            .map(|s| s.as_str())
            .unwrap_or("*");
        let sdk_target_pkg = build_package_spec_with_lock(
            &lock_file,
            target,
            &sdk_sysroot,
            &sdk_target_pkg_name,
            sdk_target_config_version,
        );

        let sdk_target_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_HOST_REPO_CONF \
    -y \
    install \
    {sdk_target_pkg}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: sdk_target_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        let sdk_target_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Track all SDK packages installed for lock file update at the end
        let mut all_sdk_package_names: Vec<String> = Vec::new();

        if sdk_target_success {
            print_success(
                &format!("Installed SDK for target '{target}'."),
                OutputLevel::Normal,
            );
            // Add to list for later query (after environment is fully set up)
            all_sdk_package_names.push(sdk_target_pkg_name);
        } else {
            return Err(anyhow::anyhow!(
                "Failed to install SDK for target '{target}'."
            ));
        }

        // Run check-update to refresh metadata using the combined repo config.
        // This uses arch-specific varsdir for correct architecture filtering,
        // with repos from both arch-specific SDK and target-repoconf.
        let check_update_command = r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    check-update || true
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_update_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Install avocado-sdk-bootstrap with version from distro.version
        print_info("Installing SDK bootstrap.", OutputLevel::Normal);

        let bootstrap_pkg_name = "avocado-sdk-bootstrap";
        let bootstrap_config_version = config
            .get_distro_version()
            .map(|s| s.as_str())
            .unwrap_or("*");
        let bootstrap_pkg = build_package_spec_with_lock(
            &lock_file,
            target,
            &sdk_sysroot,
            bootstrap_pkg_name,
            bootstrap_config_version,
        );

        // Use combined repo config for bootstrap installation.
        // The bootstrap package is a nativesdk package that needs both the base repos
        // (from arch-specific SDK) and target-specific repos (from target-repoconf).
        let bootstrap_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    -y \
    install \
    {bootstrap_pkg}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: bootstrap_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        let bootstrap_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        if bootstrap_success {
            print_success("Installed SDK bootstrap.", OutputLevel::Normal);
            // Add to list for later query (after environment is fully set up)
            all_sdk_package_names.push(bootstrap_pkg_name.to_string());
        } else {
            return Err(anyhow::anyhow!("Failed to install SDK bootstrap."));
        }

        // Fetch remote extensions now that SDK repos are available
        // This uses the SDK environment with configured repos to download extension packages
        self.fetch_remote_extensions_in_sdk(target, merged_container_args)
            .await?;

        // Reload composed config to include extension configs
        let composed = Config::load_composed(&self.config_path, Some(target))
            .with_context(|| "Failed to reload composed config after fetching extensions")?;
        let config = &composed.config;

        // Re-compute extension SDK dependencies now that extension configs are available
        let config_content = serde_yaml::to_string(&composed.merged_value)
            .with_context(|| "Failed to serialize composed config")?;
        let extension_sdk_dependencies = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                &config_content,
                Some(&self.config_path),
                Some(target),
            )
            .with_context(|| "Failed to parse extension SDK dependencies")?;

        // After bootstrap, source environment-setup and configure SSL certs for subsequent commands
        if self.verbose {
            print_info(
                "Configuring SDK environment after bootstrap.",
                OutputLevel::Normal,
            );
        }

        let env_setup_command = r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
    echo "[INFO] Sourced SDK environment setup."
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"
    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
    echo "[INFO] SSL certificates configured."
fi
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: env_setup_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        // Install SDK dependencies (into SDK)
        let mut sdk_packages = Vec::new();
        let mut sdk_package_names = Vec::new();

        // Add regular SDK dependencies
        if let Some(ref dependencies) = sdk_dependencies {
            sdk_packages.extend(self.build_package_list_with_lock(
                dependencies,
                &lock_file,
                target,
                &sdk_sysroot,
            ));
            sdk_package_names.extend(self.extract_package_names(dependencies));
        }

        // Add extension SDK dependencies to the package list
        for (ext_name, ext_deps) in &extension_sdk_dependencies {
            if self.verbose {
                print_info(
                    &format!("Adding SDK dependencies from extension '{ext_name}'"),
                    OutputLevel::Normal,
                );
            }
            let ext_packages =
                self.build_package_list_with_lock(ext_deps, &lock_file, target, &sdk_sysroot);
            sdk_packages.extend(ext_packages);
            sdk_package_names.extend(self.extract_package_names(ext_deps));
        }

        if !sdk_packages.is_empty() {
            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            // Use combined repo config for SDK dependencies.
            // SDK dependencies are nativesdk packages that need both the base repos
            // (from arch-specific SDK) and target-specific repos (from target-repoconf).
            // The combined config uses arch-specific varsdir for correct architecture
            // filtering, which is critical for --runs-on with cross-arch targets.
            let command = format!(
                r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} \
    {} \
    install \
    {}
"#,
                dnf_args_str,
                yes,
                sdk_packages.join(" ")
            );

            // Use the container helper's run_in_container method
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command,
                verbose: self.verbose,
                source_environment: true,
                interactive: !self.force,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                // runs_on handled by shared context
                ..Default::default()
            };
            let install_success = run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if install_success {
                print_success("Installed SDK dependencies.", OutputLevel::Normal);
                // Add SDK dependency package names to the list
                all_sdk_package_names.extend(sdk_package_names);
            } else {
                return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
            }
        } else {
            print_success("No dependencies configured.", OutputLevel::Normal);
        }

        // Query all SDK packages at once (bootstrap + dependencies)
        // This is done after environment-setup is sourced for reliability
        if !all_sdk_package_names.is_empty() {
            let installed_versions = container_helper
                .query_installed_packages(
                    &sdk_sysroot,
                    &all_sdk_package_names,
                    container_image,
                    target,
                    repo_url.map(|s| s.to_string()),
                    repo_release.map(|s| s.to_string()),
                    merged_container_args.cloned(),
                    runs_on_context,
                )
                .await?;

            if !installed_versions.is_empty() {
                lock_file.update_sysroot_versions(target, &sdk_sysroot, installed_versions);
                if self.verbose {
                    print_info(
                        &format!(
                            "Updated lock file with {} SDK package versions.",
                            all_sdk_package_names.len()
                        ),
                        OutputLevel::Normal,
                    );
                }
                // Save lock file immediately after SDK install
                lock_file.save(&src_dir)?;
            }
        }

        // Install rootfs sysroot with version from distro.version
        print_info("Installing rootfs sysroot.", OutputLevel::Normal);

        let rootfs_base_pkg = "avocado-pkg-rootfs";
        let rootfs_config_version = config
            .get_distro_version()
            .map(|s| s.as_str())
            .unwrap_or("*");
        let rootfs_pkg = build_package_spec_with_lock(
            &lock_file,
            target,
            &SysrootType::Rootfs,
            rootfs_base_pkg,
            rootfs_config_version,
        );

        let yes = if self.force { "-y" } else { "" };
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        let rootfs_command = format!(
            r#"
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_TARGET_REPO_CONF \
    {dnf_args_str} {yes} --installroot $AVOCADO_PREFIX/rootfs install {rootfs_pkg}
"#
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: rootfs_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: !self.force,
            repo_url: repo_url.map(|s| s.to_string()),
            repo_release: repo_release.map(|s| s.to_string()),
            container_args: merged_container_args.cloned(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            // runs_on handled by shared context
            ..Default::default()
        };

        let rootfs_success = run_container_command(
            container_helper,
            run_config,
            runs_on_context,
            self.sdk_arch.as_ref(),
        )
        .await?;

        if rootfs_success {
            print_success("Installed rootfs sysroot.", OutputLevel::Normal);

            // Query installed version and update lock file
            let installed_versions = container_helper
                .query_installed_packages(
                    &SysrootType::Rootfs,
                    &[rootfs_base_pkg.to_string()],
                    container_image,
                    target,
                    repo_url.map(|s| s.to_string()),
                    repo_release.map(|s| s.to_string()),
                    merged_container_args.cloned(),
                    runs_on_context,
                )
                .await?;

            if !installed_versions.is_empty() {
                lock_file.update_sysroot_versions(target, &SysrootType::Rootfs, installed_versions);
                if self.verbose {
                    print_info(
                        "Updated lock file with rootfs package version.",
                        OutputLevel::Normal,
                    );
                }
                // Save lock file immediately after rootfs install
                lock_file.save(&src_dir)?;
            }
        } else {
            return Err(anyhow::anyhow!("Failed to install rootfs sysroot."));
        }

        // Install target-sysroot if there are any sdk.compile sections defined
        // (regardless of whether they have dependencies).
        // This is needed for cross-compilation support.
        // The composed config already has external extension compile sections merged in.
        if config.has_compile_sections() {
            // Aggregate all compile dependencies into a single list (with lock file support)
            let compile_dependencies = config.get_compile_dependencies();
            let mut all_compile_packages: Vec<String> = Vec::new();
            let mut all_compile_package_names: Vec<String> = Vec::new();
            for dependencies in compile_dependencies.values() {
                let packages = self.build_package_list_with_lock(
                    dependencies,
                    &lock_file,
                    target,
                    &SysrootType::TargetSysroot,
                );
                all_compile_packages.extend(packages);
                all_compile_package_names.extend(self.extract_package_names(dependencies));
            }

            // Deduplicate packages
            all_compile_packages.sort();
            all_compile_packages.dedup();
            all_compile_package_names.sort();
            all_compile_package_names.dedup();

            print_info(
                &format!(
                    "Installing target-sysroot with {} compile dependencies.",
                    all_compile_packages.len()
                ),
                OutputLevel::Normal,
            );

            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            // Build the target-sysroot package spec with version from distro.version (with lock)
            let target_sysroot_base_pkg = "avocado-sdk-target-sysroot";
            let target_sysroot_config_version = config
                .get_distro_version()
                .map(|s| s.as_str())
                .unwrap_or("*");
            let target_sysroot_pkg = build_package_spec_with_lock(
                &lock_file,
                target,
                &SysrootType::TargetSysroot,
                target_sysroot_base_pkg,
                target_sysroot_config_version,
            );

            // Install the target-sysroot with avocado-sdk-target-sysroot plus compile deps
            let command = format!(
                r#"
unset RPM_CONFIGDIR
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_TARGET_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} {} --installroot ${{AVOCADO_PREFIX}}/sdk/target-sysroot \
    install {} {}
"#,
                dnf_args_str,
                yes,
                target_sysroot_pkg,
                all_compile_packages.join(" ")
            );

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command,
                verbose: self.verbose,
                source_environment: false, // Don't source environment - matches rootfs install behavior
                interactive: !self.force,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                // runs_on handled by shared context
                ..Default::default()
            };

            let install_success = run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if install_success {
                print_success(
                    "Installed target-sysroot with compile dependencies.",
                    OutputLevel::Normal,
                );

                // Query installed versions and update lock file
                let mut packages_to_query = all_compile_package_names;
                packages_to_query.push(target_sysroot_base_pkg.to_string());

                let installed_versions = container_helper
                    .query_installed_packages(
                        &SysrootType::TargetSysroot,
                        &packages_to_query,
                        container_image,
                        target,
                        repo_url.map(|s| s.to_string()),
                        repo_release.map(|s| s.to_string()),
                        merged_container_args.cloned(),
                        runs_on_context,
                    )
                    .await?;

                if !installed_versions.is_empty() {
                    lock_file.update_sysroot_versions(
                        target,
                        &SysrootType::TargetSysroot,
                        installed_versions,
                    );
                    if self.verbose {
                        print_info(
                            "Updated lock file with target-sysroot package versions.",
                            OutputLevel::Normal,
                        );
                    }
                    // Save lock file immediately after target-sysroot install
                    lock_file.save(&src_dir)?;
                }
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to install target-sysroot with compile dependencies."
                ));
            }
        }

        // Write SDK install stamp (unless --no-stamps)
        // The stamp uses the host architecture (CPU arch where SDK runs) rather than
        // the target architecture (what you're building for). This allows --runs-on
        // to detect if the SDK is installed for the remote's architecture.
        if !self.no_stamps {
            let inputs = compute_sdk_input_hash(&composed.merged_value)?;

            // When using --runs-on, we need to detect the remote architecture dynamically
            // since the remote host may have a different CPU arch than the local machine.
            // Otherwise, use the local architecture.
            let stamp_script = if self.runs_on.is_some() {
                // Use dynamic arch detection for remote execution
                generate_write_sdk_stamp_script_dynamic_arch(inputs)
            } else {
                // Use local architecture for local execution
                let outputs = StampOutputs::default();
                let host_arch = get_local_arch();
                let stamp = Stamp::sdk_install(host_arch, inputs, outputs);
                generate_write_stamp_script(&stamp)?
            };

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.to_string(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.map(|s| s.to_string()),
                repo_release: repo_release.map(|s| s.to_string()),
                container_args: merged_container_args.cloned(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                // runs_on handled by shared context
                ..Default::default()
            };

            run_container_command(
                container_helper,
                run_config,
                runs_on_context,
                self.sdk_arch.as_ref(),
            )
            .await?;

            if self.verbose {
                print_info("Wrote SDK install stamp.", OutputLevel::Normal);
            }
        }

        Ok(())
    }

    /// Build a list of packages from dependencies HashMap, using lock file for pinned versions
    fn build_package_list_with_lock(
        &self,
        dependencies: &HashMap<String, serde_yaml::Value>,
        lock_file: &LockFile,
        target: &str,
        sysroot: &SysrootType,
    ) -> Vec<String> {
        let mut packages = Vec::new();

        for (package_name, version) in dependencies {
            let config_version = match version {
                serde_yaml::Value::String(v) => v.clone(),
                serde_yaml::Value::Mapping(_) => "*".to_string(),
                _ => "*".to_string(),
            };

            let package_spec = build_package_spec_with_lock(
                lock_file,
                target,
                sysroot,
                package_name,
                &config_version,
            );
            packages.push(package_spec);
        }

        packages
    }

    /// Extract just the package names from a dependencies HashMap
    fn extract_package_names(
        &self,
        dependencies: &HashMap<String, serde_yaml::Value>,
    ) -> Vec<String> {
        dependencies.keys().cloned().collect()
    }
}

/// Helper function to run a container command, using shared context if available
async fn run_container_command(
    container_helper: &SdkContainer,
    mut config: RunConfig,
    runs_on_context: Option<&RunsOnContext>,
    sdk_arch: Option<&String>,
) -> Result<bool> {
    // Inject sdk_arch if provided
    if let Some(arch) = sdk_arch {
        config.sdk_arch = Some(arch.clone());
    }

    if let Some(context) = runs_on_context {
        // Use the shared context - don't set runs_on in config as we're handling it
        container_helper
            .run_in_container_with_context(&config, context)
            .await
    } else {
        // No shared context - use regular execution (may create its own context if runs_on is set)
        container_helper.run_in_container(config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;
    use std::collections::HashMap;

    #[test]
    fn test_build_package_list_with_lock() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);
        let lock_file = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));
        deps.insert(
            "package3".to_string(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );

        let packages = cmd.build_package_list_with_lock(&deps, &lock_file, target, &sdk_x86);

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"package1".to_string()));
        assert!(packages.contains(&"package2-1.0.0".to_string()));
        assert!(packages.contains(&"package3".to_string()));
    }

    #[test]
    fn test_build_package_list_with_lock_uses_locked_version() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);
        let mut lock_file = LockFile::new();
        let target = "qemux86-64";
        let sdk_x86 = SysrootType::Sdk("x86_64".to_string());

        // Add a locked version for package1
        lock_file.update_sysroot_versions(
            target,
            &sdk_x86,
            [("package1".to_string(), "2.0.0-r0.x86_64".to_string())]
                .into_iter()
                .collect(),
        );

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));

        let packages = cmd.build_package_list_with_lock(&deps, &lock_file, target, &sdk_x86);

        assert_eq!(packages.len(), 2);
        // package1 should use locked version instead of "*"
        assert!(packages.contains(&"package1-2.0.0-r0.x86_64".to_string()));
        // package2 has no lock entry, uses config version
        assert!(packages.contains(&"package2-1.0.0".to_string()));
    }

    #[test]
    fn test_new() {
        let cmd = SdkInstallCommand::new(
            "config.toml".to_string(),
            true,
            false,
            Some("test-target".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "config.toml");
        assert!(cmd.verbose);
        assert!(!cmd.force);
        assert_eq!(cmd.target, Some("test-target".to_string()));
    }
}
