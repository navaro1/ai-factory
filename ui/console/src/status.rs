use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::zellij;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Classification {
    pub role: String,
    pub agent: String,
    pub model: String,
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct PaneStatus {
    pub pane: String,
    #[serde(flatten)]
    pub class: Classification,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub session: String,
    pub running: bool,
    pub panes: Vec<PaneStatus>,
}

pub fn repo_root() -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            Ok(PathBuf::from(path))
        }
        _ => Ok(PathBuf::from(".")),
    }
}

pub fn session_name(root: &std::path::Path) -> String {
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_owned());
    format!("{repo}-factory")
}

pub fn registry_dir(session: &str) -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(base).join("aif-registry").join(session)
}

pub fn registry_roles(session: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(registry_dir(session)) else {
        return map;
    };
    for entry in entries.flatten() {
        let pane = entry.file_name().to_string_lossy().into_owned();
        if let Ok(role) = std::fs::read_to_string(entry.path()) {
            map.insert(pane.trim().to_owned(), role.trim().to_owned());
        }
    }
    map
}

fn registry_key(pane: &str) -> &str {
    pane.strip_prefix("terminal_").unwrap_or(pane)
}

pub fn report() -> Result<StatusReport> {
    let root = repo_root()?;
    let session = session_name(&root);
    let sessions = zellij::list_sessions()?;
    let running = sessions.iter().any(|s| s == &session);
    if !running {
        bail!("session {session} is not running; start it with: ai-factory");
    }
    let registry = registry_roles(&session);
    let panes = probe_panes(&session, &registry);
    Ok(StatusReport {
        session,
        running,
        panes,
    })
}

fn probe_panes(session: &str, registry: &HashMap<String, String>) -> Vec<PaneStatus> {
    let mut panes = Vec::new();
    let mut misses = 0;
    for id in 0..=24u32 {
        if panes.len() >= 16 || misses >= 4 {
            break;
        }
        let pane = format!("terminal_{id}");
        match zellij::dump_screen(session, &pane) {
            Some(content) if !content.trim().is_empty() => {
                misses = 0;
                let class = classify(&content);
                if class.is_agent_pane() {
                    let role = registry
                        .get(registry_key(&pane))
                        .map(|r| resolve_role(r, &class))
                        .unwrap_or_else(|| class.role.clone());
                    panes.push(PaneStatus {
                        pane,
                        class: Classification { role, ..class },
                    });
                }
            }
            _ => misses += 1,
        }
    }
    panes
}

pub fn resolve_role(registry_role: &str, class: &Classification) -> String {
    let trusted = match registry_role {
        "Planner" => class.agent == "claude" && class.model_marker().contains("Fable 5"),
        "Releaser" => class.agent == "claude" && class.model_marker().contains("Opus 5"),
        "Refiner" | "Reviewer" => class.agent == "opencode" && class.model == "openai/gpt-5.6-sol",
        "Implementer" => {
            class.agent == "opencode" && class.model == "zai-coding-plan/glm-5.3-flash"
        }
        _ => false,
    };
    if trusted {
        registry_role.to_owned()
    } else {
        class.role.clone()
    }
}

impl Classification {
    pub fn is_agent_pane(&self) -> bool {
        self.agent != "unknown"
    }

    fn model_marker(&self) -> &str {
        &self.model
    }
}

pub fn classify(content: &str) -> Classification {
    if content.contains("EXITED") {
        return Classification {
            role: "unknown".into(),
            agent: "exited".into(),
            model: String::new(),
            state: "exited".into(),
        };
    }
    if content.contains("Claude Code v") || content.contains("auto mode on") {
        let (model, role) = if content.contains("Fable 5") {
            ("claude-fable-5", "Planner")
        } else if content.contains("Opus 5") {
            ("claude-opus-5", "Releaser")
        } else {
            ("", "unknown")
        };
        let state = if content.contains("Yes, I trust this folder") {
            "needs trust"
        } else if content.contains("esc to interrupt") {
            "working"
        } else if has_typed_draft(content) {
            "draft waiting"
        } else {
            "idle"
        };
        return Classification {
            role: role.into(),
            agent: "claude".into(),
            model: model.into(),
            state: state.into(),
        };
    }
    if content.contains("Build auto") || content.contains("Ask anything") {
        let model = if content.contains("GPT-5.6 Sol") {
            "openai/gpt-5.6-sol"
        } else if content.contains("GLM-5.3-Flash") {
            "zai-coding-plan/glm-5.3-flash"
        } else {
            "unknown"
        };
        let role = if model == "zai-coding-plan/glm-5.3-flash" {
            "Implementer"
        } else {
            "unknown"
        };
        let state = if content.contains("esc interrupt") {
            "working"
        } else if content.contains("[Pasted ~") {
            "draft waiting"
        } else {
            "empty"
        };
        return Classification {
            role: role.into(),
            agent: "opencode".into(),
            model: model.into(),
            state: state.into(),
        };
    }
    Classification {
        role: "unknown".into(),
        agent: "unknown".into(),
        model: String::new(),
        state: "empty".into(),
    }
}

