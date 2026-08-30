//! The TUI and control binary. Later chunks fill in the subcommands.

use clap::{Parser, Subcommand};

/// Command line for `aif`.
#[derive(Parser)]
#[command(name = "aif", about = "AI Factory terminal UI and control", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// The `aif` subcommands.
#[derive(Subcommand)]
enum Command {
    /// Run the terminal UI. This is the default when no subcommand is given.
    Tui,
    /// Stop the daemon.
    Stop,
    /// Report on the installation.
    Doctor,
}

fn main() {
    match Cli::parse().command {
        Some(Command::Stop) => println!("aif stop: not implemented yet"),
        Some(Command::Doctor) => println!("aif doctor: not implemented yet"),
        Some(Command::Tui) | None => println!("aif tui: not implemented yet"),
    }
}
