use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use aif::theme;

#[derive(Parser)]
#[command(
    name = "aif",
    version,
    about = "Console and graph engine for the ai-factory workspace"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the factory session and pane map
    Status,
    /// Open the cockpit TUI
    Tui,
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
        Command::Status => {
            bail!("status arrives in v0.2.0 chunk 2")
        }
        Command::Tui => {
            bail!("the cockpit arrives in v0.2.0 chunk 3")
        }
        Command::Tokens { command } => match command {
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
