use anyhow::Result;

use crate::utils::config::Config;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_error, print_info, print_success, OutputLevel};
use crate::utils::target::resolve_target;

pub struct RuntimeDnfCommand {
    config_path: String,
    runtime: String,
    command: Vec<String>,
    verbose: bool,
    target: Option<String>,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
}

impl RuntimeDnfCommand {
    pub fn new(
        config_path: String,
        runtime: String,
        command: Vec<String>,
        verbose: bool,
        target: Option<String>,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            config_path,
            runtime,
            command,
            verbose,
            target,
            container_args,
            dnf_args,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let merged_container_args = config.merge_sdk_container_args(self.container_args.as_ref());
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        self.validate_runtime_exists(&parsed)?;
        let container_image = self.get_container_image(&parsed)?;
        let target = self.resolve_target_architecture(&parsed)?;

        // Get repo_url and repo_release from config
        let repo_url = config.get_sdk_repo_url();
        let repo_release = config.get_sdk_repo_release();

        self.execute_dnf_command(
            &parsed,
            &container_image,
            &target,
            repo_url,
            repo_release,
            &merged_container_args,
        )
        .await
    }

    fn validate_runtime_exists(&self, parsed: &toml::Value) -> Result<()> {
        let runtime_section = parsed.get("runtime").ok_or_else(|| {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            anyhow::anyhow!("No runtime section found")
        })?;

        let runtime_table = runtime_section
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("Invalid runtime section format"))?;

        if !runtime_table.contains_key(&self.runtime) {
            print_error(
                &format!("Runtime '{}' not found in configuration.", self.runtime),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Runtime not found"));
        }

        Ok(())
    }

