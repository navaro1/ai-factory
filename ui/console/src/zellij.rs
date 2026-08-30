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
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect())
}

pub fn session_line<'a>(sessions: &'a [String], session: &str) -> Option<&'a String> {
    sessions
        .iter()
        .find(|line| line.split_whitespace().next() == Some(session))
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
