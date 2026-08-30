//! The daemon binary. Chunk 15 fills in the real daemon.

use clap::{Parser, Subcommand};

/// Command line for `aifd`.
#[derive(Parser)]
#[command(name = "aifd", about = "AI Factory daemon", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The one `aifd` command.
#[derive(Subcommand)]
enum Command {
    /// Run the daemon event loop.
    Run {
        /// Path to the config file. Defaults to the config directory.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}

fn main() {
    match Cli::parse().command {
        Command::Run { .. } => println!("aifd: not implemented yet"),
    }
}
