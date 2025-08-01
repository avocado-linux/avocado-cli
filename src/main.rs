use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

use commands::init::InitCommand;

#[derive(Parser)]
#[command(name = "avocado")]
#[command(about = "Avocado CLI - A command line interface for Avocado")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Global target architecture
    #[arg(long, global = true)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new avocado project
    Init {
        /// Directory to initialize (defaults to current directory)
        directory: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { directory } => {
            let init_cmd = InitCommand::new(cli.target, directory);
            init_cmd.execute()
        }
    }
}