    fn get_container_image(&self, parsed: &toml::Value) -> Result<String> {
        parsed
            .get("sdk")
            .and_then(|sdk| sdk.get("image"))
            .and_then(|img| img.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("No container image specified in config under 'sdk.image'.")
            })
    }

    fn resolve_target_architecture(&self, parsed: &toml::Value) -> Result<String> {
        let config_target = self.extract_config_target(parsed);
        let resolved_target = resolve_target(self.target.as_deref(), config_target.as_deref());

        resolved_target.ok_or_else(|| {
            anyhow::anyhow!(
                "No target architecture specified for runtime '{}'. Use --target, AVOCADO_TARGET env var, or config under 'runtime.{}.target'.",
                self.runtime, self.runtime
            )
        })
    }

    fn extract_config_target(&self, parsed: &toml::Value) -> Option<String> {
        parsed
            .get("runtime")
            .and_then(|runtime| runtime.as_table())
            .and_then(|runtime_table| runtime_table.get(&self.runtime))
            .and_then(|runtime_config| runtime_config.get("target"))
            .and_then(|target| target.as_str())
            .map(|s| s.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_dnf_command(
        &self,
        parsed: &toml::Value,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<()> {
        let container_helper = SdkContainer::new();

        // Perform runtime setup first
        self.setup_runtime_environment(
            parsed,
            &container_helper,
            container_image,
            target,
            repo_url,
            repo_release,
            merged_container_args,
        )
        .await?;

        // Build and execute DNF command
        let dnf_command = self.build_dnf_command();
        self.run_dnf_command(
            &container_helper,
            container_image,
            target,
            &dnf_command,
            repo_url,
            repo_release,
            merged_container_args,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn setup_runtime_environment(
        &self,
        _config: &toml::Value,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<()> {
        let check_cmd = format!("test -d $AVOCADO_PREFIX/runtimes/{}", self.runtime);

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: check_cmd,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let dir_exists = container_helper.run_in_container(config).await?;

        if !dir_exists {
            self.create_runtime_directory(
                container_helper,
                container_image,
                target,
                repo_url,
                repo_release,
                merged_container_args,
            )
            .await?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_runtime_directory(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<()> {
        let setup_cmd = format!(
            "mkdir -p $AVOCADO_PREFIX/runtimes/{}/var/lib && cp -rf $AVOCADO_PREFIX/rootfs/var/lib/rpm $AVOCADO_PREFIX/runtimes/{}/var/lib",
            self.runtime, self.runtime
        );

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: setup_cmd,
            verbose: self.verbose,
            source_environment: false, // don't source environment
            interactive: false,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let setup_success = container_helper.run_in_container(config).await?;

        if !setup_success {
            print_error(
                &format!("Failed to set up runtime directory for '{}'.", self.runtime),
                OutputLevel::Normal,
            );
            return Err(anyhow::anyhow!("Failed to create runtime directory"));
        }

        if self.verbose {
            print_info(
                &format!("Created runtime directory for '{}'.", self.runtime),
                OutputLevel::Normal,
            );
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_dnf_command(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        dnf_command: &str,
        repo_url: Option<&String>,
        repo_release: Option<&String>,
        merged_container_args: &Option<Vec<String>>,
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Running DNF command: {dnf_command}"),
                OutputLevel::Normal,
            );
        }

        let config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: dnf_command.to_string(),
            verbose: self.verbose,
            source_environment: true, // source environment for DNF
            interactive: true,
            repo_url: repo_url.cloned(),
            repo_release: repo_release.cloned(),
            container_args: merged_container_args.clone(),
            dnf_args: self.dnf_args.clone(),
            ..Default::default()
        };
        let success = container_helper.run_in_container(config).await?;

        if success {
            print_success("DNF command completed successfully.", OutputLevel::Normal);
            Ok(())
        } else {
            print_error("DNF command failed.", OutputLevel::Normal);
            Err(anyhow::anyhow!("DNF command failed"))
        }
    }

    fn build_dnf_command(&self) -> String {
        let installroot = format!("$AVOCADO_PREFIX/runtimes/{}", self.runtime);
        let command_args_str = self.command.join(" ");
        let dnf_args_str = if let Some(args) = &self.dnf_args {
            format!(" {} ", args.join(" "))
        } else {
            String::new()
        };

        format!(
            r#"
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={installroot} \
    {dnf_args_str} \
    {command_args_str}
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let cmd = RuntimeDnfCommand::new(
            "avocado.toml".to_string(),
            "test-runtime".to_string(),
            vec!["list".to_string(), "installed".to_string()],
            false,
            Some("x86_64".to_string()),
            None,
            None,
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.command, vec!["list", "installed"]);
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
    }

    #[test]
    fn test_new_with_verbose_and_args() {
        let cmd = RuntimeDnfCommand::new(
            "avocado.toml".to_string(),
            "test-runtime".to_string(),
            vec!["install".to_string(), "gcc".to_string()],
            true,
            None,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()]),
            Some(vec!["--nogpgcheck".to_string()]),
        );

        assert_eq!(cmd.config_path, "avocado.toml");
        assert_eq!(cmd.runtime, "test-runtime");
        assert_eq!(cmd.command, vec!["install", "gcc"]);
        assert!(cmd.verbose);
        assert_eq!(cmd.target, None);
        assert_eq!(
            cmd.container_args,
            Some(vec!["--cap-add=SYS_ADMIN".to_string()])
        );
        assert_eq!(cmd.dnf_args, Some(vec!["--nogpgcheck".to_string()]));
    }

    #[test]
    fn test_build_dnf_command() {
        let cmd = RuntimeDnfCommand::new(
            "avocado.toml".to_string(),
            "test-runtime".to_string(),
            vec!["list".to_string(), "installed".to_string()],
            false,
            None,
            None,
            Some(vec!["--nogpgcheck".to_string()]),
        );

        let dnf_command = cmd.build_dnf_command();

        assert!(dnf_command.contains("--installroot=$AVOCADO_PREFIX/runtimes/test-runtime"));
        assert!(dnf_command.contains("list installed"));
        assert!(dnf_command.contains("--nogpgcheck"));
        assert!(dnf_command.contains("RPM_ETCCONFIGDIR"));
        assert!(dnf_command.contains("$DNF_SDK_HOST"));
    }

    #[test]
    fn test_build_dnf_command_no_args() {
        let cmd = RuntimeDnfCommand::new(
            "avocado.toml".to_string(),
            "my-runtime".to_string(),
            vec!["search".to_string(), "python".to_string()],
            false,
            None,
            None,
            None,
        );

        let dnf_command = cmd.build_dnf_command();

        assert!(dnf_command.contains("--installroot=$AVOCADO_PREFIX/runtimes/my-runtime"));
        assert!(dnf_command.contains("search python"));
        assert!(!dnf_command.contains("--nogpgcheck"));
    }
}
