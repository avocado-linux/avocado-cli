use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod utils;

use commands::clean::CleanCommand;
use commands::ext::{
    ExtBuildCommand, ExtCleanCommand, ExtDepsCommand, ExtDnfCommand, ExtImageCommand,
    ExtInstallCommand, ExtListCommand,
};
use commands::init::InitCommand;
use commands::runtime::{
    RuntimeBuildCommand, RuntimeCleanCommand, RuntimeDepsCommand, RuntimeDnfCommand,
    RuntimeInstallCommand, RuntimeListCommand, RuntimeProvisionCommand,
};
use commands::sdk::{
    SdkCleanCommand, SdkCompileCommand, SdkDepsCommand, SdkDnfCommand, SdkInstallCommand,
    SdkRunCommand,
};

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
    },
    /// Runtime management commands
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommands,
    },
    /// Clean the avocado project by removing the _avocado directory
    Clean {
        /// Directory to clean (defaults to current directory)
        directory: Option<String>,
    },
}

#[derive(Subcommand)]
enum SdkCommands {
    /// Create and run an SDK container
    Run {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
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
        #[arg(short, long)]
        env: bool,
        /// Command and arguments to run in container
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List SDK dependencies
    Deps {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Run compile scripts
    Compile {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Remove the SDK directory
    Clean {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
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
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build a runtime
    Build {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
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
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Provision a runtime
    Provision {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
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
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List runtime names
    List {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
    },
    /// List dependencies for a runtime
    Deps {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Runtime name to list dependencies for
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
    },
    /// Run DNF commands in a runtime's context
    Dnf {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the runtime to operate on
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the runtime to clean
        #[arg(short = 'r', long = "runtime", required = true)]
        runtime: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { directory } => {
            let init_cmd = InitCommand::new(cli.target, directory);
            init_cmd.execute()?;
            Ok(())
        }
        Commands::Clean { directory } => {
            let clean_cmd = CleanCommand::new(directory);
            clean_cmd.execute()?;
            Ok(())
        }
        Commands::Runtime { command } => match command {
            RuntimeCommands::Install {
                runtime,
                config,
                verbose,
                force,
                container_args,
                dnf_args,
            } => {
                let install_cmd = RuntimeInstallCommand::new(
                    runtime,
                    config,
                    verbose,
                    force,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let build_cmd = RuntimeBuildCommand::new(
                    runtime,
                    config,
                    verbose,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let provision_cmd = RuntimeProvisionCommand::new(
                    runtime,
                    config,
                    verbose,
                    force,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                provision_cmd.execute().await?;
                Ok(())
            }
            RuntimeCommands::List { config } => {
                let list_cmd = RuntimeListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Deps { config, runtime } => {
                let deps_cmd = RuntimeDepsCommand::new(config, runtime);
                deps_cmd.execute()?;
                Ok(())
            }
            RuntimeCommands::Dnf {
                config,
                verbose,
                runtime,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = RuntimeDnfCommand::new(
                    config,
                    runtime,
                    command,
                    verbose,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let clean_cmd = RuntimeCleanCommand::new(
                    runtime,
                    config,
                    verbose,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                clean_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Ext { command } => match command {
            ExtCommands::Install {
                config,
                verbose,
                force,
                extension,
                container_args,
                dnf_args,
            } => {
                let install_cmd = ExtInstallCommand::new(
                    extension,
                    config,
                    verbose,
                    force,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let build_cmd = ExtBuildCommand::new(
                    extension,
                    config,
                    verbose,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                build_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::List { config } => {
                let list_cmd = ExtListCommand::new(config);
                list_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Deps { config, extension } => {
                let deps_cmd = ExtDepsCommand::new(config, extension);
                deps_cmd.execute()?;
                Ok(())
            }
            ExtCommands::Dnf {
                config,
                verbose,
                extension,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd = ExtDnfCommand::new(
                    config,
                    extension,
                    command,
                    verbose,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let clean_cmd = ExtCleanCommand::new(
                    extension,
                    config,
                    verbose,
                    cli.target,
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
                container_args,
                dnf_args,
            } => {
                let image_cmd = ExtImageCommand::new(
                    extension,
                    config,
                    verbose,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                image_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Sdk { command } => match command {
            SdkCommands::Install {
                config,
                verbose,
                force,
                container_args,
                dnf_args,
            } => {
                let install_cmd = SdkInstallCommand::new(
                    config,
                    verbose,
                    force,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                install_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Run {
                config,
                name,
                detach,
                rm,
                interactive,
                verbose,
                env,
                command,
                container_args,
                dnf_args,
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
                    cmd,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                run_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Deps {
                config,
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
                sections,
                container_args,
                dnf_args,
            } => {
                let compile_cmd = SdkCompileCommand::new(
                    config,
                    verbose,
                    sections,
                    cli.target,
                    container_args,
                    dnf_args,
                );
                compile_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Dnf {
                config,
                command,
                container_args,
                dnf_args,
            } => {
                let dnf_cmd =
                    SdkDnfCommand::new(config, command, cli.target, container_args, dnf_args);
                dnf_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Clean {
                config,
                verbose,
                container_args,
                dnf_args,
            } => {
                let clean_cmd =
                    SdkCleanCommand::new(config, verbose, cli.target, container_args, dnf_args);
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
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
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Build sysext and/or confext extensions from configuration
    Build {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to build (must be defined in config)
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// List extension names
    List {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
    },
    /// List dependencies for extensions
    Deps {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Name of the extension to show dependencies for (if not provided, shows all extensions)
        #[arg(short = 'e', long = "extension")]
        extension: Option<String>,
    },
    /// Run DNF commands in an extension's context
    Dnf {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to operate on
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
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
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to clean
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
    /// Create squashfs image from system extension
    Image {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Name of the extension to create image for
        #[arg(short = 'e', long = "extension", required = true)]
        extension: String,
        /// Additional arguments to pass to the container runtime
        #[arg(long = "container-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        container_args: Option<Vec<String>>,
        /// Additional arguments to pass to DNF commands
        #[arg(long = "dnf-arg", num_args = 1, allow_hyphen_values = true, action = clap::ArgAction::Append)]
        dnf_args: Option<Vec<String>>,
    },
}
