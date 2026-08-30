use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use aif::{app, cockpit, graph, ops, runner, status, theme};

#[derive(Parser)]
#[command(
    name = "aif",
    version,
    about = "Console and graph engine for the ai-factory workspace"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the factory session and pane map
    Status {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Open the cockpit TUI (default)
    Tui,
    /// Start the factory session (v3 panes or v4 daemon plus lean UI)
    Start {
        /// Panes or tabs to skip, comma separated (v3 only)
        #[arg(long)]
        skip: Option<String>,
        /// Start the v4 UI without attaching to it
        #[arg(long)]
        detach: bool,
    },
    /// Stop the running v4 daemon
    Stop {
        /// Cancel active work first
        #[arg(long)]
        force: bool,
    },
    /// Restart the v4 daemon after graph or binary changes
    Restart {
        /// Cancel active work first
        #[arg(long)]
        force: bool,
    },
    /// Pause new dispatches globally or for one node
    Pause {
        /// Node name; omit for the whole factory
        #[arg(long)]
        node: Option<String>,
    },
    /// Resume paused dispatches
    Resume {
        /// Node name; omit for the whole factory
        #[arg(long)]
        node: Option<String>,
    },
    /// Run the v4 daemon in the foreground (v3 keeps the tick loop)
    Daemon,
    /// Task operations over the control socket
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Approve automatic full-access execution for this repository
    Trust,
    /// Check binaries, protocols, graph, and daemon health
    Doctor,
    /// List local factories
    List,
    /// Remove clean worktrees of terminal tasks
    Cleanup,
    /// Show a task log or the daemon log
    Logs { task: Option<String> },
    /// Render theme files from tokens.json
    Tokens {
        #[command(subcommand)]
        command: TokensCommand,
    },
    /// Inspect the repo graph file
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },
    /// Evaluate the graph and dispatch ready tasks
    Run {
        /// Run one tick and exit
        #[arg(long)]
        once: bool,
        /// Print the dispatch plan without touching anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the event log
    Events {
        /// Show only the last N events
        #[arg(long, default_value_t = 20)]
        last: usize,
        /// Follow new events (v4)
        #[arg(long)]
        follow: bool,
    },
    /// Show factory memory usage per process and scope
    Top,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Submit supervised work in its pane
    Submit { task: String },
    /// Request harness cancellation
    Cancel { task: String },
    /// Retry a terminal task as the next attempt
    Retry { task: String },
    /// Resolve an uncertain task
    Resolve { task: String, outcome: String },
    /// Clear stale supervised presentation state
    Dismiss { task: String },
    /// Mark supervised work completed
    Complete { task: String },
    /// Mark supervised work failed
    Fail { task: String },
}

#[derive(Subcommand)]
enum GraphCommand {
    /// Parse and validate .aif/graph.kdl
    Validate {
        #[arg(long, default_value = graph::DEFAULT_GRAPH_PATH)]
        path: PathBuf,
    },
    /// Print the graph as Graphviz dot
    Dot {
        #[arg(long, default_value = graph::DEFAULT_GRAPH_PATH)]
        path: PathBuf,
    },
    /// Write a starter .aif/graph.kdl for this repo
    Init {
        #[arg(long, default_value = graph::DEFAULT_GRAPH_PATH)]
        path: PathBuf,
        #[arg(long, default_value = "~/.config/zellij/prompts")]
        prompts: PathBuf,
    },
    /// Preview or apply a v3 to v4 migration
    Migrate {
        #[arg(long)]
        write: bool,
        #[arg(long)]
        auto_workers: bool,
    },
}

#[derive(Subcommand)]
enum TokensCommand {
    /// Render the zellij theme file
    Zellij {
        #[arg(long, default_value = "ui/tokens/tokens.json")]
        tokens: PathBuf,
        #[arg(long, default_value = "zellij/themes/retro-future.kdl")]
        out: PathBuf,
        #[arg(long)]
        check: bool,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("aif: {err:#}");
        std::process::exit(1);
    }
}

fn graph_version(root: &Path) -> u32 {
    graph::Graph::load(&root.join(graph::DEFAULT_GRAPH_PATH))
        .map(|g| g.version)
        .unwrap_or(3)
}

fn v4_paths(root: &Path) -> Result<aif::factory::FactoryPaths> {
    aif::factory::FactoryPaths::open(root)
}

