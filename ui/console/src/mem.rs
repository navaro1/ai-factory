use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub rss_kib: u64,
    pub cmdline: String,
    pub cwd: Option<String>,
}

#[derive(Debug)]
pub struct MemReport {
    pub session: String,
    pub scope_bytes: Option<u64>,
    pub scope_high: Option<String>,
    pub server_rss_kib: Option<u64>,
    pub agents: Vec<ProcInfo>,
    pub agents_rss_kib: u64,
}

pub fn scope_dir_path(session: &str) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self")
        .map(|md| md.uid())
        .unwrap_or(1000);
    PathBuf::from("/sys/fs/cgroup")
        .join("user.slice")
        .join(format!("user-{uid}.slice"))
        .join(format!("user@{uid}.service"))
        .join("app.slice")
        .join(format!("aif-{session}.scope"))
}

pub fn scope_dir(session: &str) -> Option<PathBuf> {
    let dir = scope_dir_path(session);
    dir.exists().then_some(dir)
}

pub fn read_scope(session: &str) -> (Option<u64>, Option<String>) {
    let Some(dir) = scope_dir(session) else {
        return (None, None);
    };
    let current = std::fs::read_to_string(dir.join("memory.current"))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let high = std::fs::read_to_string(dir.join("memory.high"))
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| raw != "max");
    (current, high)
}

pub fn snapshot_procs() -> Vec<ProcInfo> {
    let mut procs = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return procs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.parse::<u32>().ok() else {
            continue;
        };
        let Ok(statm) = std::fs::read_to_string(entry.path().join("statm")) else {
            continue;
        };
        let Some(resident_pages) = statm.split_whitespace().nth(1) else {
            continue;
        };
        let Ok(pages) = resident_pages.parse::<u64>() else {
            continue;
        };
        let Ok(cmdline_raw) = std::fs::read_to_string(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = cmdline_raw.replace('\0', " ").trim().to_owned();
        if cmdline.is_empty() {
            continue;
        }
        let cwd = std::fs::read_link(entry.path().join("cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        procs.push(ProcInfo {
            pid,
            rss_kib: pages.saturating_mul(4),
            cmdline,
            cwd,
        });
    }
    procs
}

pub fn collect(root: &Path, session: &str) -> MemReport {
    let (scope_bytes, scope_high) = read_scope(session);
    let mut server_rss_kib = None;
    let mut agents = Vec::new();
    for proc in snapshot_procs() {
        if proc.cmdline.contains("zellij") && proc.cmdline.contains("--server") {
            if proc.cmdline.contains(session) {
                server_rss_kib = Some(proc.rss_kib);
            }
            continue;
        }
        let is_agent = (proc.cmdline.contains("codex") && !proc.cmdline.contains("codexd"))
            || proc.cmdline.contains("opencode --auto")
            || (proc.cmdline.contains("claude") && proc.cmdline.contains("--model"));
        if !is_agent {
            continue;
        }
        if proc.cwd.as_deref() == Some(root.to_str().unwrap_or("")) {
            agents.push(proc);
        }
    }
    agents.sort_by_key(|p| std::cmp::Reverse(p.rss_kib));
    let agents_rss_kib = agents.iter().map(|p| p.rss_kib).sum();
    MemReport {
        session: session.to_owned(),
        scope_bytes,
        scope_high,
        server_rss_kib,
        agents,
        agents_rss_kib,
    }
}

pub fn human_kib(kib: u64) -> String {
    let mib = kib as f64 / 1024.0;
    if mib >= 1024.0 {
        format!("{gib:.1} GiB", gib = mib / 1024.0)
    } else {
        format!("{mib:.1} MiB")
    }
}

pub fn human_bytes(bytes: u64) -> String {
    human_kib(bytes / 1024)
}

impl MemReport {
    pub fn render_text(&self) -> String {
        let mut out = format!("factory {} (memory)\n", self.session);
        match (self.scope_bytes, &self.scope_high) {
            (Some(bytes), high) => {
                out.push_str(&format!(
                    "  scope aif-{}: {} (memory.high: {})\n",
                    self.session,
                    human_bytes(bytes),
                    high.clone().unwrap_or_else(|| "unset".into())
                ));
            }
            (None, _) => out.push_str(
                "  scope aif-<session> not found; session predates isolation or systemd is absent\n",
            ),
        }
        if let Some(server) = self.server_rss_kib {
            out.push_str(&format!("  zellij server: {}\n", human_kib(server)));
        }
        for agent in &self.agents {
            let kind = if agent.cmdline.contains("codex") {
                "codex"
            } else if agent.cmdline.contains("opencode") {
                "opencode"
            } else {
                "claude"
            };
            let model = agent
                .cmdline
                .split("--model")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or("-");
            out.push_str(&format!(
                "  {:>7}  {:<8} {:<28} pid {}\n",
                human_kib(agent.rss_kib),
                kind,
                model,
                agent.pid
            ));
        }
        out.push_str(&format!(
            "  agents total: {} across {} processes\n",
            human_kib(self.agents_rss_kib),
            self.agents.len()
        ));
        out
    }
}

pub fn top(root: &Path) -> Result<String> {
    let session = crate::status::session_name(root);
    Ok(collect(root, &session).render_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human_kib(512 * 1024), "512.0 MiB");
        assert_eq!(human_kib(3 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn scope_dir_shape() {
        let dir = scope_dir_path("borsuk-factory");
        let text = dir.to_string_lossy().to_string();
        assert!(text.ends_with("app.slice/aif-borsuk-factory.scope"));
        assert!(text.contains("user-"));
    }

    #[test]
    fn snapshot_includes_self() {
        let procs = snapshot_procs();
        assert!(procs.iter().any(|p| p.cmdline.contains("aif")));
    }
}
