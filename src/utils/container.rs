//! Container utilities for SDK operations.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_error, print_info, OutputLevel};
use crate::utils::volume::{VolumeManager, VolumeState};

/// Configuration for running commands in containers
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub container_image: String,
    pub target: String,
    pub command: String,
    pub container_name: Option<String>,
    pub detach: bool,
    pub rm: bool,
    pub env_vars: Option<HashMap<String, String>>,
    pub verbose: bool,
    pub source_environment: bool,
    pub use_entrypoint: bool,
    pub interactive: bool,
    pub repo_url: Option<String>,
    pub repo_release: Option<String>,
    pub container_args: Option<Vec<String>>,
    pub dnf_args: Option<Vec<String>>,
    pub extension_sysroot: Option<String>,
    pub runtime_sysroot: Option<String>,
    pub no_bootstrap: bool,
    pub disable_weak_dependencies: bool,
    pub signing_socket_path: Option<PathBuf>,
    pub signing_helper_script_path: Option<PathBuf>,
    pub signing_key_name: Option<String>,
    pub signing_checksum_algorithm: Option<String>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            container_image: String::new(),
            target: String::new(),
            command: String::new(),
            container_name: None,
            detach: false,
            rm: true,
            env_vars: None,
            verbose: false,
            source_environment: true,
            use_entrypoint: true,
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: None,
            dnf_args: None,
            extension_sysroot: None,
            runtime_sysroot: None,
            no_bootstrap: false,
            disable_weak_dependencies: false,
            signing_socket_path: None,
            signing_helper_script_path: None,
            signing_key_name: None,
            signing_checksum_algorithm: None,
        }
    }
}

/// Container helper for SDK operations
pub struct SdkContainer {
    pub container_tool: String,
    pub cwd: PathBuf,
    pub src_dir: Option<PathBuf>,
    pub verbose: bool,
}

impl Default for SdkContainer {
    fn default() -> Self {
        Self::new()
    }
}

impl SdkContainer {
    /// Create a new SdkContainer instance
    pub fn new() -> Self {
        Self {
            container_tool: "docker".to_string(),
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            src_dir: None,
            verbose: false,
        }
    }

    /// Create a new SdkContainer with custom container tool
    #[allow(dead_code)]
    pub fn with_tool(container_tool: String) -> Self {
        Self {
            container_tool,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            src_dir: None,
            verbose: false,
        }
    }

    /// Set verbose mode
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set custom source directory for mounting
    pub fn with_src_dir(mut self, src_dir: Option<PathBuf>) -> Self {
        self.src_dir = src_dir;
        self
    }

    /// Create a new SdkContainer with configuration from config file
    pub fn from_config(config_path: &str, config: &crate::utils::config::Config) -> Result<Self> {
        let src_dir = config.get_resolved_src_dir(config_path);
        Ok(Self::new().with_src_dir(src_dir))
    }

