use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Command to initialize a new Avocado project with configuration files.
///
/// This command creates a new `avocado.toml` configuration file in the specified
/// directory with default settings for the Avocado build system.
pub struct InitCommand {
    /// Target architecture (e.g., "qemux86-64")
    target: Option<String>,
    /// Directory to initialize (defaults to current directory)
    directory: Option<String>,
}

impl InitCommand {
    /// Creates a new InitCommand instance.
    ///
    /// # Arguments
    /// * `target` - Optional target architecture string
    /// * `directory` - Optional directory path to initialize
    pub fn new(target: Option<String>, directory: Option<String>) -> Self {
        Self { target, directory }
    }

    /// Detects the system architecture and returns the appropriate default target.
    ///
    /// # Returns
    /// * `"qemux86-64"` for x86_64 systems
    /// * `"qemuarm64"` for aarch64 systems
    /// * `"qemux86-64"` as fallback for unknown architectures
    pub fn get_default_target() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "qemux86-64",
            "aarch64" => "qemuarm64",
            _ => "qemux86-64", // fallback to x86_64 for unknown architectures
        }
    }

    /// Executes the init command, creating the avocado.toml configuration file.
    ///
    /// # Returns
    /// * `Ok(())` if the initialization was successful
    /// * `Err` if there was an error during initialization
    ///
    /// # Errors
    /// This function will return an error if:
    /// * The target directory cannot be created
    /// * The avocado.toml file already exists
    /// * The configuration file cannot be written
    pub fn execute(&self) -> Result<()> {
        let target = self
            .target
            .as_deref()
            .unwrap_or_else(|| Self::get_default_target());
        let directory = self.directory.as_deref().unwrap_or(".");

        // Validate and create directory if it doesn't exist
        if !Path::new(directory).exists() {
            fs::create_dir_all(directory)
                .with_context(|| format!("Failed to create directory '{directory}'"))?;
        }

        // Create the avocado.toml file path
        let toml_path = Path::new(directory).join("avocado.toml");

        // Check if configuration file already exists
        if toml_path.exists() {
            anyhow::bail!(
                "Configuration file '{}' already exists.",
                toml_path.display()
            );
        }

        // Create the configuration content
        let config_content = format!(
            r#"default_target = "{target}"

# Add adiditonal supported targets or use "*" for all targets
supported_targets = ["{target}"]

##
## Runtimes
##

[runtime.dev.dependencies]
avocado-img-bootfiles = "*"
avocado-img-rootfs = "*"
avocado-img-initramfs = "*"
avocado-ext-dev = {{ ext = "avocado-ext-dev", vsn = "*" }}
avocado-ext-sshd-dev = {{ ext = "avocado-ext-sshd-dev", vsn = "*" }}
config = {{ ext = "config" }}
app = {{ ext = "app" }}

##
## Extensions
##

# Generated default app extension
# Use or modify this to install dependencies and or include sdk compiled code
[ext.app]
types = ["sysext", "confext"]

# Install application dependencies
[ext.app.dependencies]
#curl = "*"
#iperf3 = "*"

# Generated default config extension
# Use or modify this to configure "real" user accounts (passwd, shadow, group)
# or configure other system services
[ext.config]
types = ["confext"]

# NOT FOR PRODUCTION: Set root password to empty string
[ext.config.users.root]
password = ""

##
## SDK
##

[sdk]
image = "docker.io/avocadolinux/sdk:apollo-edge"

[sdk.dependencies]
avocado-sdk-toolchain = "*"

[sdk.qemux86-64]
container_args = ["--network=host"]

[sdk.qemuarm64]
container_args = ["--network=host"]

##
## Provisioning
##

# When provisioning using usb or sd provisioning profiles, set extra sdk
# container arguments to allow access to these devices

[provision.usb]
container_args = ["-v", "/dev:/dev", "-v", "/sys:/sys:ro", "--privileged"]

[provision.sd]
container_args = ["-v", "/dev:/dev", "-v", "/sys:/sys:ro", "--privileged"]
"#
        );

        // Write the configuration file
        fs::write(&toml_path, config_content).with_context(|| {
            format!(
                "Failed to write configuration file '{}'",
                toml_path.display()
            )
        })?;

        println!(
            "✓ Created config at {}.",
            toml_path
                .canonicalize()
                .unwrap_or_else(|_| toml_path.to_path_buf())
                .display()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_init_default_target() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()));
        let result = init_cmd.execute();

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.toml");
        assert!(config_path.exists());

        let content = fs::read_to_string(&config_path).unwrap();
        let expected_target = InitCommand::get_default_target();
        assert!(content.contains(&format!("default_target = \"{expected_target}\"")));
        assert!(content.contains("[runtime.dev.dependencies]"));
        assert!(content.contains("avocado-img-bootfiles = \"*\""));
        assert!(content.contains("avocado-img-rootfs = \"*\""));
        assert!(content.contains("avocado-img-initramfs = \"*\""));
        assert!(content.contains("avocado-ext-dev = { ext = \"avocado-ext-dev\", vsn = \"*\" }"));
        assert!(content.contains("image = \"docker.io/avocadolinux/sdk:apollo-edge\""));
        assert!(content.contains("[ext.app]"));
        assert!(content.contains("types = [\"sysext\", \"confext\"]"));
        assert!(content.contains("[ext.config]"));
        assert!(content.contains("avocado-sdk-toolchain = \"*\""));
    }

    #[test]
    fn test_init_custom_target() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        let init_cmd =
            InitCommand::new(Some("custom-arch".to_string()), Some(temp_path.to_string()));
        let result = init_cmd.execute();

        assert!(result.is_ok());

        let config_path = PathBuf::from(temp_path).join("avocado.toml");
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("default_target = \"custom-arch\""));
    }

    #[test]
    fn test_init_file_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let config_path = PathBuf::from(temp_path).join("avocado.toml");

        // Create existing file
        fs::write(&config_path, "existing content").unwrap();

        let init_cmd = InitCommand::new(None, Some(temp_path.to_string()));
        let result = init_cmd.execute();

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("already exists"));
    }

    #[test]
    fn test_init_creates_directory() {
        let temp_dir = TempDir::new().unwrap();
        let new_dir_path = temp_dir.path().join("new_project");
        let new_dir_str = new_dir_path.to_str().unwrap();

        let init_cmd = InitCommand::new(None, Some(new_dir_str.to_string()));
        let result = init_cmd.execute();

        assert!(result.is_ok());
        assert!(new_dir_path.exists());

        let config_path = new_dir_path.join("avocado.toml");
        assert!(config_path.exists());
    }
}
