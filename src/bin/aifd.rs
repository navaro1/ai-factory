//! The daemon binary: `aifd run` starts the factory event loop.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use aif::config::{self, Config};
use aif::daemon::Daemon;
use aif::poll;
use aif::sock::{self, Action};

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
        config: Option<PathBuf>,
        /// Start with the whole factory paused. The daemon polls, serves the
        /// socket, and reports to the UI, but dispatches no task until the
        /// operator resumes. A factory that points at live repositories can
        /// then start without dispatching anything.
        #[arg(long)]
        paused: bool,
    },
}

fn main() {
    let code = match Cli::parse().command {
        Command::Run { config, paused } => {
            exit_code(run(config.as_deref(), &config::socket_path(), paused))
        }
    };
    if code != 0 {
        exit(code);
    }
}

/// Map the outcome of a daemon run to a process exit code.
///
/// A failure prints its whole error chain on stderr and maps to 1.
fn exit_code(result: anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("aifd: {error:#}");
            1
        }
    }
}

/// Load the config, bind the control socket, spawn the pollers, and run the
/// daemon until the operator stops it.
///
/// A true `paused` starts the factory with `Paused.global` set: the daemon
/// polls and reports, but dispatches nothing until the operator resumes.
///
/// The call drops the server before it returns, so every connected client
/// sees its stream close and the socket file disappears. Dropping the daemon
/// inside `run` ends the pollers, because the daemon owns their wake
/// senders.
fn run(config_path: Option<&Path>, socket_path: &Path, paused: bool) -> anyhow::Result<()> {
    let config_path = config::resolved_config_path(config_path)
        .context("cannot resolve the factory config path")?;
    let config_text = fs::read_to_string(&config_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "no config file at {}; run ./install.sh to create it",
                    config_path.display()
                )
            } else {
                anyhow!(error).context(format!("cannot read {}", config_path.display()))
            }
        })
        .context("cannot load the factory config")?;
    let revision = config::file_revision(&config_text);
    let config = Config::parse_resolved(&config_text, &aif::exec::RealExec)
        .context("cannot load the factory config")?;
    let (server, action_rx) = sock::Server::bind(socket_path)?;
    eprintln!("aifd: listening on {}", socket_path.display());
    // One exit path serves every caller: the signal thread turns the first
    // SIGTERM, SIGINT, or SIGHUP into the same Action::Stop that
    // `aif stop` sends over the socket. The socket channel and the signal
    // channel merge into the one receiver the daemon takes.
    let (merged_tx, merged_rx) = mpsc::channel::<Action>();
    install_signal_thread(merged_tx.clone())?;
    let socket_tx = merged_tx;
    std::thread::Builder::new()
        .name("aif-socket".to_string())
        .spawn(move || {
            for action in action_rx {
                if socket_tx.send(action).is_err() {
                    break;
                }
            }
        })
        .context("cannot spawn the socket forwarder")?;
    if paused {
        eprintln!("aifd: the factory starts paused; nothing dispatches until the operator resumes");
    }
    let (poll_tx, poll_rx) = mpsc::channel();
    let pollers = poll::spawn_pollers(&config, poll_tx);
    let mut daemon = Daemon::new(
        config,
        config_path.clone(),
        revision,
        prompts_dir(Some(&config_path)),
        poll_rx,
        pollers.wake,
        merged_rx,
        paused,
    )
    .context("cannot initialize the factory daemon")?;
    // Every dirty drive of the loop hands its state view to the socket
    // server. The Arc clone in the pusher dies with the daemon inside
    // `run`, so this `drop` still stops the server last.
    let server = Arc::new(server);
    daemon.set_pusher(Box::new({
        let server = Arc::clone(&server);
        move |view| server.publish(view)
    }));
    daemon.set_ticket_pusher(Box::new({
        let server = Arc::clone(&server);
        move |push| server.push(push)
    }));
    let result = daemon.run();
    drop(server);
    result
}

/// Install the one signal thread of the daemon.
///
/// The first `SIGTERM`, `SIGINT`, or `SIGHUP` sends [`Action::Stop`] into
/// the daemon's action channel and names the signal on standard error. A
/// later signal writes one line that says the daemon already stops and
/// changes nothing. The thread runs until the process exits.
fn install_signal_thread(stop_tx: mpsc::Sender<Action>) -> anyhow::Result<()> {
    let signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
        .context("cannot register the daemon signal handlers")?;
    std::thread::Builder::new()
        .name("aif-signal".to_string())
        .spawn(move || {
            let mut signals = signals;
            let announced = Arc::new(AtomicBool::new(false));
            for signal in signals.forever() {
                let name = signal_name(signal);
                if !announced.swap(true, Ordering::SeqCst) {
                    eprintln!("aifd: received {name}; the daemon stops its agent sessions");
                    let _ = stop_tx.send(Action::Stop);
                } else {
                    eprintln!("aifd: received {name}; the daemon already stops");
                }
            }
        })
        .context("cannot spawn the signal thread")?;
    Ok(())
}

/// The readable name of one registered signal number.
fn signal_name(signal: i32) -> &'static str {
    match signal {
        SIGTERM => "SIGTERM",
        SIGINT => "SIGINT",
        SIGHUP => "SIGHUP",
        _ => "a signal",
    }
}

/// Select prompt files beside a custom config file.
///
/// The default config uses the standard config directory. A custom config
/// carries its own prompt set, so test and alternate factory setups remain
/// self-contained.
fn prompts_dir(config_path: Option<&Path>) -> PathBuf {
    config_path.and_then(Path::parent).map_or_else(
        || config::config_dir().join("prompts"),
        |dir| dir.join("prompts"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique temporary directory for one test.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aifd-wire-{label}-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the old temp dir must be removable");
        }
        fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }

    #[test]
    fn a_missing_config_file_fails_and_names_the_file() {
        let dir = temp_dir("missing-config");
        let config_path = dir.join("factory.toml");
        let error = run(Some(&config_path), &dir.join("daemon.sock"), false).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot load the factory config"),
            "message: {message}"
        );
        assert!(
            message.contains(&config_path.to_string_lossy().into_owned()),
            "message: {message}"
        );
        assert_eq!(exit_code(Err(error)), 1);
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn the_cli_parses_the_config_option() {
        let parsed = Cli::try_parse_from(["aifd", "run", "--config", "/tmp/factory.toml"])
            .expect("the arguments must parse");
        let Command::Run { config, paused } = parsed.command;
        assert_eq!(config.as_deref(), Some(Path::new("/tmp/factory.toml")));
        assert!(!paused);

        let parsed = Cli::try_parse_from(["aifd", "run"]).expect("the arguments must parse");
        let Command::Run { config, paused } = parsed.command;
        assert_eq!(config, None);
        assert!(!paused);
    }

    #[test]
    fn the_cli_parses_the_paused_flag() {
        let parsed =
            Cli::try_parse_from(["aifd", "run", "--paused"]).expect("the arguments must parse");
        let Command::Run { config, paused } = parsed.command;
        assert!(paused);
        assert_eq!(config, None);
    }

    #[test]
    fn a_custom_config_uses_its_sibling_prompt_directory() {
        assert_eq!(
            prompts_dir(Some(Path::new("/tmp/factory/factory.toml"))),
            PathBuf::from("/tmp/factory/prompts")
        );
        assert_eq!(prompts_dir(None), config::config_dir().join("prompts"));
    }
}
