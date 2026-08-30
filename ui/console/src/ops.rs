use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::control::{self, Reply};
use crate::factory::{self, FactoryPaths};

pub fn trust(root: &Path) -> Result<()> {
    let paths = FactoryPaths::open(root)?;
    paths.ensure()?;
    let graph = crate::graph::Graph::load(&root.join(crate::graph::DEFAULT_GRAPH_PATH))?;
    let auto: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.exec == crate::graph::Exec::Auto)
        .map(|n| n.name.as_str())
        .collect();
    let remote = std::process::Command::new("git")
        .current_dir(root)
        .args(["remote", "get-url", "origin"])
        .output();
    let remote = remote
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "(no origin remote)".into());
    println!("repository: {}", root.display());
    println!("remote:     {remote}");
    println!("factory:    {}", paths.factory_id);
    if auto.is_empty() {
        println!("auto nodes: none; trust is not required");
        return Ok(());
    }
    println!("auto nodes: {}", auto.join(", "));
    println!();
    println!("Warning: polling cannot verify which collaborator changed a label.");
    println!("Any collaborator who can set a matching label can trigger automatic");
    println!("full-access execution on this machine.");
    println!();
    print!("type the factory id {} to trust it: ", paths.factory_id);
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != paths.factory_id {
        bail!("answer does not match the factory id; trust not recorded");
    }
    paths.write_trust(true)?;
    println!("trust recorded in {}", paths.trust().display());
    Ok(())
}

pub fn doctor(root: &Path) -> Result<()> {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        let mark = if ok { "ok" } else { "FAIL" };
        if !ok {
            failures += 1;
        }
        println!("[{mark}] {name}: {detail}");
    };
    let graph_path = root.join(crate::graph::DEFAULT_GRAPH_PATH);
    match crate::graph::Graph::load(&graph_path) {
        Ok(graph) => check(
            "graph",
            true,
            format!(
                "v{}, {} nodes, limit {}",
                graph.version,
                graph.nodes.len(),
                graph.limit
            ),
        ),
        Err(err) => check("graph", false, format!("{err:#}")),
    }
    match crate::factory::git_common_dir(root) {
        Ok(common) => check("git", true, common.display().to_string()),
        Err(err) => check("git", false, format!("{err:#}")),
    }
    match FactoryPaths::open(root) {
        Ok(paths) => {
            check(
                "factory",
                true,
                format!("id {} at {}", paths.factory_id, paths.state.display()),
            );
            let alive = factory::socket_alive(&paths.socket());
            check("daemon", true, format!("socket alive: {alive}"));
            if alive {
                match control::request(&paths.socket(), "ping", serde_json::json!({})) {
                    Ok(reply) => check(
                        "daemon rpc",
                        reply.ok,
                        format!("revision {}", reply.revision),
                    ),
                    Err(err) => check("daemon rpc", false, format!("{err:#}")),
                }
            }
        }
        Err(err) => check("factory", false, format!("{err:#}")),
    }
    for tool in ["gh", "git", "zellij", "claude", "opencode", "codex"] {
        let found = which(tool).is_some();
        check(
            "binary",
            found,
            if found {
                tool.to_owned()
            } else {
                format!("{tool} missing")
            },
        );
    }
    check(
        "codex protocol",
        crate::codex::check_codex_version(crate::codex::discover_native_codex().as_deref()).is_ok(),
        format!("supported {}", crate::codex::SUPPORTED_VERSIONS),
    );
    check(
        "opencode protocol",
        crate::ocserve::check_oc_version().is_ok(),
        format!("supported {}", crate::ocserve::SUPPORTED_VERSIONS),
    );
    check(
        "systemd",
        std::process::Command::new("systemd-run")
            .arg("--version")
            .output()
            .is_ok(),
        "transient user services".to_owned(),
    );
    if failures > 0 {
        bail!("{failures} doctor checks failed");
    }
    Ok(())
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn list() -> Result<()> {
    for (id, dir) in factory::list_factories() {
        let meta = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        let repo = meta
            .as_ref()
            .and_then(|m| m.get("repo"))
            .and_then(|r| r.as_str())
            .unwrap_or("?");
        let paths = FactoryPaths::from_id(Path::new("."), &id);
        let alive = factory::socket_alive(&paths.socket());
        println!("{id}  {alive:<5}  {repo}");
    }
    Ok(())
}

