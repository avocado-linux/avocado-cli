use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashMap;

mod commands;
mod utils;

use commands::build::BuildCommand;
use commands::clean::CleanCommand;
use commands::ext::{
    ExtBuildCommand, ExtCheckoutCommand, ExtCleanCommand, ExtDepsCommand, ExtDnfCommand,
    ExtImageCommand, ExtInstallCommand, ExtListCommand, ExtPackageCommand,
};
use commands::fetch::FetchCommand;
use commands::hitl::HitlServerCommand;
use commands::init::InitCommand;
use commands::install::InstallCommand;
use commands::provision::ProvisionCommand;
use commands::runtime::{
    RuntimeBuildCommand, RuntimeCleanCommand, RuntimeDeployCommand, RuntimeDepsCommand,
    RuntimeDnfCommand, RuntimeInstallCommand, RuntimeListCommand, RuntimeProvisionCommand,
    RuntimeSignCommand,
};
use commands::sdk::{
    SdkCleanCommand, SdkCompileCommand, SdkDepsCommand, SdkDnfCommand, SdkInstallCommand,
    SdkRunCommand,
};
use commands::sign::SignCommand;
use commands::signing_keys::{
    SigningKeysCreateCommand, SigningKeysListCommand, SigningKeysRemoveCommand,
};
use commands::upgrade::UpgradeCommand;

