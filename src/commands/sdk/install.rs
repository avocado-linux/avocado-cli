//! SDK install command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    stamps::{compute_sdk_input_hash, generate_write_stamp_script, Stamp, StampOutputs},
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
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Execute the sdk install command
    pub async fn execute(&self) -> Result<()> {
        // Early target validation - load basic config first
        let basic_config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let target = validate_and_log_target(self.target.as_deref(), &basic_config)?;

        // Load the composed configuration (merges external configs, applies interpolation)
        let composed = Config::load_composed(&self.config_path, self.target.as_deref())
            .with_context(|| format!("Failed to load composed config from {}", self.config_path))?;

        let config = &composed.config;

        // Merge container args from config with CLI args
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());

        // Serialize the merged config back to string for extension parsing methods
        let config_content = serde_yaml::to_string(&composed.merged_value)
            .with_context(|| "Failed to serialize composed config")?;

        // Get the SDK image from configuration
        let container_image = config.get_sdk_image().ok_or_else(|| {
            anyhow::anyhow!("No container image specified in config under 'sdk.image'")
        })?;

        print_info("Installing SDK dependencies.", OutputLevel::Normal);

        // Get SDK dependencies from the composed config (already has external deps merged)
        let sdk_dependencies = config
            .get_sdk_dependencies_for_target(&self.config_path, &target)
            .with_context(|| "Failed to get SDK dependencies with target interpolation")?;

        // Get extension SDK dependencies (from the composed, interpolated config)
        let extension_sdk_dependencies = config
            .get_extension_sdk_dependencies_with_config_path_and_target(
                &config_content,
                Some(&self.config_path),
                Some(&target),
            )
            .with_context(|| "Failed to parse extension SDK dependencies")?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        // Use the container helper to run the installation
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

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
            target: target.clone(),
            command: sdk_init_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let init_success = container_helper.run_in_container(run_config).await?;

        if init_success {
            print_success("Initialized SDK environment.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to initialize SDK environment."));
        }

        // Install avocado-sdk-{target} with version from distro.version
        print_info(
            &format!("Installing SDK for target '{}'.", target),
            OutputLevel::Normal,
        );

        let sdk_target_pkg = if let Some(version) = config.get_distro_version() {
            format!("avocado-sdk-{}-{}", target, version)
        } else {
            format!("avocado-sdk-{}", target)
        };

        let sdk_target_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_HOST_REPO_CONF \
    -y \
    install \
    {}
"#,
            sdk_target_pkg
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: sdk_target_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let sdk_target_success = container_helper.run_in_container(run_config).await?;

        if sdk_target_success {
            print_success(
                &format!("Installed SDK for target '{}'.", target),
                OutputLevel::Normal,
            );
        } else {
            return Err(anyhow::anyhow!(
                "Failed to install SDK for target '{}'.",
                target
            ));
        }

        // Run check-update to refresh metadata
        let check_update_command = r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    check-update || true
"#;

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: check_update_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        container_helper.run_in_container(run_config).await?;

        // Install avocado-sdk-bootstrap with version from distro.version
        print_info("Installing SDK bootstrap.", OutputLevel::Normal);

        let bootstrap_pkg = if let Some(version) = config.get_distro_version() {
            format!("avocado-sdk-bootstrap-{}", version)
        } else {
            "avocado-sdk-bootstrap".to_string()
        };

        let bootstrap_command = format!(
            r#"
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST $DNF_NO_SCRIPTS \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
    -y \
    install \
    {}
"#,
            bootstrap_pkg
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: bootstrap_command,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let bootstrap_success = container_helper.run_in_container(run_config).await?;

        if bootstrap_success {
            print_success("Installed SDK bootstrap.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to install SDK bootstrap."));
        }

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
            target: target.clone(),
            command: env_setup_command.to_string(),
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        container_helper.run_in_container(run_config).await?;

        // Install SDK dependencies (into SDK)
        let mut sdk_packages = Vec::new();

        // Add regular SDK dependencies
        if let Some(ref dependencies) = sdk_dependencies {
            sdk_packages.extend(self.build_package_list(dependencies));
        }

        // Add extension SDK dependencies to the package list
        for (ext_name, ext_deps) in &extension_sdk_dependencies {
            if self.verbose {
                print_info(
                    &format!("Adding SDK dependencies from extension '{ext_name}'"),
                    OutputLevel::Normal,
                );
            }
            let ext_packages = self.build_package_list(ext_deps);
            sdk_packages.extend(ext_packages);
        }

        if !sdk_packages.is_empty() {
            let yes = if self.force { "-y" } else { "" };
            let dnf_args_str = if let Some(args) = &self.dnf_args {
                format!(" {} ", args.join(" "))
            } else {
                String::new()
            };

            let command = format!(
                r#"
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_REPO_CONF \
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
                target: target.clone(),
                command,
                verbose: self.verbose,
                source_environment: true,
                interactive: !self.force,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };
            let install_success = container_helper.run_in_container(run_config).await?;

            if install_success {
                print_success("Installed SDK dependencies.", OutputLevel::Normal);
            } else {
                return Err(anyhow::anyhow!("Failed to install SDK package(s)."));
            }
        } else {
            print_success("No dependencies configured.", OutputLevel::Normal);
        }

        // Install rootfs sysroot with version from distro.version
        print_info("Installing rootfs sysroot.", OutputLevel::Normal);

        let rootfs_pkg = if let Some(version) = config.get_distro_version() {
            format!("avocado-pkg-rootfs-{}", version)
        } else {
            "avocado-pkg-rootfs".to_string()
        };

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
    {} {} --installroot $AVOCADO_PREFIX/rootfs install {}
