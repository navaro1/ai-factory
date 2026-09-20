//! The TUI and control binary: `aif`, `aif stop`, and `aif doctor`.

use std::io::Write;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

use clap::{Parser, Subcommand};

use anyhow::{bail, Context};

use aif::config;
use aif::exec::RealExec;
use aif::sock::{Action, Client, MeasureResult, Push, TheoryAction};

#[path = "../doctor.rs"]
mod doctor;

use doctor::DoctorEnv;

/// How long `aif` waits for a started daemon to open the socket.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `aif stop` waits for the socket to disappear.
///
/// The daemon stops every live agent session before it exits. The wait
/// covers the full stop ladder: the interrupt grace, `SIGTERM`, and
/// `SIGKILL` in `src/proc.rs`, with room to spare.
const STOP_TIMEOUT: Duration = Duration::from_secs(40);

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
    /// Measure the touched areas of the current worktree against the
    /// merge base, and print the comparison table.
    Measure {
        /// Measure only these areas of the verification map. Without the
        /// flag the daemon measures the areas the working tree diff
        /// touches.
        #[arg(long, value_name = "AREA")]
        area: Vec<String>,
    },
    /// Report on the installation.
    Doctor {
        /// Path to the config file. Defaults to the config directory.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Remove the worktrees of closed tickets and merged PRs.
        #[arg(long)]
        clean: bool,
        /// Answer the confirmation question of `--clean` with yes.
        #[arg(long)]
        yes: bool,
        /// Start one audit sweep in this repository through the running
        /// daemon, then exit. The daemon reads the model and the run
        /// skills, audits them against the code, and opens one theory
        /// event per finding.
        #[arg(long, value_name = "ALIAS", conflicts_with = "clean")]
        audit: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Stop) => stop(),
        Some(Command::Measure { area }) => measure_main(&area),
        Some(Command::Doctor {
            config,
            clean,
            yes,
            audit,
        }) => doctor_main(config, clean, yes, audit),
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
    println!(
        "aif stop: the daemon stops its agent sessions; the wait can take up to {} s",
        STOP_TIMEOUT.as_secs()
    );
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

/// Send one measure request for the current directory to the daemon.
///
/// The daemon runs the touched areas at the working tree and answers one
/// comparison table. The exit code is 0 on a pass, 1 on a failed
/// verdict, and 3 on an error: no daemon, a broken transport, or an
/// `error:` reply.
fn measure_main(areas: &[String]) -> i32 {
    let path = config::socket_path();
    let mut client = match Client::connect(&path) {
        Ok(client) => client,
        Err(error) => {
            eprintln!(
                "aif measure: no daemon is listening on {}: {error}",
                path.display()
            );
            return MEASURE_EXIT_ERROR;
        }
    };
    let request = measure_request();
    if let Err(error) = client.send(&measure_action(&request, areas)) {
        eprintln!("aif measure: cannot send the measure request: {error}");
        return MEASURE_EXIT_ERROR;
    }
    let pushes = match client.pushes() {
        Ok(pushes) => pushes,
        Err(error) => {
            eprintln!("aif measure: {error:#}");
            return MEASURE_EXIT_ERROR;
        }
    };
    for push in pushes {
        match push {
            // The daemon answers with the state push first, and the
            // unrelated pushes of other clients pass by.
            Ok(Push::MeasureResult(result)) if result.request == request => {
                println!("{}", result.text);
                return measure_exit(&result);
            }
            Ok(_) => continue,
            Err(error) => {
                eprintln!("aif measure: {error:#}");
                return MEASURE_EXIT_ERROR;
            }
        }
    }
    eprintln!("aif measure: the daemon closed the connection before the result");
    MEASURE_EXIT_ERROR
}

/// The exit code `aif measure` returns on every error.
const MEASURE_EXIT_ERROR: i32 = 3;

/// The action behind one `aif measure` call.
fn measure_action(request: &str, areas: &[String]) -> Action {
    Action::Theory(TheoryAction::Measure {
        path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        areas: areas.to_vec(),
        request: request.to_string(),
    })
}

/// The unique request identity of one `aif measure` call.
fn measure_request() -> String {
    format!("measure-{}", uuid::Uuid::new_v4())
}