fn has_typed_draft(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('❯') && trimmed.chars().skip(1).any(|c| !c.is_whitespace())
    })
}

impl StatusReport {
    pub fn render_text(&self) -> String {
        let mut out = format!("session {} (running)\n", self.session);
        if self.panes.is_empty() {
            out.push_str("no agent panes found\n");
            return out;
        }
        let role_w = self
            .panes
            .iter()
            .map(|p| p.class.role.len())
            .max()
            .unwrap_or(6)
            .max(6);
        let agent_w = self
            .panes
            .iter()
            .map(|p| p.class.agent.len())
            .max()
            .unwrap_or(5)
            .max(5);
        for pane in &self.panes {
            out.push_str(&format!(
                "  {:<12} {:<role_w$} {:<agent_w$} {}\n",
                pane.pane, pane.class.role, pane.class.agent, pane.class.state
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_IDLE: &str = "Claude Code v2.1.251\nFable 5 with xhigh effort\n❯\n";
    const CLAUDE_DRAFT: &str =
        "Opus 5 with xhigh effort · Claude Max\n❯ Act as the Releaser.\n⏵⏵ auto mode on\n";
    const CLAUDE_TRUST: &str =
        "Claude Code v2.1.251\nAccessing workspace:\nYes, I trust this folder\n";
    const OC_SOL_DRAFT: &str = "OPENCODE\nBuild auto · GPT-5.6 Sol OpenAI\n[Pasted ~9 lines]\n";
    const OC_IMPLEM: &str = "Build auto · GLM-5.3-Flash Z.AI Coding Plan\n[Pasted ~8 lines]\n";
    const OC_EMPTY: &str = "Ask anything... \"Fix a TODO\"\nBuild auto · GPT-5.6 Sol OpenAI\n";
    const OC_WORKING: &str = "Build auto · GPT-5.6 Sol\n⬝⬝⬝  esc interrupt\n";

    fn class(text: &str) -> Classification {
        classify(text)
    }

    #[test]
    fn classifies_claude_planner_idle() {
        let c = class(CLAUDE_IDLE);
        assert_eq!(
            (c.role.as_str(), c.agent.as_str(), c.state.as_str()),
            ("Planner", "claude", "idle")
        );
        assert_eq!(c.model, "claude-fable-5");
    }

    #[test]
    fn classifies_claude_releaser_draft() {
        let c = class(CLAUDE_DRAFT);
        assert_eq!(
            (c.role.as_str(), c.state.as_str()),
            ("Releaser", "draft waiting")
        );
        assert_eq!(c.model, "claude-opus-5");
    }

    #[test]
    fn classifies_claude_trust_dialog() {
        assert_eq!(class(CLAUDE_TRUST).state, "needs trust");
    }

    #[test]
    fn classifies_opencode_models_and_states() {
        let sol = class(OC_SOL_DRAFT);
        assert_eq!(
            (sol.agent.as_str(), sol.model.as_str(), sol.state.as_str()),
            ("opencode", "openai/gpt-5.6-sol", "draft waiting")
        );
        let impl_pane = class(OC_IMPLEM);
        assert_eq!(impl_pane.role, "Implementer");
        assert_eq!(class(OC_EMPTY).state, "empty");
        assert_eq!(class(OC_WORKING).state, "working");
    }

    #[test]
    fn registry_roles_validate_against_content() {
        let sol = class(OC_SOL_DRAFT);
        assert_eq!(resolve_role("Refiner", &sol), "Refiner");
        assert_eq!(resolve_role("Reviewer", &sol), "Reviewer");
        assert_eq!(resolve_role("Implementer", &sol), "unknown");
        let impl_pane = class(OC_IMPLEM);
        assert_eq!(resolve_role("Refiner", &impl_pane), "Implementer");
        let planner = class(CLAUDE_IDLE);
        assert_eq!(resolve_role("Planner", &planner), "Planner");
        assert_eq!(resolve_role("Releaser", &planner), "Planner");
    }

    #[test]
    fn session_name_uses_repo_dir() {
        let name = session_name(std::path::Path::new("/home/x/Workplace/borsuk"));
        assert_eq!(name, "borsuk-factory");
    }
}
