//! The TUI and control binary: `aif`, `aif stop`, and `aif doctor`.

use std::io::Write;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

use clap::{Parser, Subcommand};

use anyhow::{bail, Context};

use aif::config;
use aif::exec::RealExec;
use aif::sock::{Action, Client};

#[path = "../doctor.rs"]
mod doctor;

use doctor::DoctorEnv;

/// How long `aif` waits for a started daemon to open the socket.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `aif stop` waits for the socket to disappear.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Command line for `aif`.
#[derive(Parser)]
#[command(
    name = "aif",
    about = "AI Factory terminal UI and control",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Start a new daemon with the whole factory paused: it polls and
    /// reports, but dispatches nothing until the operator resumes with `P`
    /// in the UI. A factory that points at live repositories can then start
    /// without dispatching anything. The flag cannot apply when a daemon
    /// already runs; pause that daemon with `P`, or stop it with `aif stop`
    /// first.
    #[arg(long)]
    paused: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

/// The `aif` subcommands.
#[derive(Subcommand)]
enum Command {
    /// Run the terminal UI. This is the default when no subcommand is given.
    Tui {
        /// Start a new daemon with the whole factory paused.
        #[arg(long)]
        paused: bool,
    },
    /// Stop the daemon.
    Stop,
    /// Report on the installation.
    Doctor {
        /// Path to the config file. Defaults to the config directory.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Remove the worktrees of closed issues and merged pull requests.
        #[arg(long)]
        clean: bool,
        /// Answer the confirmation question of `--clean` with yes.
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Stop) => stop(),
        Some(Command::Doctor { config, clean, yes }) => doctor_main(config, clean, yes),
        Some(Command::Tui { paused }) => tui(paused),
        None => tui(cli.paused),
    };
    if code != 0 {
        exit(code);
    }
}

/// Ensure a daemon runs, then start the terminal UI.
///
/// `paused` reaches a daemon that this call starts. A daemon that already
/// runs keeps its own state, so the call says so instead of applying the
/// flag.
fn tui(paused: bool) -> i32 {
    if let Err(error) = ensure_daemon(paused) {
        eprintln!("aif: {error:#}");
        return 1;
    }
    let socket = config::socket_path();
    if let Err(error) = aif::tui::run(&socket) {
        eprintln!("aif: {error:#}");
        return 1;
    }
    0
}

/// Start the daemon unless one answers already.
///
/// The start goes through `systemd-run --user` first and falls back to a
/// plain detached spawn. It waits up to [`DAEMON_START_TIMEOUT`] for the
/// daemon to open the socket.
fn ensure_daemon(paused: bool) -> anyhow::Result<()> {
    let socket = config::socket_path();
    let program = doctor::daemon_program();
    let outcome = doctor::start_detached(
        &socket,
        &program,
        &RealExec,
        DAEMON_START_TIMEOUT,
        paused,
        &mut doctor::spawn_detached,
    )?;
    match outcome {
        doctor::StartOutcome::Started => Ok(()),
        doctor::StartOutcome::AlreadyRunning if paused => bail!(
            "a daemon already runs on {}; --paused cannot apply. \
             Pause it with P in the UI, or run `aif stop` first and start again.",
            socket.display()
        ),
        doctor::StartOutcome::AlreadyRunning => Ok(()),
    }
}

/// Send the stop action to the daemon and wait for the socket to disappear.
///
/// The exit code is 0 on success and 1 on any failure. A successful stop
/// also unloads the transient systemd unit, so a start that follows at once
/// cannot hit a unit that systemd still holds.
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
    if doctor::wait_socket_gone(&path, STOP_TIMEOUT) {
        doctor::cleanup_daemon_unit(&RealExec);
        println!("aif stop: the daemon stopped");
        0
    } else {
        eprintln!(
            "aif stop: the daemon still listens on {} after {} s",
            path.display(),
            STOP_TIMEOUT.as_secs()
        );
        1
    }
}

/// Run every doctor check, or the clean when asked.
///
/// The exit code is 0 when nothing failed and 1 when a check or a removal
/// failed.
fn doctor_main(config_path: Option<PathBuf>, do_clean: bool, yes: bool) -> i32 {
    let config_path = config_path.unwrap_or_else(config::default_config_path);
    let state_dir = config::state_dir();
    let socket = config::socket_path();
    let env = DoctorEnv {
        config_path: &config_path,
        state_dir: &state_dir,
        socket: &socket,
        exec: &RealExec,
    };
    if do_clean {
        match doctor::clean(&env, yes, &mut ask_to_remove) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("aif doctor: {error:#}");
                1
            }
        }
    } else {
        let checks = doctor::report(&env);
        doctor::print_report(&checks);
        i32::from(doctor::has_failures(&checks))
    }
}

/// Ask the operator on the terminal to confirm the removal.
///
/// End of input means no. An input or output error propagates.
fn ask_to_remove() -> anyhow::Result<bool> {
    print!("Remove these worktrees? [y/N] ");
    std::io::stdout()
        .flush()
        .context("cannot write the removal confirmation")?;
    let mut line = String::new();
    let count = std::io::stdin()
        .read_line(&mut line)
        .context("cannot read the removal confirmation")?;
    Ok(count != 0 && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_parses_both_required_paused_forms() {
        let parsed = Cli::try_parse_from(["aif", "--paused"]).expect("the arguments must parse");
        assert!(parsed.paused);
        assert!(parsed.command.is_none());

        let parsed =
            Cli::try_parse_from(["aif", "tui", "--paused"]).expect("the arguments must parse");
        assert!(!parsed.paused);
        assert!(matches!(
            parsed.command,
            Some(Command::Tui { paused: true })
        ));

        let parsed = Cli::try_parse_from(["aif"]).expect("the arguments must parse");
        assert!(!parsed.paused);

        let parsed = Cli::try_parse_from(["aif", "tui"]).expect("the arguments must parse");
        assert!(!parsed.paused);
        assert!(matches!(
            parsed.command,
            Some(Command::Tui { paused: false })
        ));

        let parsed = Cli::try_parse_from(["aif", "stop"]).expect("the arguments must parse");
        assert!(!parsed.paused);

        assert!(Cli::try_parse_from(["aif", "--paused", "stop"]).is_err());
        assert!(Cli::try_parse_from(["aif", "--paused", "doctor"]).is_err());
    }
}