#[derive(Parser)]
#[command(name = "avocado")]
#[command(about = "Avocado CLI - A command line interface for Avocado")]
#[command(version)]
#[command(disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Global target architecture
    #[arg(long)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// SDK related commands
    Sdk {
        #[command(subcommand)]
        command: SdkCommands,
    },
    /// Extension related commands
    Ext {
        #[command(subcommand)]
        command: ExtCommands,
    },
    /// Initialize a new avocado project
    Init {
        /// Directory to initialize (defaults to current directory)
        directory: Option<String>,
        /// Target architecture (e.g., "qemux86-64")
        #[arg(long)]
        target: Option<String>,
        /// Reference example to initialize from (downloads from avocado-os/references)
        #[arg(long)]
        reference: Option<String>,
        /// Branch to fetch reference from (defaults to "main")
        #[arg(long)]
        reference_branch: Option<String>,
        /// Specific commit SHA to fetch reference from
        #[arg(long)]
        reference_commit: Option<String>,
        /// Repository to fetch reference from (format: "owner/repo", defaults to "avocado-linux/avocado-os")
        #[arg(long)]
        reference_repo: Option<String>,
    },
    /// Runtime management commands
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommands,
    },
    /// Hardware-in-the-loop testing commands
    Hitl {
        #[command(subcommand)]
        command: HitlCommands,
    },
    /// Clean the avocado project by removing docker volumes and state files
    Clean {
        /// Directory to clean (defaults to current directory)
        directory: Option<String>,
        /// Skip cleaning docker volumes (volumes are cleaned by default)
        #[arg(long)]
        skip_volumes: bool,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
    },
    /// Install all components (SDK, extensions, and runtime dependencies)
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to install dependencies for (if not provided, installs for all runtimes)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Upgrade the CLI to the latest (or specified) version
    Upgrade {
        /// Controls what version to upgrade to. If not specified, the latest version will be used.
        #[arg(long)]
        version: Option<String>,
    },
    /// Build all components (SDK compile, extensions, and runtime images)
    Build {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to build (if not provided, builds all runtimes)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Extension name to build (if not provided, builds all required extensions)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Fetch and refresh repository metadata for sysroots
    Fetch {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Extension name to fetch metadata for (if not provided, fetches for all sysroots)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Runtime name to fetch metadata for (if not provided, fetches for all sysroots)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Provision a runtime (shortcut for 'runtime provision')
    Provision {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to provision
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Provision profile to use
        #[arg(long = "provision-profile")]
        provision_profile: Option<String>,
        /// Environment variables to pass to the provision process
        #[arg(long = "env", num_args = 1, action = clap::ArgAction::Append)]
        env: Option<Vec<String>>,
        /// Output path relative to src_dir for provisioning artifacts
        #[arg(long = "out")]
        out: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Deploy a runtime to a device (shortcut for 'runtime deploy')
    Deploy {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to deploy
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Device IP address or hostname to deploy to
        #[arg(short = 'd', long = "device", required = true)]
        device: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Manage signing keys for extension and image signing
    #[command(name = "signing-keys")]
    SigningKeys {
        #[command(subcommand)]
        command: SigningKeysCommands,
    },
    /// Sign runtime images (shortcut for 'runtime sign')
    Sign {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to sign (if not provided, signs all runtimes with signing config)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum SigningKeysCommands {
    /// Create a new signing key or register an external PKCS#11 key
    Create {
        /// Name for the key (defaults to key ID if not provided)
        name: Option<String>,
        /// PKCS#11 URI for hardware-backed keys (e.g., 'pkcs11:token=YubiKey;object=signing-key')
        #[arg(long)]
        uri: Option<String>,
        /// Hardware device type (tpm, yubikey, or auto-detect)
        #[arg(long, value_name = "DEVICE")]
        pkcs11_device: Option<String>,
        /// PKCS#11 token label (e.g., 'avocado', 'YubiKey PIV'). If not provided, uses the first available token.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Label of existing key to reference in the device
        #[arg(long, value_name = "LABEL")]
        key_label: Option<String>,
        /// Generate a new key in the device
        #[arg(long)]
        generate: bool,
        /// Authentication method for PKCS#11 device (none, prompt, env)
        #[arg(long, default_value = "prompt", value_name = "METHOD")]
        auth: String,
    },
    /// List all registered signing keys
    List,
    /// Remove a signing key
    Remove {
        /// Name or key ID of the key to remove
        name: String,
        /// Delete hardware key from device (requires confirmation)
        #[arg(long)]
        delete: bool,
    },
}

#[derive(Subcommand)]
enum SdkCommands {
    /// Create and run an SDK container
    Run {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Assign a name to the container
        #[arg(long)]
        name: Option<String>,
        /// Run container in background and print container ID
        #[arg(short, long)]
        detach: bool,
        /// Automatically remove the container when it exits
        #[arg(long)]
        rm: bool,
        /// Drop into interactive shell in container
        #[arg(short, long)]
        interactive: bool,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Source the avocado SDK environment before running command
        #[arg(short = 'E', long)]
        env: bool,
        /// Mount extension sysroot and change working directory to it
        #[arg(short = 'e', long)]
        extension: Option<String>,
        /// Mount runtime sysroot and change working directory to it
        #[arg(short = 'r', long)]
        runtime: Option<String>,
        /// Command and arguments to run in container
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Skip SDK bootstrap initialization and go directly to container prompt
        #[arg(long)]
        no_bootstrap: bool,
    },
    /// List SDK dependencies
    Deps {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Run compile scripts
    Compile {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Specific compile sections to run
        sections: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Run DNF commands in the SDK context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Install dependencies into the SDK
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove the SDK directory
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum RuntimeCommands {
    /// Install dependencies into runtime installroots
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to install dependencies for (if not provided, installs for all runtimes)
        #[arg(short = 'r', long = "runtime")]
        runtime: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build a runtime
    Build {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to build
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Provision a runtime
    Provision {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Runtime name to provision
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Provision profile to use
        #[arg(long = "provision-profile")]
        provision_profile: Option<String>,
        /// Environment variables to pass to the provision process
        #[arg(long = "env", num_args = 1, action = clap::ArgAction::Append)]
        env: Option<Vec<String>>,
        /// Output path relative to src_dir for provisioning artifacts
        #[arg(long = "out")]
        out: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List runtime names
    List {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// List dependencies for a runtime
    Deps {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Runtime name to list dependencies for
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run DNF commands in a runtime's context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the runtime to operate on
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Clean runtime installroot directory
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the runtime to clean
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Deploy a runtime to a device
    Deploy {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to deploy
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Device IP address or hostname to deploy to
        #[arg(short = 'd', long = "device", required = true)]
        device: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Sign runtime images
    Sign {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Runtime name to sign
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

/// Parse environment variable arguments in the format "KEY=VALUE" into a HashMap
fn parse_env_vars(env_args: Option<&Vec<String>>) -> Option<HashMap<String, String>> {
    env_args.map(|args| {
        args.iter()
            .filter_map(|arg| {
                let parts: Vec<&str> = arg.splitn(2, '=').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    eprintln!("[WARNING] Invalid environment variable format: '{arg}'. Expected 'KEY=VALUE'.");
                    None
                }
            })
            .collect()
    })
}

/// Combine provision profile and env vars into a single HashMap
fn build_env_vars(
    provision_profile: Option<&String>,
    env_args: Option<&Vec<String>>,
) -> Option<HashMap<String, String>> {
    let mut env_vars = parse_env_vars(env_args).unwrap_or_default();

    if let Some(profile) = provision_profile {
        env_vars.insert("AVOCADO_PROVISION_PROFILE".to_string(), profile.clone());
    }

    if env_vars.is_empty() {
        None
    } else {
        Some(env_vars)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            directory,
            target,
            reference,
            reference_branch,
            reference_commit,
            reference_repo,
        } => {
            let init_cmd = InitCommand::new(
                target.or(cli.target),
                directory,
                reference,
                reference_branch,
                reference_commit,
                reference_repo,
            );
            init_cmd.execute().await?;
            Ok(())
        }
        Commands::Clean {
            directory,
            skip_volumes,
            container_tool,
            verbose,
        } => {
            let clean_cmd =
                CleanCommand::new(directory, !skip_volumes, Some(container_tool), verbose);
            clean_cmd.execute().await?;
            Ok(())
        }
        Commands::Install {
            config,
            verbose,
            force,
            runtime,
            target,
            container_args,
            dnf_args,
        } => {
            let install_cmd = InstallCommand::new(
                config,
                verbose,
                force,
                runtime,
                target.or(cli.target),
                container_args,
                dnf_args,
            );
            install_cmd.execute().await?;
            Ok(())
        }
        Commands::Build {
            config,
            verbose,
            runtime,
            extension,
            target,
            container_args,
            dnf_args,
        } => {
            let build_cmd = BuildCommand::new(
                config,
                verbose,
                runtime,
                extension,
                target.or(cli.target),
                container_args,
                dnf_args,
            );
            build_cmd.execute().await?;
            Ok(())
        }
        Commands::Fetch {
            config,
            verbose,
            extension,
            runtime,
            target,
            container_args,
            dnf_args,
        } => {
            let fetch_cmd = FetchCommand::new(
                config,
                verbose,
                extension,
                runtime,
                target.or(cli.target),
                container_args,
                dnf_args,
            );
            fetch_cmd.execute().await?;
            Ok(())
        }
        Commands::Upgrade { version } => {
            let cmd = UpgradeCommand { version };
            cmd.run().await?;
            Ok(())
        }
        Commands::Provision {
            config,
            verbose,
            force,
            runtime,
            target,
            provision_profile,
            env,
            out,
            container_args,
            dnf_args,
        } => {
            let provision_cmd =
                ProvisionCommand::new(crate::commands::provision::ProvisionConfig {
                    runtime,
                    config_path: config,
                    verbose,
                    force,
                    target: target.or(cli.target),
                    provision_profile: provision_profile.clone(),
                    env_vars: build_env_vars(provision_profile.as_ref(), env.as_ref()),
                    out,
                    container_args,
                    dnf_args,
                });
            provision_cmd.execute().await?;
            Ok(())
        }
        Commands::Deploy {
            config,
            verbose,
            runtime,
            target,
            device,
            container_args,
            dnf_args,
        } => {
            let deploy_cmd = RuntimeDeployCommand::new(
                runtime,
                config,
                verbose,
                target.or(cli.target),
                device,
                container_args,
                dnf_args,
            );
            deploy_cmd.execute().await?;
            Ok(())
        }
        Commands::SigningKeys { command } => match command {
            SigningKeysCommands::Create {
                name,
                uri,
                pkcs11_device,
                token,
                key_label,
                generate,
                auth,
            } => {
                let cmd = SigningKeysCreateCommand::new(
                    name,
                    uri,
                    pkcs11_device,
                    token,
                    key_label,
                    generate,
                    auth,
                );
                cmd.execute()?;
                Ok(())
            }
            SigningKeysCommands::List => {
                let cmd = SigningKeysListCommand::new();
                cmd.execute()?;
                Ok(())
            }
            SigningKeysCommands::Remove { name, delete } => {
                let cmd = SigningKeysRemoveCommand::new(name, delete);
                cmd.execute()?;
                Ok(())
            }
        },
        Commands::Sign {
            config,
            verbose,
            runtime,
            target,
            container_args,
            dnf_args,
        } => {
            let sign_cmd = SignCommand::new(
                config,
                verbose,
                runtime,
                target.or(cli.target),
                container_args,
                dnf_args,
            );
            sign_cmd.execute().await?;
            Ok(())
        }
        Commands::Runtime { command } => match command {
            RuntimeCommands::Install {
                runtime,
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = RuntimeInstallCommand::new(
                    runtime,
                    config,
                    verbose,
                    force,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                install_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Build {
                runtime,
                config,
                verbose,
                force: _,
                target,
                container_args,
                dnf_args,
            } => {
                let build_cmd = RuntimeBuildCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                build_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Provision {
                runtime,
                config,
                verbose,
                force,
                target,
                provision_profile,
                env,
                out,
                container_args,
                dnf_args,
            } => {
                let mut provision_cmd = RuntimeProvisionCommand::new(
                    crate::commands::runtime::provision::RuntimeProvisionConfig {
                        runtime_name: runtime,
                        config_path: config,
                        verbose,
                        force,
                        target: target.or(cli.target),
                        provision_profile: provision_profile.clone(),
                        env_vars: build_env_vars(provision_profile.as_ref(), env.as_ref()),
                        out,
                        container_args,
                        dnf_args,
                    },
                );
                provision_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::List { config, target: _ } => {
                let list_cmd = RuntimeListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Deps {
                config,
                runtime,
                target: _,
            } => {
                let deps_cmd = RuntimeDepsCommand::new(config, runtime);
                deps_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Dnf {
                config,
                verbose,
                runtime,
                target,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = RuntimeDnfCommand::new(
                    config,
                    runtime,
                    command,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                dnf_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Clean {
                config,
                verbose,
                runtime,
                target,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = RuntimeCleanCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                clean_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Deploy {
                config,
                verbose,
                runtime,
                target,
                device,
                container_args,
                dnf_args,
            } => {
                let deploy_cmd = RuntimeDeployCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target),
                    device,
                    container_args,
                    dnf_args,
                );
                deploy_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::Sign {
                config,
                verbose,
                runtime,
                target,
                container_args,
                dnf_args,
            } => {
                let sign_cmd = RuntimeSignCommand::new(
                    runtime,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                sign_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Ext { command } => match command {
            ExtCommands::Install {
                config,
                verbose,
                force,
                extension,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = ExtInstallCommand::new(
                    extension,
                    config,
                    verbose,
                    force,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                install_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Build {
                extension,
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let build_cmd = ExtBuildCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                build_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Checkout {
                config,
                verbose,
                extension,
                target,
                ext_path,
                src_path,
                container_tool,
            } => {
                let checkout_cmd = ExtCheckoutCommand::new(
                    extension,
                    ext_path,
                    src_path,
                    config,
                    verbose,
                    container_tool,
                    target.or(cli.target),
                );
                checkout_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::List { config, target: _ } => {
                let list_cmd = ExtListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Deps {
                config,
                extension,
                target,
            } => {
                let deps_cmd = ExtDepsCommand::new(config, extension, target.or(cli.target));
                deps_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Dnf {
                config,
                verbose,
                extension,
                target,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = ExtDnfCommand::new(
                    config,
                    extension,
                    command,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                dnf_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Clean {
                extension,
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = ExtCleanCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                clean_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Image {
                extension,
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let image_cmd = ExtImageCommand::new(
                    extension,
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                image_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Package {
                extension,
                target,
                config,
                verbose,
                output_dir,
                container_args,
                dnf_args,
            } => {
                let package_cmd = ExtPackageCommand::new(
                    config,
                    extension,
                    target,
                    output_dir,
                    verbose,
                    container_args,
                    dnf_args,
                );
                package_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Hitl { command } => match command {
            HitlCommands::Server {
                config_path,
                extensions,
                container_args,
                dnf_args,
                target,
                verbose,
                port,
            } => {
                let hitl_cmd = HitlServerCommand {
                    config_path,
                    extensions,
                    container_args,
                    dnf_args,
                    target: target.or(cli.target),
                    verbose,
                    port,
                };
                hitl_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Sdk { command } => match command {
            SdkCommands::Install {
                config,
                verbose,
                force,
                target,
                container_args,
                dnf_args,
            } => {
                let install_cmd = SdkInstallCommand::new(
                    config,
                    verbose,
                    force,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                install_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Run {
                config,
                target,
                name,
                detach,
                rm,
                interactive,
                verbose,
                env,
                extension,
                runtime,
                command,
                container_args,
                dnf_args,
                no_bootstrap,
            } => {
                let cmd = if command.is_empty() {
                    None
                } else {
                    Some(command)
                };
                let run_cmd = SdkRunCommand::new(
                    config,
                    name,
                    detach,
                    rm,
                    interactive,
                    verbose,
                    env,
                    extension,
                    runtime,
                    cmd,
                    target,
                    container_args,
                    dnf_args,
                    no_bootstrap,
                );
                run_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Deps {
                config,
                target: _,
                container_args: _,
                dnf_args: _,
            } => {
                let deps_cmd = SdkDepsCommand::new(config);
                deps_cmd.execute()?;
                Ok(())
            }
            SdkCommands::Compile {
                config,
                verbose,
                target,
                sections,
                container_args,
                dnf_args,
            } => {
                let compile_cmd = SdkCompileCommand::new(
                    config,
                    verbose,
                    sections,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                compile_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Dnf {
                config,
                verbose,
                target,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = SdkDnfCommand::new(
                    config,
                    verbose,
                    command,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                dnf_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Clean {
                config,
                verbose,
                target,
                container_args,
                dnf_args,
            } => {
                let clean_cmd = SdkCleanCommand::new(
                    config,
                    verbose,
                    target.or(cli.target),
                    container_args,
                    dnf_args,
                );
                clean_cmd.execute().await?;
                Ok(())
            }
        },
    }
}

#[derive(Subcommand)]
enum ExtCommands {
    /// Install dependencies into extension sysroots
    Install {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Name of the extension to install (if not provided, installs all extensions)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build sysext and/or confext extensions from configuration
    Build {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to build (must be defined in config)
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List extension names
    List {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// List dependencies for extensions
    Deps {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Name of the extension to show dependencies for (if not provided, shows all extensions)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run DNF commands in an extension's context
    Dnf {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to operate on
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// DNF command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Clean an extension's sysroot
    Clean {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to clean
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Check out files from extension sysroot to source directory
    Checkout {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to checkout from
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Path within the extension sysroot to checkout (e.g., /etc/config.json or /etc for directory)
        #[arg(long = "ext-path", required = true)]
        ext_path: String,
        /// Destination path in source directory (relative to src root)
        #[arg(long = "src-path", required = true)]
        src_path: String,
        /// Container tool to use (docker/podman)
        #[arg(long, default_value = "docker")]
        container_tool: String,
    },
    /// Create squashfs image from system extension
    Image {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to create image for
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Package extension sysroot into an RPM
    Package {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to package
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Target architecture
        #[arg(short, long)]
        target: Option<String>,
        /// Output directory on host for the RPM package (relative or absolute path). If not specified, RPM stays in container at $AVOCADO_PREFIX/output/extensions
        #[arg(long = "out-dir")]
        output_dir: Option<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum HitlCommands {
    /// Start a HITL server container with preconfigured settings
    Server {
        /// Path to avocado.yaml configuration file
        #[arg(short = 'C', long, default_value = "avocado.yaml")]
        config_path: String,
        /// Extensions to create NFS exports for
        #[arg(short, long = "extension")]
        extensions: Vec<String>,
        /// Additional container arguments
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
        /// Target to build for
        #[arg(short, long)]
        target: Option<String>,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// NFS port number to use
        #[arg(short, long)]
        port: Option<u16>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_vars() {
        let env_args = vec![
            "KEY1=value1".to_string(),
            "KEY2=value2".to_string(),
            "COMPLEX_KEY=value with spaces".to_string(),
        ];

        let result = parse_env_vars(Some(&env_args)).unwrap();

        assert_eq!(result.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(result.get("KEY2"), Some(&"value2".to_string()));
        assert_eq!(
            result.get("COMPLEX_KEY"),
            Some(&"value with spaces".to_string())
        );
    }

    #[test]
    fn test_parse_env_vars_invalid_format() {
        let env_args = vec![
            "VALID_KEY=valid_value".to_string(),
            "INVALID_FORMAT".to_string(),
            "ANOTHER_VALID=another_value".to_string(),
        ];

        let result = parse_env_vars(Some(&env_args)).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("VALID_KEY"), Some(&"valid_value".to_string()));
        assert_eq!(
            result.get("ANOTHER_VALID"),
            Some(&"another_value".to_string())
        );
        assert_eq!(result.get("INVALID_FORMAT"), None);
    }

    #[test]
    fn test_parse_env_vars_empty() {
        let result = parse_env_vars(None);
        assert_eq!(result, None);

        let empty_vec = vec![];
        let result = parse_env_vars(Some(&empty_vec));
        assert_eq!(result, Some(HashMap::new()));
    }

    #[test]
    fn test_build_env_vars_with_provision_profile_only() {
        let result = build_env_vars(Some(&"production".to_string()), None).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get("AVOCADO_PROVISION_PROFILE"),
            Some(&"production".to_string())
        );
    }

    #[test]
    fn test_build_env_vars_with_env_args_only() {
        let env_args = vec!["CUSTOM_VAR=custom_value".to_string()];

        let result = build_env_vars(None, Some(&env_args)).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("CUSTOM_VAR"), Some(&"custom_value".to_string()));
    }

    #[test]
    fn test_build_env_vars_combined() {
        let env_args = vec![
            "AVOCADO_DEVICE_ID=device123".to_string(),
            "AVOCADO_DEVICE_CERT=cert_data".to_string(),
        ];

        let result = build_env_vars(Some(&"staging".to_string()), Some(&env_args)).unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(
            result.get("AVOCADO_PROVISION_PROFILE"),
            Some(&"staging".to_string())
        );
        assert_eq!(
            result.get("AVOCADO_DEVICE_ID"),
            Some(&"device123".to_string())
        );
        assert_eq!(
            result.get("AVOCADO_DEVICE_CERT"),
            Some(&"cert_data".to_string())
        );
    }

    #[test]
    fn test_build_env_vars_empty() {
        let result = build_env_vars(None, None);
        assert_eq!(result, None);

        let empty_vec = vec![];
        let result = build_env_vars(None, Some(&empty_vec));
        assert_eq!(result, None);
    }
}