    /// Run a command in the container
    pub async fn run_in_container(&self, config: RunConfig) -> Result<bool> {
        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        // Build environment variables
        let mut env_vars = config.env_vars.clone().unwrap_or_default();

        // Set host platform environment variable
        let host_platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        env_vars.insert(
            "AVOCADO_HOST_PLATFORM".to_string(),
            host_platform.to_string(),
        );

        if let Some(url) = &config.repo_url {
            env_vars.insert("AVOCADO_SDK_REPO_URL".to_string(), url.clone());
        }
        if let Some(release) = &config.repo_release {
            env_vars.insert("AVOCADO_SDK_REPO_RELEASE".to_string(), release.clone());
        }
        if let Some(dnf_args) = &config.dnf_args {
            env_vars.insert("AVOCADO_DNF_ARGS".to_string(), dnf_args.join(" "));
        }
        if config.verbose || self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Build the complete command
        let mut full_command = String::new();

        // Conditionally include the entrypoint script
        if config.use_entrypoint {
            full_command.push_str(&self.create_entrypoint_script(
                config.source_environment,
                config.extension_sysroot.as_deref(),
                config.runtime_sysroot.as_deref(),
                &config.target,
                config.no_bootstrap,
                config.disable_weak_dependencies,
            ));
            full_command.push('\n');
        }

        full_command.push_str(&config.command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command with volume state
        let container_cmd =
            self.build_container_command(&config, &bash_cmd, &env_vars, &volume_state)?;

        // Execute the command
        self.execute_container_command(
            &container_cmd,
            config.detach,
            config.verbose || self.verbose,
        )
        .await
    }

    /// Build the complete container command
    fn build_container_command(
        &self,
        config: &RunConfig,
        command: &[String],
        env_vars: &HashMap<String, String>,
        volume_state: &VolumeState,
    ) -> Result<Vec<String>> {
        let mut container_cmd = vec![self.container_tool.clone(), "run".to_string()];

        // Container options
        if config.rm {
            container_cmd.push("--rm".to_string());
        }
        if let Some(name) = &config.container_name {
            container_cmd.push("--name".to_string());
            container_cmd.push(name.to_string());
        }
        if config.detach {
            container_cmd.push("-d".to_string());
        }
        if config.interactive {
            container_cmd.push("-i".to_string());
            container_cmd.push("-t".to_string());
        }

        // Volume mounts: docker volume for persistent state, bind mount for source
        container_cmd.push("-v".to_string());
        let src_path = self.src_dir.as_ref().unwrap_or(&self.cwd);
        container_cmd.push(format!("{}:/opt/src:rw", src_path.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Mount signing socket directory if provided
        if let Some(socket_path) = &config.signing_socket_path {
            if let Some(socket_dir) = socket_path.parent() {
                container_cmd.push("-v".to_string());
                container_cmd.push(format!("{}:/run/avocado:rw", socket_dir.display()));
            }
        }

        // Mount signing helper script if provided
        if let Some(helper_script_path) = &config.signing_helper_script_path {
            container_cmd.push("-v".to_string());
            container_cmd.push(format!(
                "{}:/usr/local/bin/avocado-sign-request:ro",
                helper_script_path.display()
            ));
        }

        // Mount signing keys directory if it exists (read-only for security)
        let signing_keys_env =
            if let Ok(signing_keys_dir) = crate::utils::signing_keys::get_signing_keys_dir() {
                if signing_keys_dir.exists() {
                    container_cmd.push("-v".to_string());
                    container_cmd.push(format!(
                        "{}:/opt/signing-keys:ro",
                        signing_keys_dir.display()
                    ));
                    // Return environment variable so container knows where keys are mounted
                    Some("/opt/signing-keys".to_string())
                } else {
                    None
                }
            } else {
                None
            };

        // Note: Working directory is handled in the entrypoint script based on sysroot parameters

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_TARGET={}", config.target));
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={}", config.target));

        // Add signing-related environment variables
        if config.signing_socket_path.is_some() {
            container_cmd.push("-e".to_string());
            container_cmd.push("AVOCADO_SIGNING_SOCKET=/run/avocado/sign.sock".to_string());
            container_cmd.push("-e".to_string());
            container_cmd.push("AVOCADO_SIGNING_ENABLED=1".to_string());
        }

        if let Some(key_name) = &config.signing_key_name {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_KEY_NAME={}", key_name));
        }

        if let Some(checksum_algo) = &config.signing_checksum_algorithm {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_CHECKSUM={}", checksum_algo));
        }

        // Add signing keys directory env var if mounted
        if let Some(keys_dir) = signing_keys_env {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("AVOCADO_SIGNING_KEYS_DIR={}", keys_dir));
        }

        for (key, value) in env_vars {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("{key}={value}"));
        }

        // Add additional container arguments if provided
        if let Some(args) = &config.container_args {
            for arg in args {
                container_cmd.extend(Self::parse_container_arg(arg));
            }
        }

        // Add the container image
        container_cmd.push(config.container_image.to_string());

        // Add the command to execute
        container_cmd.extend(command.iter().cloned());

        Ok(container_cmd)
    }

    /// Run a command in the container and capture its output
    pub async fn run_in_container_with_output(&self, config: RunConfig) -> Result<Option<String>> {
        // Get or create docker volume for persistent state
        let volume_manager = VolumeManager::new(self.container_tool.clone(), self.verbose);
        let volume_state = volume_manager.get_or_create_volume(&self.cwd).await?;

        // Build environment variables
        let mut env_vars = config.env_vars.clone().unwrap_or_default();

        // Set host platform environment variable
        let host_platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        env_vars.insert(
            "AVOCADO_HOST_PLATFORM".to_string(),
            host_platform.to_string(),
        );

        if let Some(url) = &config.repo_url {
            env_vars.insert("AVOCADO_SDK_REPO_URL".to_string(), url.clone());
        }
        if let Some(release) = &config.repo_release {
            env_vars.insert("AVOCADO_SDK_REPO_RELEASE".to_string(), release.clone());
        }
        if let Some(dnf_args) = &config.dnf_args {
            env_vars.insert("AVOCADO_DNF_ARGS".to_string(), dnf_args.join(" "));
        }
        if config.verbose || self.verbose {
            env_vars.insert("AVOCADO_VERBOSE".to_string(), "1".to_string());
        }

        // Build the complete command
        let mut full_command = String::new();

        // Conditionally include the entrypoint script
        if config.use_entrypoint {
            full_command.push_str(&self.create_entrypoint_script(
                config.source_environment,
                config.extension_sysroot.as_deref(),
                config.runtime_sysroot.as_deref(),
                &config.target,
                config.no_bootstrap,
                config.disable_weak_dependencies,
            ));
            full_command.push('\n');
        }

        full_command.push_str(&config.command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command with volume state
        let container_cmd =
            self.build_container_command(&config, &bash_cmd, &env_vars, &volume_state)?;

        if config.verbose || self.verbose {
            print_info(
                &format!(
                    "Mounting source directory: {} -> /opt/src",
                    self.cwd.display()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Container command: {}", container_cmd.join(" ")),
                OutputLevel::Normal,
            );
        }

        // Execute command and capture output
        let mut cmd = AsyncCommand::new(&container_cmd[0]);
        cmd.args(&container_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .with_context(|| "Failed to execute container command")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(Some(stdout))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if config.verbose || self.verbose {
                print_error(
                    &format!("Container execution failed: {stderr}"),
                    OutputLevel::Normal,
                );
            }
            Ok(None)
        }
    }

    /// Execute the container command
    async fn execute_container_command(
        &self,
        container_cmd: &[String],
        detach: bool,
        verbose: bool,
    ) -> Result<bool> {
        if verbose {
            print_info(
                &format!(
                    "Mounting source directory: {} -> /opt/src",
                    self.cwd.display()
                ),
                OutputLevel::Normal,
            );
            print_info(
                &format!("Container command: {}", container_cmd.join(" ")),
                OutputLevel::Normal,
            );
        }

        let mut cmd = AsyncCommand::new(&container_cmd[0]);
        cmd.args(&container_cmd[1..]);

        if detach {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let output = cmd
                .output()
                .await
                .with_context(|| "Failed to execute container command")?;

            if output.status.success() {
                let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                print_info(
                    &format!("Container started in detached mode with ID: {container_id}"),
                    OutputLevel::Normal,
                );
                Ok(true)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                print_error(
                    &format!("Container execution failed: {stderr}"),
                    OutputLevel::Normal,
                );
                Ok(false)
            }
        } else {
            // In non-detached mode, we need to capture output to ensure stderr is visible
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            let status = cmd
                .status()
                .await
                .with_context(|| "Failed to execute container command")?;
            Ok(status.success())
        }
    }

    /// Create the entrypoint script for SDK initialization
    pub fn create_entrypoint_script(
        &self,
        source_environment: bool,
        extension_sysroot: Option<&str>,
        runtime_sysroot: Option<&str>,
        target: &str,
        no_bootstrap: bool,
        disable_weak_dependencies: bool,
    ) -> String {
        // Conditionally add install_weak_deps flag
        let weak_deps_flag = if disable_weak_dependencies {
            "--setopt=install_weak_deps=0 \\\n"
        } else {
            ""
        };

        let mut script = format!(
            r#"
set -e

# Get repo url from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_URL" ]; then
    REPO_URL="$AVOCADO_SDK_REPO_URL"
else
    REPO_URL="https://repo.avocadolinux.org"
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo URL: '$REPO_URL'"; fi

# Get repo release from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_RELEASE" ]; then
    REPO_RELEASE="$AVOCADO_SDK_REPO_RELEASE"
else
    REPO_RELEASE="https://repo.avocadolinux.org"

    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        REPO_RELEASE=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    REPO_RELEASE=${{REPO_RELEASE:-dev}}
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo release: '$REPO_RELEASE'"; fi

export AVOCADO_PREFIX="/opt/_avocado/${{AVOCADO_TARGET}}"
export AVOCADO_SDK_PREFIX="${{AVOCADO_PREFIX}}/sdk"
export AVOCADO_EXT_SYSROOTS="${{AVOCADO_PREFIX}}/extensions"
export DNF_SDK_HOST_PREFIX="${{AVOCADO_SDK_PREFIX}}"
export DNF_SDK_TARGET_PREFIX="${{AVOCADO_SDK_PREFIX}}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
{weak_deps_flag}--setopt=check_config_file_age=0 \
${{AVOCADO_DNF_ARGS:-}} \
"

export DNF_NO_SCRIPTS="--setopt=tsflags=noscripts"
export SSL_CERT_FILE=${{AVOCADO_SDK_PREFIX}}/etc/ssl/certs/ca-certificates.crt

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${{DNF_SDK_HOST_PREFIX}}/var/cache \
--setopt=logdir=${{DNF_SDK_HOST_PREFIX}}/var/log \
--setopt=persistdir=${{DNF_SDK_HOST_PREFIX}}/var/lib/dnf \
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_HOST_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars \
--setopt=reposdir=${{DNF_SDK_TARGET_PREFIX}}/etc/yum.repos.d \
"

mkdir -p /etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/etc/dnf/vars
mkdir -p ${{AVOCADO_SDK_PREFIX}}/target-repoconf/etc/dnf/vars

echo "${{REPO_URL}}" > /etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars/repo_url
echo "${{REPO_URL}}" > ${{DNF_SDK_TARGET_PREFIX}}/etc/dnf/vars/repo_url
"#
        );

        // Only include bootstrap logic if no_bootstrap is false
        if !no_bootstrap {
            script.push_str(r#"
if [ ! -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
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

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF -y install "avocado-sdk-$AVOCADO_TARGET"

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF check-update

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF -y install avocado-sdk-bootstrap

    echo "[INFO] Installing rootfs sysroot."
    RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
      $DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_TARGET_REPO_CONF \
      -y --installroot $AVOCADO_PREFIX/rootfs install avocado-pkg-rootfs

    echo "[INFO] Installing SDK target sysroot."
    RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
    $DNF_SDK_HOST $DNF_NO_SCRIPTS \
        $DNF_SDK_TARGET_REPO_CONF \
        -y \
        --installroot ${AVOCADO_SDK_PREFIX}/target-sysroot \
        install \
        packagegroup-core-standalone-sdk-target
fi
"#);
        }

        script.push_str(
            r#"
export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

"#,
        );

        // Conditionally change to sysroot directory or default to /opt/src
        if let Some(extension_name) = extension_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/extensions/{extension_name}\n"
            ));
        } else if let Some(runtime_name) = runtime_sysroot {
            script.push_str(&format!(
                "cd /opt/_avocado/{target}/runtimes/{runtime_name}\n"
            ));
        } else {
            script.push_str("cd /opt/src\n");
        }

        // Conditionally add environment sourcing based on the source_environment parameter
        if source_environment {
            script.push_str(
                r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi

# Add SSL certificate path to DNF options and CURL if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt" ]; then
    export DNF_SDK_HOST_OPTS="${DNF_SDK_HOST_OPTS} \
      --setopt=sslcacert=${SSL_CERT_FILE} \
"

    export CURL_CA_BUNDLE=${AVOCADO_SDK_PREFIX}/etc/ssl/certs/ca-certificates.crt
fi
"#,
            );
        }

        script
    }

    /// Parse a container argument, splitting on spaces while respecting quotes
    fn parse_container_arg(arg: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let chars = arg.chars().peekable();

        for ch in chars {
            match ch {
                '"' => {
                    in_quotes = !in_quotes;
                }
                ' ' if !in_quotes => {
                    if !current.is_empty() {
                        result.push(current.trim().to_string());
                        current.clear();
                    }
                }
                _ => {
                    current.push(ch);
                }
            }
        }

        if !current.is_empty() {
            result.push(current.trim().to_string());
        }

        // If no spaces were found and no quotes, or if result is empty, return the original string
        if (result.len() == 1 && result[0] == arg) || result.is_empty() {
            vec![arg.to_string()]
        } else {
            result
        }
    }

    /// Write signature files to a Docker volume using docker cp
    ///
    /// This creates a temporary container, copies signature files into it,
    /// then removes the container.
    pub async fn write_signatures_to_volume(
        &self,
        volume_name: &str,
        signatures: &[crate::utils::image_signing::SignatureData],
    ) -> Result<()> {
        if signatures.is_empty() {
            return Ok(());
        }

        // Create temporary directory for signature files
        let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

        // Write signature files to temp directory with flattened names
        let mut file_mappings = Vec::new();
        for (idx, sig) in signatures.iter().enumerate() {
            let temp_file_name = format!("sig_{}.json", idx);
            let temp_file_path = temp_dir.path().join(&temp_file_name);
            std::fs::write(&temp_file_path, &sig.content).with_context(|| {
                format!(
                    "Failed to write signature file to temp: {}",
                    temp_file_path.display()
                )
            })?;

            file_mappings.push((temp_file_path, sig.container_path.clone()));
        }

        // Create a temporary container with the volume mounted
        let container_name = format!("avocado-sig-writer-{}", uuid::Uuid::new_v4());
        let volume_mount = format!("{}:/opt/_avocado:rw", volume_name);

        let create_cmd = [
            &self.container_tool,
            &"create".to_string(),
            &"--name".to_string(),
            &container_name,
            &"-v".to_string(),
            &volume_mount,
            &"alpine:latest".to_string(),
            &"true".to_string(),
        ];

        if self.verbose {
            print_info(
                &format!(
                    "Creating temporary container for signature writing: {}",
                    container_name
                ),
                OutputLevel::Verbose,
            );
        }

        let mut cmd = AsyncCommand::new(create_cmd[0]);
        cmd.args(&create_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .context("Failed to create temporary container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create temporary container: {}", stderr);
        }

        // Copy each signature file into the container
        for (temp_path, container_path) in &file_mappings {
            let temp_path_str = temp_path.display().to_string();
            let container_dest = format!("{}:{}", container_name, container_path);

            let cp_cmd = [
                &self.container_tool,
                &"cp".to_string(),
                &temp_path_str,
                &container_dest,
            ];

            if self.verbose {
                print_info(
                    &format!("Copying signature to {}", container_path),
                    OutputLevel::Verbose,
                );
            }

            let mut cmd = AsyncCommand::new(cp_cmd[0]);
            cmd.args(&cp_cmd[1..]);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            let output = cmd.output().await.with_context(|| {
                format!(
                    "Failed to copy signature file to container: {}",
                    container_path
                )
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);

                // Clean up container before returning error
                let _ = self.remove_container(&container_name).await;

                anyhow::bail!(
                    "Failed to copy signature file {}: {}",
                    container_path,
                    stderr
                );
            }
        }

        // Remove the temporary container
        self.remove_container(&container_name).await?;

        if self.verbose {
            print_info(
                &format!(
                    "Successfully wrote {} signature file(s) to volume",
                    signatures.len()
                ),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    /// Remove a container by name
    async fn remove_container(&self, container_name: &str) -> Result<()> {
        let container_name_str = container_name.to_string();
        let rm_cmd = [
            &self.container_tool,
            &"rm".to_string(),
            &"-f".to_string(),
            &container_name_str,
        ];

        let mut cmd = AsyncCommand::new(rm_cmd[0]);
        cmd.args(&rm_cmd[1..]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .context("Failed to remove temporary container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.verbose {
                print_error(
                    &format!(
                        "Warning: Failed to remove temporary container {}: {}",
                        container_name, stderr
                    ),
                    OutputLevel::Verbose,
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sdk_container_creation() {
        let container = SdkContainer::new();
        assert_eq!(container.container_tool, "docker");
        assert!(!container.verbose);
    }

    #[test]
    fn test_sdk_container_with_tool() {
        let container = SdkContainer::with_tool("podman".to_string());
        assert_eq!(container.container_tool, "podman");
    }

    #[test]
    fn test_sdk_container_verbose() {
        let container = SdkContainer::new().verbose(true);
        assert!(container.verbose);
    }

    #[test]
    fn test_build_container_command() {
        use crate::utils::volume::VolumeState;
        let container = SdkContainer::new();
        let command = vec!["echo".to_string(), "test".to_string()];
        let env_vars = HashMap::new();
        let volume_state = VolumeState::new(std::env::current_dir().unwrap(), "docker".to_string());

        let config = RunConfig {
            container_image: "test-image".to_string(),
            target: "test-target".to_string(),
            command: "".to_string(),
            container_name: None,
            detach: false,
            rm: true,
            env_vars: None,
            verbose: false,
            source_environment: false,
            use_entrypoint: false,
            interactive: false,
            repo_url: None,
            repo_release: None,
            container_args: None,
            dnf_args: None,
            extension_sysroot: None,
            runtime_sysroot: None,
            no_bootstrap: false,
            disable_weak_dependencies: false,
            signing_socket_path: None,
            signing_helper_script_path: None,
            signing_key_name: None,
            signing_checksum_algorithm: None,
        };

        let result = container.build_container_command(&config, &command, &env_vars, &volume_state);

        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.contains(&"docker".to_string()));
        assert!(cmd.contains(&"run".to_string()));
        assert!(cmd.contains(&"--rm".to_string()));
        assert!(cmd.contains(&"test-image".to_string()));
        assert!(cmd.contains(&"echo".to_string()));
        assert!(cmd.contains(&"test".to_string()));
    }

    #[test]
    fn test_entrypoint_script() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, None, None, "x86_64", false, false);
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));
        assert!(script.contains("environment-setup"));
        assert!(script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_extension_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(
            true,
            Some("test-ext"),
            None,
            "x86_64",
            false,
            false,
        );
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/extensions/test-ext"));
        assert!(!script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_runtime_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(
            true,
            None,
            Some("test-runtime"),
            "x86_64",
            false,
            false,
        );
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/runtimes/test-runtime"));
        assert!(!script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_no_bootstrap() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, None, None, "x86_64", true, false);

        // Should still contain environment variables
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));

        // Should NOT contain bootstrap initialization
        assert!(!script.contains("Initializing Avocado SDK"));
        assert!(!script.contains("install \"avocado-sdk-"));
        assert!(!script.contains("install avocado-sdk-toolchain"));
        assert!(!script.contains("Installing rootfs sysroot"));

        // Should still change to /opt/src
        assert!(script.contains("cd /opt/src"));

        // Should still contain environment sourcing (this is separate from bootstrap)
        assert!(script.contains("source \"${AVOCADO_SDK_PREFIX}/environment-setup\""));
    }

    #[test]
    fn test_parse_container_arg_single() {
        let result = SdkContainer::parse_container_arg("--rm");
        assert_eq!(result, vec!["--rm"]);
    }

    #[test]
    fn test_parse_container_arg_with_spaces() {
        let result = SdkContainer::parse_container_arg("-v /host:/container");
        assert_eq!(result, vec!["-v", "/host:/container"]);
    }

    #[test]
    fn test_parse_container_arg_with_quotes() {
        let result = SdkContainer::parse_container_arg("-v \"/path with spaces:/container\"");
        assert_eq!(result, vec!["-v", "/path with spaces:/container"]);
    }

    #[test]
    fn test_parse_container_arg_complex() {
        let result = SdkContainer::parse_container_arg("-e \"VAR=value with spaces\" --name test");
        assert_eq!(
            result,
            vec!["-e", "VAR=value with spaces", "--name", "test"]
        );
    }
}