fn rpc(root: &Path, method: &str, params: serde_json::Value) -> Result<aif::control::Reply> {
    let paths = v4_paths(root)?;
    aif::control::request(&paths.socket(), method, params)
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command;
    match command {
        None | Some(Command::Tui) => {
            let root = status::repo_root()?;
            if graph_version(&root) >= 4 {
                cockpit::run(&root)
            } else {
                app::run()
            }
        }
        Some(Command::Status { json }) => {
            let root = status::repo_root()?;
            if graph_version(&root) >= 4 {
                let reply = rpc(&root, "status", serde_json::json!({}))?;
                if !reply.ok {
                    bail!(
                        "daemon unreachable: {}",
                        reply.error.map(|e| e.message).unwrap_or_default()
                    );
                }
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&reply.result.unwrap_or_default())?
                    );
                } else {
                    cockpit::print_status(&reply.result.unwrap_or_default());
                }
                return Ok(());
            }
            let report = status::report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_text());
            }
            Ok(())
        }
        Some(Command::Start { skip, detach }) => start_factory(skip.as_deref(), detach),
        Some(Command::Stop { force }) => {
            let root = status::repo_root()?;
            ops::stop_daemon(&v4_paths(&root)?, force)
        }
        Some(Command::Restart { force }) => {
            let root = status::repo_root()?;
            let paths = v4_paths(&root)?;
            ops::stop_daemon(&paths, force)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while aif::factory::socket_alive(&paths.socket())
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            ops::ensure_daemon(&paths, &root)
        }
        Some(Command::Pause { node }) => {
            let root = status::repo_root()?;
            let params = match &node {
                Some(node) => serde_json::json!({ "node": node }),
                None => serde_json::json!({}),
            };
            ops::print_reply(&rpc(&root, "pause", params)?)
        }
        Some(Command::Resume { node }) => {
            let root = status::repo_root()?;
            let params = match &node {
                Some(node) => serde_json::json!({ "node": node }),
                None => serde_json::json!({}),
            };
            ops::print_reply(&rpc(&root, "resume", params)?)
        }
        Some(Command::Daemon) => {
            let root = status::repo_root()?;
            let graph = graph::Graph::load(&root.join(graph::DEFAULT_GRAPH_PATH))?;
            if graph.version < 4 {
                bail!("`aif daemon` needs graph version=4; run `aif graph migrate`");
            }
            let paths = v4_paths(&root)?;
            aif::daemon::run(paths, graph, true)
        }
        Some(Command::Task { command }) => {
            let root = status::repo_root()?;
            let (method, params) = match command {
                TaskCommand::Submit { task } => ("task.submit", serde_json::json!({"task": task})),
                TaskCommand::Cancel { task } => ("task.cancel", serde_json::json!({"task": task})),
                TaskCommand::Retry { task } => ("task.retry", serde_json::json!({"task": task})),
                TaskCommand::Resolve { task, outcome } => (
                    "task.resolve",
                    serde_json::json!({"task": task, "outcome": outcome}),
                ),
                TaskCommand::Dismiss { task } => {
                    ("task.dismiss", serde_json::json!({"task": task}))
                }
                TaskCommand::Complete { task } => {
                    ("task.complete", serde_json::json!({"task": task}))
                }
                TaskCommand::Fail { task } => ("task.fail", serde_json::json!({"task": task})),
            };
            ops::print_reply(&rpc(&root, method, params)?)
        }
        Some(Command::Trust) => {
            let root = status::repo_root()?;
            ops::trust(&root)
        }
        Some(Command::Doctor) => {
            let root = status::repo_root()?;
            ops::doctor(&root)
        }
        Some(Command::List) => ops::list(),
        Some(Command::Cleanup) => {
            let root = status::repo_root()?;
            ops::cleanup(&root)
        }
        Some(Command::Logs { task }) => {
            let root = status::repo_root()?;
            match task {
                Some(task) => ops::task_log(&root, &task),
                None => ops::daemon_log(&root),
            }
        }
        Some(Command::Tokens { command }) => match command {
            TokensCommand::Zellij { tokens, out, check } => {
                render_zellij_theme(&tokens, &out, check)
            }
        },
        Some(Command::Graph { command }) => match command {
            GraphCommand::Validate { path } => {
                let graph = graph::Graph::load(&path)?;
                println!(
                    "ok: v{} graph, {} nodes, {} edges, limit {}",
                    graph.version,
                    graph.nodes.len(),
                    graph.edges.len(),
                    graph.limit
                );
                if graph.version >= 4 {
                    println!("tick {}s is unused by the v4 daemon", graph.tick_secs);
                }
                for node in &graph.nodes {
                    let when = node
                        .when
                        .as_ref()
                        .map(|c| c.render())
                        .unwrap_or_else(|| "manual".into());
                    println!(
                        "  {} · {} · {} · {} · limit {} · retrigger {} · when: {}",
                        node.name,
                        node.agent.as_str(),
                        node.model,
                        node.exec.as_str(),
                        node.limit.unwrap_or(1),
                        node.retrigger.as_str(),
                        when
                    );
                }
                Ok(())
            }
            GraphCommand::Dot { path } => {
                let graph = graph::Graph::load(&path)?;
                print!("{}", graph.to_dot());
                Ok(())
            }
            GraphCommand::Init { path, prompts } => {
                let root = status::repo_root()?;
                let path = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                if path.exists() {
                    bail!("{} already exists; refusing to overwrite", path.display());
                }
                let prompts = if prompts.is_absolute() {
                    prompts
                } else {
                    let home = std::env::var("HOME").unwrap_or_default();
                    PathBuf::from(prompts.to_string_lossy().replacen('~', &home, 1))
                };
                let text = graph::Graph::template(&prompts);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, text)?;
                println!("wrote {}", path.display());
                println!("validate it with: aif graph validate");
                println!("then run the loop with: aif run");
                Ok(())
            }
            GraphCommand::Migrate {
                write,
                auto_workers,
            } => {
                let root = status::repo_root()?;
                aif::migrate::migrate(&root, write, auto_workers)
            }
        },
        Some(Command::Run { once, dry_run }) => {
            let root = status::repo_root()?;
            if graph_version(&root) >= 4 {
                return run_v4(&root, once);
            }
            if once || dry_run {
                runner::run_once(&root, dry_run)
            } else {
                runner::run_loop(&root)
            }
        }
        Some(Command::Events { last, follow }) => {
            let root = status::repo_root()?;
            if graph_version(&root) >= 4 {
                return events_v4(&root, last, follow);
            }
            runner::print_events(&root, last)
        }
        Some(Command::Top) => {
            let root = status::repo_root()?;
            print!("{}", aif::mem::top(&root)?);
            Ok(())
        }
    }
}

