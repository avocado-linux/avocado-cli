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
        let mut env_vars = config.env_vars.unwrap_or_default();

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
            ));
            full_command.push('\n');
        }

        full_command.push_str(&config.command);

        let bash_cmd = vec!["bash".to_string(), "-c".to_string(), full_command];

        // Build container command with volume state
        let container_cmd = self.build_container_command(
            &config.container_image,
            &bash_cmd,
            &config.target,
            &env_vars,
            config.container_name.as_deref(),
            config.detach,
            config.rm,
            config.interactive,
            config.container_args.as_deref(),
            &volume_state,
        )?;

        // Execute the command
        self.execute_container_command(
            &container_cmd,
            config.detach,
            config.verbose || self.verbose,
        )
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
        container_args: Option<&[String]>,
        volume_state: &VolumeState,
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

        // Volume mounts: docker volume for persistent state, bind mount for source
        container_cmd.push("-v".to_string());
        let src_path = self.src_dir.as_ref().unwrap_or(&self.cwd);
        container_cmd.push(format!("{}:/opt/src:rw", src_path.display()));
        container_cmd.push("-v".to_string());
        container_cmd.push(format!("{}:/opt/_avocado:rw", volume_state.volume_name));

        // Note: Working directory is handled in the entrypoint script based on sysroot parameters

        // Add environment variables
        container_cmd.push("-e".to_string());
        container_cmd.push(format!("AVOCADO_TARGET={target}"));

        for (key, value) in env_vars {
            container_cmd.push("-e".to_string());
            container_cmd.push(format!("{key}={value}"));
        }

        // Add additional container arguments if provided
        if let Some(args) = container_args {
            for arg in args {
                container_cmd.extend(Self::parse_container_arg(arg));
            }
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
    ) -> String {
        let mut script = r#"
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
    REPO_RELEASE=${REPO_RELEASE:-dev}
fi

if [ -n "$AVOCADO_VERBOSE" ]; then echo "[INFO] Using repo release: '$REPO_RELEASE'"; fi

export AVOCADO_PREFIX="/opt/_avocado/${AVOCADO_TARGET}"
export AVOCADO_SDK_PREFIX="${AVOCADO_PREFIX}/sdk"
export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/extensions"
export DNF_SDK_HOST_PREFIX="${AVOCADO_SDK_PREFIX}"
export DNF_SDK_TARGET_PREFIX="${AVOCADO_SDK_PREFIX}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
${AVOCADO_DNF_ARGS:-} \
"

export DNF_NO_SCRIPTS="--setopt=tsflags=noscripts"

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

# TODO Checking
# export RPM_NO_CHROOT_FOR_SCRIPTS=1

mkdir -p /etc/dnf/vars
mkdir -p ${AVOCADO_SDK_PREFIX}/etc/dnf/vars
mkdir -p ${AVOCADO_SDK_PREFIX}/target-repoconf/etc/dnf/vars

echo "${REPO_URL}" > /etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_HOST_PREFIX}/etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars/repo_url

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
        $DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF -y install "avocado-sdk-$AVOCADO_TARGET"

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF check-update

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_NO_SCRIPTS $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF -y install avocado-sdk-toolchain

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

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

"#.to_string();

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

        let result = container.build_container_command(
            "test-image",
            &command,
            "test-target",
            &env_vars,
            None,
            false,
            true,
            false,
            None,
            &volume_state,
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
        let script = container.create_entrypoint_script(true, None, None, "x86_64");
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("DNF_SDK_HOST"));
        assert!(script.contains("environment-setup"));
        assert!(script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_extension_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, Some("test-ext"), None, "x86_64");
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/extensions/test-ext"));
        assert!(!script.contains("cd /opt/src"));
    }

    #[test]
    fn test_entrypoint_script_with_runtime_sysroot() {
        let container = SdkContainer::new();
        let script = container.create_entrypoint_script(true, None, Some("test-runtime"), "x86_64");
        assert!(script.contains("AVOCADO_SDK_PREFIX"));
        assert!(script.contains("cd /opt/_avocado/x86_64/runtimes/test-runtime"));
        assert!(!script.contains("cd /opt/src"));
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