/// The exit code of one measure reply.
///
/// An `error:` text is an error, a pass verdict is a pass, and every
/// other verdict fails.
fn measure_exit(result: &MeasureResult) -> i32 {
    if result.text.starts_with("error:") {
        MEASURE_EXIT_ERROR
    } else if result.pass {
        0
    } else {
        1
    }
}

/// Run every doctor check, or the clean when asked.
///
/// The exit code is 0 when nothing failed and 1 when a check or a removal
/// failed.
fn doctor_main(
    config_path: Option<PathBuf>,
    do_clean: bool,
    yes: bool,
    audit: Option<String>,
) -> i32 {
    if let Some(alias) = audit {
        return audit_sweep(&alias);
    }
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

/// Send one audit sweep request to the daemon.
///
/// The exit code is 0 when the daemon took the request and 1 when no
/// daemon listens or the send failed.
fn audit_sweep(alias: &str) -> i32 {
    let path = config::socket_path();
    let mut client = match Client::connect(&path) {
        Ok(client) => client,
        Err(error) => {
            eprintln!(
                "aif doctor --audit: no daemon is listening on {}: {error}",
                path.display()
            );
            return 1;
        }
    };
    if let Err(error) = client.send(&audit_action(alias)) {
        eprintln!("aif doctor --audit: cannot send the audit request: {error}");
        return 1;
    }
    println!("aif doctor --audit: the daemon starts the audit sweep of {alias}");
    0
}

/// The action behind one `--audit` request.
fn audit_action(alias: &str) -> Action {
    Action::Theory(TheoryAction::Sweep {
        repo: alias.to_string(),
    })
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

    #[test]
    fn measure_lever_builds_the_action_with_the_given_areas_and_request() {
        let parsed = Cli::try_parse_from([
            "aif",
            "measure",
            "--area",
            "web-checkout",
            "--area",
            "api-orders",
        ])
        .expect("the arguments must parse");
        let Some(Command::Measure { area }) = parsed.command else {
            panic!("the measure command must parse");
        };
        assert_eq!(area, vec!["web-checkout", "api-orders"]);

        let Action::Theory(TheoryAction::Measure {
            areas,
            request,
            path: _,
        }) = measure_action("measure-1", &area)
        else {
            panic!("the measure command must build one theory measure action");
        };
        assert_eq!(
            areas,
            vec!["web-checkout".to_string(), "api-orders".to_string()]
        );
        assert_eq!(request, "measure-1");

        let parsed = Cli::try_parse_from(["aif", "measure"]).expect("the arguments must parse");
        assert!(matches!(parsed.command, Some(Command::Measure { .. })));
    }

    #[test]
    fn measure_lever_maps_the_reply_and_the_transport_to_exit_codes() {
        let pass = MeasureResult {
            request: "measure-1".to_string(),
            text: "AREA web-checkout\npoll_p95  12 \u{2192} 12  ms  unchanged".to_string(),
            pass: true,
        };
        assert_eq!(measure_exit(&pass), 0);

        let failed = MeasureResult {
            request: "measure-1".to_string(),
            text: "AREA web-checkout\npoll_p95  12 \u{2192} 14  ms  worsened".to_string(),
            pass: false,
        };
        assert_eq!(measure_exit(&failed), 1);

        let error = MeasureResult {
            request: "measure-1".to_string(),
            text: "error: the governor of borsuk is off".to_string(),
            pass: false,
        };
        assert_eq!(measure_exit(&error), 3);
    }

    #[test]
    fn the_audit_option_builds_the_sweep_action_and_refuses_the_clean() {
        let parsed = Cli::try_parse_from(["aif", "doctor", "--audit", "borsuk"])
            .expect("the arguments must parse");
        let Some(Command::Doctor { audit, .. }) = parsed.command else {
            panic!("the doctor command must parse");
        };
        assert_eq!(audit.as_deref(), Some("borsuk"));

        assert!(Cli::try_parse_from(["aif", "doctor", "--audit", "borsuk", "--clean"]).is_err());

        assert_eq!(
            audit_action("borsuk"),
            Action::Theory(TheoryAction::Sweep {
                repo: "borsuk".to_string()
            })
        );
    }
}