fn run_v4(root: &Path, once: bool) -> Result<()> {
    let graph = graph::Graph::load(&root.join(graph::DEFAULT_GRAPH_PATH))?;
    if graph.version < 4 {
        bail!("this graph is v3; aif run keeps the tick loop");
    }
    let paths = v4_paths(root)?;
    if once {
        if aif::factory::socket_alive(&paths.socket()) {
            let reply = aif::control::request(&paths.socket(), "reconcile", serde_json::json!({}))?;
            return ops::print_reply(&reply);
        }
        return aif::daemon::run(paths, graph, false);
    }
    aif::daemon::run(paths, graph, true)
}

fn events_v4(root: &Path, last: usize, follow: bool) -> Result<()> {
    let paths = v4_paths(root)?;
    let records = aif::journal::Journal::replay(&paths.journal())?;
    let start = records.len().saturating_sub(last);
    for record in &records[start..] {
        let line = serde_json::to_string(record)?;
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    if !aif::factory::socket_alive(&paths.socket()) {
        eprintln!("aif: daemon is not running; printed the journal tail only");
        return Ok(());
    }
    use std::io::BufRead;
    let mut stream = std::os::unix::net::UnixStream::connect(paths.socket())?;
    let envelope = serde_json::json!({
        "v": 1,
        "id": aif::ids::new_id(),
        "method": "events.follow",
        "params": {},
    });
    use std::io::Write;
    let mut line = serde_json::to_string(&envelope)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let reader = std::io::BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("event stream closed")?;
        if line.starts_with('{') && line.contains("\"in_reply_to\"") {
            continue;
        }
        println!("{line}");
    }
    Ok(())
}

fn start_factory(skip: Option<&str>, detach: bool) -> Result<()> {
    let root = status::repo_root()?;
    let graph = graph::Graph::load(&root.join(graph::DEFAULT_GRAPH_PATH)).ok();
    if graph.as_ref().map(|g| g.version).unwrap_or(3) >= 4 {
        let graph = graph.unwrap();
        let paths = v4_paths(&root)?;
        paths.ensure()?;
        ops::ensure_daemon(&paths, &root)?;
        println!("aif: daemon ready at {}", paths.socket().display());
        return start_v4_ui(&root, &paths, &graph, detach);
    }
    if detach {
        bail!("--detach requires a v4 graph");
    }
    aif::legacy::start_v3(skip)
}

