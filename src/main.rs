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
use commands::runtime::{RuntimeBuildCommand, RuntimeDepsCommand, RuntimeListCommand};
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
        /// Command and arguments to run in container
        command: Vec<String>,
    },
    /// List SDK dependencies
    Deps {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
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
    },
    /// Run DNF commands in the SDK context
    Dnf {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// DNF command and arguments to execute
        dnf_args: Vec<String>,
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
    },
    /// Remove the SDK directory
    Clean {
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
    },
}

#[derive(Subcommand)]
enum RuntimeCommands {
    /// Build a runtime
    Build {
        /// Runtime name to build
        runtime: String,
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
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
        runtime: String,
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
            RuntimeCommands::Build {
                runtime,
                config,
                verbose,
                force,
            } => {
                let build_cmd =
                    RuntimeBuildCommand::new(runtime, config, verbose, force, cli.target);
                build_cmd.execute().await?;
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
        },
        Commands::Ext { command } => match command {
            ExtCommands::Install {
                config,
                verbose,
                force,
                extension,
            } => {
                let install_cmd =
                    ExtInstallCommand::new(extension, config, verbose, force, cli.target);
                install_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Build {
                extension,
                config,
                verbose,
            } => {
                let build_cmd = ExtBuildCommand::new(extension, config, verbose, cli.target);
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
                dnf_args,
            } => {
                let dnf_cmd = ExtDnfCommand::new(config, extension, dnf_args, verbose, cli.target);
                dnf_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Clean {
                extension,
                config,
                verbose,
            } => {
                let clean_cmd = ExtCleanCommand::new(extension, config, verbose, cli.target);
                clean_cmd.execute().await?;
                Ok(())
            }
            ExtCommands::Image {
                extension,
                config,
                verbose,
            } => {
                let image_cmd = ExtImageCommand::new(extension, config, verbose, cli.target);
                image_cmd.execute().await?;
                Ok(())
            }
        },
        Commands::Sdk { command } => match command {
            SdkCommands::Install {
                config,
                verbose,
                force,
            } => {
                let install_cmd = SdkInstallCommand::new(config, verbose, force, cli.target);
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
                command,
            } => {
                let run_cmd = SdkRunCommand::new(
                    config,
                    name,
                    detach,
                    rm,
                    interactive,
                    verbose,
                    command,
                    cli.target,
                );
                run_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Deps { config } => {
                let deps_cmd = SdkDepsCommand::new(config);
                deps_cmd.execute()?;
                Ok(())
            }
            SdkCommands::Compile {
                config,
                verbose,
                sections,
            } => {
                let compile_cmd = SdkCompileCommand::new(config, verbose, sections, cli.target);
                compile_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Dnf { config, dnf_args } => {
                let dnf_cmd = SdkDnfCommand::new(config, dnf_args, cli.target);
                dnf_cmd.execute().await?;
                Ok(())
            }
            SdkCommands::Clean { config, verbose } => {
                let clean_cmd = SdkCleanCommand::new(config, verbose, cli.target);
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
        extension: Option<String>,
    },
    /// Build sysext and/or confext extensions from configuration
    Build {
        /// Name of the extension to build (must be defined in config)
        extension: String,
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
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
        extension: String,
        /// DNF command and arguments to execute
        dnf_args: Vec<String>,
    },
    /// Clean an extension's sysroot
    Clean {
        /// Name of the extension to clean
        extension: String,
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
    },
    /// Create squashfs image from system extension
    Image {
        /// Name of the extension to create image for
        extension: String,
        /// Path to avocado.toml configuration file
        #[arg(short = 'C', long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
    },
}
