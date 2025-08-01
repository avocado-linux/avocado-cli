use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod utils;

use commands::init::InitCommand;
use commands::sdk::SdkInstallCommand;

#[derive(Parser)]
#[command(name = "avocado")]
#[command(about = "Avocado CLI - A command line interface for Avocado")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Global target architecture
    #[arg(long)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new avocado project
    Init {
        /// Directory to initialize (defaults to current directory)
        directory: Option<String>,
    },
    /// SDK related commands
    Sdk {
        #[command(subcommand)]
        command: SdkCommands,
    },
}

#[derive(Subcommand)]
enum SdkCommands {
    /// Install dependencies into the SDK
    Install {
        /// Path to avocado.toml configuration file
        #[arg(short, long, default_value = "avocado.toml")]
        config: String,
        /// Enable verbose output
        #[arg(short, long)]
        verbose: bool,
        /// Force the operation to proceed, bypassing warnings or confirmation prompts
        #[arg(short, long)]
        force: bool,
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
        },
    }
}
