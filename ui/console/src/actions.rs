use std::process::Command;

use anyhow::{bail, Result};

fn run_zellij(session: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("zellij")
        .arg("-s")
        .arg(session)
        .args(args)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        bail!("zellij {} failed: {err}", args.join(" "));
    }
    Ok(out)
}

pub fn press_enter(session: &str, pane: &str) -> Result<()> {
    run_zellij(session, &["action", "write-chars", "-p", pane, "\r"])?;
    Ok(())
}

pub fn paste_text(session: &str, pane: &str, text: &str) -> Result<()> {
    let payload = format!("\x1b[200~{text}\x1b[201~");
    run_zellij(session, &["action", "write-chars", "-p", pane, &payload])?;
    Ok(())
}

pub fn dump_scrollback(session: &str, pane: &str) -> Result<String> {
    let out = run_zellij(session, &["action", "dump-screen", "-f", "-p", pane])?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
