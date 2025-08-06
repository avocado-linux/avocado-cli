//! Container utilities for SDK operations.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command as AsyncCommand;

use crate::utils::output::{print_error, print_info, OutputLevel};

/// Container helper for SDK operations
pub struct SdkContainer {
    pub container_tool: String,
    pub cwd: PathBuf,
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
            verbose: false,
        }
    }

    /// Create a new SdkContainer with custom container tool
    #[allow(dead_code)]
    pub fn with_tool(container_tool: String) -> Self {
        Self {
            container_tool,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            verbose: false,
        }
    }

    /// Set verbose mode
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Run a command in the container
    pub async fn run_in_container(
        &self,
        container_image: &str,
        target: &str,
        command: &str,
        verbose: bool,
        source_environment: bool,
        interactive: bool,
    ) -> Result<bool> {
        // Create _avocado directory
        let avocado_dir = self.cwd.join("_avocado");
        fs::create_dir_all(&avocado_dir).with_context(|| "Failed to create _avocado directory")?;

        // Build the complete command
        let mut full_command = String::new();

        // Always include the entrypoint script for environment setup, but conditionally source the environment-setup file
        full_command.push_str(&self.create_entrypoint_script(source_environment));
        full_command.push('\n');

        full_command.push_str(command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command
        let container_cmd = self.build_container_command(
            container_image,
            &bash_cmd,
            target,
            &HashMap::new(),
            None,
            false,
            true,
            interactive,
        )?;

        // Execute the command
        self.execute_container_command(&container_cmd, false, verbose || self.verbose)
            .await
    }

    /// Build the complete container command
    #[allow(clippy::too_many_arguments)]
    fn build_container_command(
        &self,
        container_image: &str,
        command: &[String],
        target: &str,
        env_vars: &HashMap<String, String>,
        container_name: Option<&str>,
        detach: bool,
        rm: bool,
        interactive: bool,
    ) -> Result<Vec<String>> {
        let mut container_cmd = vec![self.container_tool.clone(), "run".to_string()];

        // Container options
        if rm {
            container_cmd.push("--rm".to_string());
        }
        if let Some(name) = container_name {
            container_cmd.push("--name".to_string());
            container_cmd.push(name.to_string());
        }
        if detach {
            container_cmd.push("-d".to_string());
        }
        if interactive {
            container_cmd.push("-i".to_string());
            container_cmd.push("-t".to_string());
        }

        // Default volume mounts
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado/src:ro", self.cwd.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}/_avocado:/opt/_avocado:rw", self.cwd.display()));

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_SDK_TARGET={target}"));

        for (key, value) in env_vars {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("{key}={value}"));
        }

        // Add the container image
        container_cmd.push(container_image.to_string());

        // Add the command to execute
        container_cmd.extend(command.iter().cloned());

        Ok(container_cmd)
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
                &format!("Mounting host directory: {} -> /opt", self.cwd.display()),
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
            let status = cmd
                .status()
                .await
                .with_context(|| "Failed to execute container command")?;
            Ok(status.success())
        }
    }

    /// Create the entrypoint script for SDK initialization
    pub fn create_entrypoint_script(&self, source_environment: bool) -> String {
        let mut script = r#"
set -e

# Get codename from environment or os-release
if [ -n "$AVOCADO_SDK_CODENAME" ]; then
    CODENAME="$AVOCADO_SDK_CODENAME"
else
    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        CODENAME=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    CODENAME=${CODENAME:-dev}
fi

export AVOCADO_PREFIX="/opt/_avocado/${AVOCADO_SDK_TARGET}"
export AVOCADO_SDK_PREFIX="${AVOCADO_PREFIX}/sdk"
export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/extensions"
export DNF_SDK_HOST_PREFIX="${AVOCADO_SDK_PREFIX}"
export DNF_SDK_TARGET_PREFIX="${AVOCADO_SDK_PREFIX}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$CODENAME" \
--best \
--setopt=tsflags=noscripts \
"

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${DNF_SDK_HOST_PREFIX}/var/cache \
--setopt=logdir=${DNF_SDK_HOST_PREFIX}/var/log \
--setopt=persistdir=${DNF_SDK_HOST_PREFIX}/var/lib/dnf
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export RPM_NO_CHROOT_FOR_SCRIPTS=1

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


    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF -y install "avocado-sdk-$AVOCADO_SDK_TARGET"

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF check-update

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF -y install avocado-sdk-toolchain

    echo "[INFO] Installing rootfs sysroot."
    RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
      $DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF \
      -y --installroot $AVOCADO_PREFIX/rootfs install avocado-pkg-rootfs

    echo "[INFO] Installing SDK target sysroot."
    RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
    $DNF_SDK_HOST \
        $DNF_SDK_TARGET_REPO_CONF \
        -y \
        --installroot ${AVOCADO_SDK_PREFIX}/target-sysroot \
        install \
        packagegroup-core-standalone-sdk-target
fi

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

cd /opt/_avocado/src
"#.to_string();

        // Conditionally add environment sourcing based on the source_environment parameter
        if source_environment {
            script.push_str(
                r#"
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi
"#,
            );
        }

        script
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
        let container = SdkContainer::new();
        let command = vec!["echo".to_string(), "test".to_string()];
        let env_vars = HashMap::new();

        let result = container.build_container_command(
            "test-image",
            &command,
            "test-target",
            &env_vars,
            None,
            false,
            true,
            false,
        );

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
        let script = container.create_entrypoint_script(true);
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));
        assert!(script.contains("environment-setup"));
    }
}
