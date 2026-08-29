use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use aif::{app, status, theme};

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
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
        Some(Command::Start { skip }) => {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new("ai-factory");
            if let Some(skip) = skip {
                cmd.arg("--skip").arg(skip);
            }
            let err = cmd.exec();
            bail!("failed to run ai-factory (run install.sh first): {err}");
        }
        Some(Command::Tokens { command }) => match command {
            TokensCommand::Zellij { tokens, out, check } => {
                render_zellij_theme(&tokens, &out, check)
            }
        },
    }
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