pub fn cleanup(root: &Path) -> Result<()> {
    let paths = FactoryPaths::open(root)?;
    let worktrees = paths.worktrees_dir();
    let entries = std::fs::read_dir(&worktrees).with_context(|| worktrees.display().to_string())?;
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let clean = std::process::Command::new("git")
            .args([
                "-C",
                path.to_string_lossy().as_ref(),
                "status",
                "--porcelain",
            ])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        if !clean {
            println!("keep dirty: {}", path.display());
            continue;
        }
        println!("remove:    {}", path.display());
        let out = std::process::Command::new("git")
            .current_dir(&paths.root)
            .args(["worktree", "remove", path.to_string_lossy().as_ref()])
            .output()?;
        if out.status.success() {
            removed.push(path);
        } else {
            println!(
                "  worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    println!("removed {} clean worktree(s)", removed.len());
    Ok(())
}

pub fn task_log(root: &Path, task: &str) -> Result<()> {
    let paths = FactoryPaths::open(root)?;
    let log = paths
        .logs_dir()
        .join(format!("{}.log", crate::ids::sanitize_component(task)));
    let text = std::fs::read_to_string(&log).unwrap_or_else(|_| "(no log)".into());
    print!("{text}");
    Ok(())
}

pub fn daemon_log(root: &Path) -> Result<()> {
    let paths = FactoryPaths::open(root)?;
    let log = paths.logs_dir().join("daemon.log");
    let text = std::fs::read_to_string(&log).unwrap_or_else(|_| "(no log)".into());
    print!("{text}");
    Ok(())
}

pub fn ensure_daemon(paths: &FactoryPaths, root: &Path) -> Result<()> {
    if factory::socket_alive(&paths.socket()) {
        return Ok(());
    }
    let unit = format!("aif-{}", paths.short_id());
    let systemd = std::process::Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let bin = std::env::current_exe()?;
    if systemd {
        let out = std::process::Command::new("systemd-run")
            .args(["--user", "--collect", "--unit", &unit])
            .arg("--same-dir")
            .arg(bin.display().to_string())
            .arg("daemon")
            .current_dir(root)
            .output()
            .context("systemd-run failed")?;
        if !out.status.success() {
            bail!(
                "systemd-run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    } else {
        let log_file = paths.logs_dir().join("daemon-fg.log");
        let out = std::process::Command::new(bin)
            .arg("daemon")
            .current_dir(root)
            .stdout(std::fs::File::create(&log_file)?)
            .stderr(std::fs::OpenOptions::new().append(true).open(&log_file)?)
            .spawn()
            .context("failed to spawn daemon")?;
        println!("daemon started without systemd, pid {}", out.id());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if factory::socket_alive(&paths.socket()) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("daemon did not become ready within 5s");
}

pub fn stop_daemon(paths: &FactoryPaths, force: bool) -> Result<()> {
    if !factory::socket_alive(&paths.socket()) {
        println!("daemon is not running");
        return Ok(());
    }
    let reply = control::request(
        &paths.socket(),
        "stop",
        serde_json::json!({ "force": force }),
    )?;
    if !reply.ok {
        bail!("{}", reply.error.map(|e| e.message).unwrap_or_default());
    }
    println!("daemon stopping");
    Ok(())
}

pub fn print_reply(reply: &Reply) -> Result<()> {
    if !reply.ok {
        bail!(
            "{}: {}",
            reply
                .error
                .as_ref()
                .map(|e| e.code.clone())
                .unwrap_or_default(),
            reply
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_default()
        );
    }
    if let Some(result) = &reply.result {
        println!("{}", serde_json::to_string_pretty(result)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn which_finds_ls() {
        assert!(super::which("ls").is_some());
        assert!(super::which("definitely-not-a-tool-xyz").is_none());
    }
}
