use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use aif::{app, graph, runner, status, theme};

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
    /// Start the factory session through the ai-factory script
    Start {
        /// Panes or tabs to skip, comma separated
        #[arg(long)]
        skip: Option<String>,
    },
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
    },
}

#[derive(Subcommand)]
enum GraphCommand {
    /// Parse and validate .aif/graph.kdl
    Validate {
        /// Path to the graph file
        #[arg(long, default_value = graph::DEFAULT_GRAPH_PATH)]
        path: PathBuf,
    },
    /// Print the graph as Graphviz dot
    Dot {
        /// Path to the graph file
        #[arg(long, default_value = graph::DEFAULT_GRAPH_PATH)]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum TokensCommand {
    /// Render the zellij theme file
    Zellij {
        /// Path to tokens.json
        #[arg(long, default_value = "ui/tokens/tokens.json")]
        tokens: PathBuf,
        /// Path to the generated zellij theme
        #[arg(long, default_value = "zellij/themes/retro-future.kdl")]
        out: PathBuf,
        /// Fail when the file on disk differs from the generated theme
        #[arg(long)]
        check: bool,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("aif: {err:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command;
    match command {
        None | Some(Command::Tui) => app::run(),
        Some(Command::Status { json }) => {
            let report = status::report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_text());
            }
            Ok(())
        }
        Some(Command::Start { skip }) => start_factory(skip.as_deref()),
        Some(Command::Tokens { command }) => match command {
            TokensCommand::Zellij { tokens, out, check } => {
                render_zellij_theme(&tokens, &out, check)
            }
        },
        Some(Command::Run { once, dry_run }) => {
            let root = status::repo_root()?;
            if once || dry_run {
                runner::run_once(&root, dry_run)
            } else {
                runner::run_loop(&root)
            }
        }
        Some(Command::Events { last }) => {
            let root = status::repo_root()?;
            runner::print_events(&root, last)
        }
        Some(Command::Graph { command }) => match command {
            GraphCommand::Validate { path } => {
                let graph = graph::Graph::load(&path)?;
                println!(
                    "ok: {} nodes, {} edges, tick {}s, limit {}",
                    graph.nodes.len(),
                    graph.edges.len(),
                    graph.tick_secs,
                    graph.limit
                );
                for node in &graph.nodes {
                    let when = node
                        .when
                        .as_ref()
                        .map(|c| c.render())
                        .unwrap_or_else(|| "manual".into());
                    println!(
                        "  {} · {} · {} · {} · when: {}",
                        node.name,
                        node.agent.as_str(),
                        node.model,
                        node.exec.as_str(),
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
        },
    }
}

fn start_factory(skip: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let root = status::repo_root()?;
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let session = format!("{repo}-factory");

    for line in aif::zellij::list_sessions()? {
        if line.split_whitespace().next() == Some(session.as_str()) {
            if line.contains("EXITED") {
                eprintln!("aif: session {session} is dead; deleting it and starting fresh");
                let out = std::process::Command::new("zellij")
                    .args(["delete-session", &session])
                    .output()?;
                if !out.status.success() {
                    bail!("failed to delete dead session {session}");
                }
            } else {
                eprintln!("aif: session {session} already runs; attaching");
                let err = std::process::Command::new("zellij")
                    .args(["attach", &session])
                    .exec();
                bail!("failed to attach: {err}");
            }
        }
    }
    let skip_items: Vec<String> = skip
        .map(|raw| raw.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let zdir = std::env::var("ZELLIJ_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    PathBuf::from(home).join(".config")
                })
                .join("zellij")
        });
    let template = aif::layout::installed_template()?;
    let rendered =
        aif::layout::render(&template, &root, &repo, &zdir.join("prompts"), &skip_items)?;

    let runtime_base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let registry = runtime_base.join("aif-registry").join(&session);
    let _ = std::fs::remove_dir_all(&registry);

    let layout_file =
        std::env::temp_dir().join(format!("aif-{session}-{}.kdl", std::process::id()));
    std::fs::write(&layout_file, &rendered)?;

    let layout_arg = layout_file.display().to_string();
    let isolated = std::process::Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    let err = if isolated {
        eprintln!("aif: starting {session} in its own user scope aif-{session}");
        std::process::Command::new("systemd-run")
            .args(["--user", "--scope"])
            .arg(format!("--unit=aif-{session}"))
            .args([
                "zellij",
                "--new-session-with-layout",
                &layout_arg,
                "--session",
                &session,
            ])
            .exec()
    } else {
        std::process::Command::new("zellij")
            .arg("--new-session-with-layout")
            .arg(&layout_file)
            .arg("--session")
            .arg(&session)
            .exec()
    };
    bail!("failed to start zellij: {err}");
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