"#,
            dnf_args_str, yes, rootfs_pkg
        );

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.clone(),
            command: rootfs_command,
            verbose: self.verbose,
            source_environment: false,
            interactive: !self.force,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
            ..Default::default()
        };

        let rootfs_success = container_helper.run_in_container(run_config).await?;

        if rootfs_success {
            print_success("Installed rootfs sysroot.", OutputLevel::Normal);
        } else {
            return Err(anyhow::anyhow!("Failed to install rootfs sysroot."));
        }

        // Install target-sysroot if there are any sdk.compile dependencies
        // This aggregates all dependencies from all compile sections (main config + external extensions)
        let compile_dependencies = config.get_compile_dependencies();
        if !compile_dependencies.is_empty() {
            // Aggregate all compile dependencies into a single list
            let mut all_compile_packages: Vec<String> = Vec::new();
            for dependencies in compile_dependencies.values() {
                let packages = self.build_package_list(dependencies);
                all_compile_packages.extend(packages);
            }

            // Deduplicate packages
            all_compile_packages.sort();
            all_compile_packages.dedup();

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

            // Build the target-sysroot package spec with version from distro.version
            let target_sysroot_pkg = if let Some(version) = config.get_distro_version() {
                format!("avocado-sdk-target-sysroot-{}", version)
            } else {
                "avocado-sdk-target-sysroot".to_string()
            };

            // Install the target-sysroot with avocado-sdk-target-sysroot plus compile deps
            let command = format!(
                r#"
unset RPM_CONFIGDIR
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_TARGET_REPO_CONF \
    --disablerepo=${{AVOCADO_TARGET}}-target-ext \
    {} {} --installroot ${{AVOCADO_SDK_PREFIX}}/target-sysroot \
    install {} {}
"#,
                dnf_args_str,
                yes,
                target_sysroot_pkg,
                all_compile_packages.join(" ")
            );

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command,
                verbose: self.verbose,
                source_environment: true,
                interactive: !self.force,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };

            let install_success = container_helper.run_in_container(run_config).await?;

            if install_success {
                print_success(
                    "Installed target-sysroot with compile dependencies.",
                    OutputLevel::Normal,
                );
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to install target-sysroot with compile dependencies."
                ));
            }
        }

        // Write SDK install stamp (unless --no-stamps)
        if !self.no_stamps {
            let inputs = compute_sdk_input_hash(&composed.merged_value)?;
            let outputs = StampOutputs::default();
            let stamp = Stamp::sdk_install(&target, inputs, outputs);
            let stamp_script = generate_write_stamp_script(&stamp)?;

            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target.clone(),
                command: stamp_script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                repo_url: repo_url.clone(),
                repo_release: repo_release.clone(),
                container_args: merged_container_args.clone(),
                dnf_args: self.dnf_args.clone(),
                disable_weak_dependencies: config.get_sdk_disable_weak_dependencies(),
                ..Default::default()
            };

            container_helper.run_in_container(run_config).await?;

            if self.verbose {
                print_info("Wrote SDK install stamp.", OutputLevel::Normal);
            }
        }

        Ok(())
    }

    /// Build a list of packages from dependencies HashMap
    fn build_package_list(&self, dependencies: &HashMap<String, serde_yaml::Value>) -> Vec<String> {
        let mut packages = Vec::new();

        for (package_name, version) in dependencies {
            match version {
                serde_yaml::Value::String(v) if v == "*" => {
                    packages.push(package_name.clone());
                }
                serde_yaml::Value::String(v) => {
                    packages.push(format!("{package_name}-{v}"));
                }
                serde_yaml::Value::Mapping(_) => {
                    // Handle dictionary version format like {'core2_64': '*'}
                    packages.push(package_name.clone());
                }
                _ => {
                    packages.push(package_name.clone());
                }
            }
        }

        packages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;
    use std::collections::HashMap;

    #[test]
    fn test_build_package_list() {
        let cmd = SdkInstallCommand::new("test.yaml".to_string(), false, false, None, None, None);

        let mut deps = HashMap::new();
        deps.insert("package1".to_string(), Value::String("*".to_string()));
        deps.insert("package2".to_string(), Value::String("1.0.0".to_string()));
        deps.insert(
            "package3".to_string(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );

        let packages = cmd.build_package_list(&deps);

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"package1".to_string()));
        assert!(packages.contains(&"package2-1.0.0".to_string()));
        assert!(packages.contains(&"package3".to_string()));
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
