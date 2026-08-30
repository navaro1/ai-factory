//! The TUI and control binary. Later chunks fill in the TUI and doctor.

use std::process::exit;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use aif::config;
use aif::sock::{Action, Client};

/// How long `aif stop` waits for the daemon to leave the socket.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// How often `aif stop` rechecks the socket file.
const STOP_POLL: Duration = Duration::from_millis(100);

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
    let code = match Cli::parse().command {
        Some(Command::Stop) => stop(),
        Some(Command::Doctor) => {
            println!("aif doctor: not implemented yet");
            0
        }
        Some(Command::Tui) | None => {
            println!("aif tui: not implemented yet");
            0
        }
    };
    if code != 0 {
        exit(code);
    }
}

/// Send the stop action to the daemon and wait for it to leave the socket.
///
/// The exit code is 0 on success and 1 on any failure.
fn stop() -> i32 {
    let path = config::socket_path();
    let mut client = match Client::connect(&path) {
        Ok(client) => client,
        Err(error) => {
            eprintln!(
                "aif stop: no daemon is listening on {}: {error}",
                path.display()
            );
            return 1;
        }
    };
    if let Err(error) = client.send(&Action::Stop) {
        eprintln!("aif stop: cannot send the stop action: {error}");
        return 1;
    }
    println!("aif stop: stop sent to {}", path.display());
    let deadline = Instant::now() + STOP_WAIT;
    while path.exists() {
        if Instant::now() >= deadline {
            eprintln!("aif stop: the daemon did not stop within 10 s");
            return 1;
        }
        std::thread::sleep(STOP_POLL);
    }
    println!("aif stop: the daemon stopped");
    0
}