fn start_v4_ui(
    root: &Path,
    paths: &aif::factory::FactoryPaths,
    graph: &graph::Graph,
    detach: bool,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let session = format!("aif-{}-factory", paths.short_id());
    for line in aif::zellij::list_sessions()? {
        if line.split_whitespace().next() == Some(session.as_str()) {
            if line.contains("EXITED") {
                let _ = std::process::Command::new("zellij")
                    .args(["delete-session", &session])
                    .output();
            } else {
                if detach {
                    println!("aif: session {session} already runs");
                    return Ok(());
                }
                eprintln!("aif: session {session} already runs; attaching");
                let err = std::process::Command::new("zellij")
                    .args(["attach", &session])
                    .exec();
                bail!("failed to attach: {err}");
            }
        }
    }
    if detach {
        return start_v4_ui_detached(root, &session, graph);
    }
    let layout = aif::layout::render_v4(graph, root)?;
    let layout_file =
        std::env::temp_dir().join(format!("aif-{session}-{}.kdl", std::process::id()));
    std::fs::write(&layout_file, layout)?;
    let err = std::process::Command::new("zellij")
        .arg("--new-session-with-layout")
        .arg(&layout_file)
        .arg("--session")
        .arg(&session)
        .exec();
    bail!("failed to start zellij: {err}");
}

fn start_v4_ui_detached(root: &Path, session: &str, graph: &graph::Graph) -> Result<()> {
    let status = std::process::Command::new("zellij")
        .args(["attach", "--create-background", "--create", session])
        .current_dir(root)
        .status()
        .context("failed to start detached zellij session")?;
    if !status.success() {
        bail!("zellij failed to start detached session {session}");
    }

    let configured = (|| -> Result<()> {
        zellij_action(session, &["rename-tab", "--tab-id", "0", "Cockpit"])?;
        zellij_action(
            session,
            &[
                "new-pane",
                "--in-place",
                "--close-replaced-pane",
                "--pane-id",
                "terminal_0",
                "--name",
                "Cockpit",
                "--cwd",
                root.to_string_lossy().as_ref(),
                "--",
                "aif",
                "tui",
            ],
        )?;
        let _ = zellij_action(session, &["close-pane", "--pane-id", "plugin_3"]);

        for node in graph
            .nodes
            .iter()
            .filter(|node| node.exec == graph::Exec::Supervised)
        {
            let mut chars = node.name.chars();
            let role = chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default();
            let tab = zellij_action(
                session,
                &[
                    "new-tab",
                    "--name",
                    &role,
                    "--cwd",
                    root.to_string_lossy().as_ref(),
                ],
            )?;
            zellij_action(
                session,
                &[
                    "new-pane",
                    "--tab-id",
                    &tab,
                    "--name",
                    &role,
                    "--cwd",
                    root.to_string_lossy().as_ref(),
                    "--",
                    "clauded",
                    "--role",
                    &role,
                    "--model",
                    &node.model,
                ],
            )?;
        }
        Ok(())
    })();
    if let Err(err) = configured {
        let _ = std::process::Command::new("zellij")
            .args(["kill-session", session])
            .status();
        return Err(err);
    }
    println!("aif: session {session} started in the background");
    Ok(())
}

fn zellij_action(session: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("zellij")
        .arg("--session")
        .arg(session)
        .arg("action")
        .args(args)
        .output()
        .context("failed to run zellij action")?;
    if !output.status.success() {
        bail!(
            "zellij action failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn render_zellij_theme(tokens_path: &Path, out: &Path, check: bool) -> Result<()> {
    let tokens = theme::Tokens::load(tokens_path)?;
    let rendered = tokens.zellij_kdl()?;

    if check {
        let current = std::fs::read_to_string(out)
            .with_context(|| format!("failed to read {}", out.display()))?;
        if current != rendered {
            bail!(
                "{} is out of date; run `aif tokens zellij` to regenerate it",
                out.display()
            );
        }
        println!("{} matches tokens.json", out.display());
        return Ok(());
    }

    std::fs::write(out, &rendered).with_context(|| format!("failed to write {}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
}
