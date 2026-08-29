use std::process::Command;

use anyhow::Result;

pub fn list_sessions() -> Result<Vec<String>> {
    let out = Command::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

pub fn dump_screen(session: &str, pane: &str) -> Option<String> {
    let out = Command::new("zellij")
        .arg("-s")
        .arg(session)
        .args(["action", "dump-screen", "-p", pane])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}
