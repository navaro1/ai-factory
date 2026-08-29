use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::graph::{Graph, NodeSpec};
use crate::probe::{ItemKind, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub node: String,
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub url: String,
}

impl Task {
    pub fn key(&self) -> String {
        format!("{}#{}", self.kind.as_str(), self.number)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Idle,
    Busy,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Dispatch { pane: String },
    Skip { reason: String },
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub task: Task,
    pub action: Action,
}

pub fn evaluate(graph: &Graph, snapshot: &Snapshot) -> Vec<Task> {
    use crate::graph::conditions::Item;
    let mut tasks = Vec::new();
    for node in &graph.nodes {
        let Some(when) = &node.when else {
            continue;
        };
        for item in &snapshot.items {
            if !item.open {
                continue;
            }
            let cond_item = Item {
                kind: item.kind,
                number: item.number,
                labels: &item.labels,
                open: item.open,
                draft: item.draft,
                blocked_by: &item.blocked_by,
                blockers_open: &item.blockers_open,
            };
            if when.evaluate(&cond_item) {
                tasks.push(Task {
                    node: node.name.clone(),
                    kind: item.kind,
                    number: item.number,
                    title: item.title.clone(),
                    url: item.url.clone(),
                });
            }
        }
    }
    tasks
}

pub fn plan(
    graph: &Graph,
    tasks: &[Task],
    ledger: &Ledger,
    pane_states: &BTreeMap<String, PaneState>,
) -> Vec<Decision> {
    let mut decisions = Vec::new();
    let mut busy_or_dispatched = 0usize;
    for node in &graph.nodes {
        let node_tasks: Vec<&Task> = tasks.iter().filter(|t| t.node == node.name).collect();
        if node_tasks.is_empty() {
            continue;
        }
        let state = pane_states
            .get(&node.name)
            .copied()
            .unwrap_or(PaneState::Missing);
        let node_occupied = ledger.dispatched.values().any(|n| n == &node.name);
        for (index, task) in node_tasks.iter().enumerate() {
            let action = if node.exec == crate::graph::Exec::Auto {
                Action::Skip {
                    reason: "auto exec arrives in v0.4.0".into(),
                }
            } else if ledger.contains(&task.key()) {
                Action::Skip {
                    reason: "already dispatched in this cycle".into(),
                }
            } else if state == PaneState::Missing {
                Action::Skip {
                    reason: format!("no {} pane in the session", node.name),
                }
            } else if node_occupied
                || index > 0
                || state == PaneState::Busy
                || busy_or_dispatched >= graph.limit
            {
                Action::Queue
            } else {
                busy_or_dispatched += 1;
                Action::Dispatch {
                    pane: String::new(),
                }
            };
            decisions.push(Decision {
                task: (*task).clone(),
                action,
            });
        }
    }
    decisions
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub dispatched: BTreeMap<String, String>,
}

impl Ledger {
    pub fn contains(&self, key: &str) -> bool {
        self.dispatched.contains_key(key)
    }

    pub fn mark(&mut self, key: &str, node: &str) {
        self.dispatched.insert(key.to_owned(), node.to_owned());
    }

    pub fn prune_to(&mut self, live_keys: &[String]) {
        let live: std::collections::BTreeSet<&String> = live_keys.iter().collect();
        self.dispatched.retain(|key, _| live.contains(key));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: String,
    pub event: String,
    pub node: String,
    pub item: String,
    pub detail: String,
}

pub struct RunPaths {
    pub dir: PathBuf,
}

impl RunPaths {
    pub fn new(session: &str) -> Self {
        let state = std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".local").join("state")
            });
        RunPaths {
            dir: state.join("aif").join(session),
        }
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))?;
        Ok(())
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.dir.join("ledger.json")
    }

    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub fn load_ledger(&self) -> Ledger {
        std::fs::read_to_string(self.ledger_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save_ledger(&self, ledger: &Ledger) -> Result<()> {
        self.ensure()?;
        let raw = serde_json::to_string_pretty(ledger)?;
        std::fs::write(self.ledger_path(), raw)?;
        Ok(())
    }

    pub fn append_event(&self, event: &Event) -> Result<()> {
        self.ensure()?;
        let line = serde_json::to_string(event)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())?;
        use std::io::Write;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn read_events(&self) -> Result<Vec<Event>> {
        let raw = std::fs::read_to_string(self.events_path()).unwrap_or_default();
        let mut events = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let event: Event =
                serde_json::from_str(line).with_context(|| format!("bad event line: {line}"))?;
            events.push(event);
        }
        Ok(events)
    }
}

pub fn render_prompt(node: &NodeSpec, root: &std::path::Path, task: &Task) -> Result<String> {
    let prompt_path = node
        .prompt
        .as_ref()
        .with_context(|| format!("node {} has no prompt file", node.name))?;
    let full = root.join(prompt_path);
    let raw = std::fs::read_to_string(&full)
        .with_context(|| format!("failed to read prompt {}", full.display()))?;
    Ok(raw
        .replace("{github_issue_no}", &task.number.to_string())
        .replace("{gh_ticket_no}", &task.number.to_string()))
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::conditions::Condition;
    use crate::graph::{Agent, Exec};
    use crate::probe::SnapshotItem;

    fn node(name: &str, when: Option<&str>) -> NodeSpec {
        NodeSpec {
            name: name.into(),
            agent: Agent::Opencode,
            model: "m".into(),
            exec: Exec::Supervised,
            when: when.map(|w| Condition::parse(w).unwrap()),
            prompt: Some(PathBuf::from("prompts/x.md")),
        }
    }

    fn graph_with(nodes: Vec<NodeSpec>, limit: usize) -> Graph {
        Graph {
            tick_secs: 60,
            limit,
            nodes,
            edges: vec![],
        }
    }

    fn issue(number: u64, labels: &[&str]) -> SnapshotItem {
        SnapshotItem {
            kind: ItemKind::Issue,
            number,
            title: format!("item {number}"),
            url: String::new(),
            open: true,
            draft: false,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            blocked_by: vec![],
            blockers_open: vec![],
        }
    }

    #[test]
    fn evaluate_matches_labels() {
        let graph = graph_with(
            vec![
                node("refiner", Some("issue has label 'to-refine'")),
                node(
                    "implementer",
                    Some("issue has label 'refined' and dependencies met"),
                ),
            ],
            3,
        );
        let snapshot = Snapshot {
            items: vec![
                issue(1, &["to-refine"]),
                issue(2, &["refined"]),
                issue(3, &[]),
            ],
        };
        let tasks = evaluate(&graph, &snapshot);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].node, "refiner");
        assert_eq!(tasks[0].number, 1);
        assert_eq!(tasks[1].node, "implementer");
    }

    #[test]
    fn same_ticket_dispatches_once_and_others_queue() {
        let graph = graph_with(
            vec![node("refiner", Some("issue has label 'to-refine'"))],
            3,
        );
        let snapshot = Snapshot {
            items: vec![issue(1, &["to-refine"]), issue(2, &["to-refine"])],
        };
        let tasks = evaluate(&graph, &snapshot);
        let mut ledger = Ledger::default();
        ledger.mark("issue#1", "refiner");
        let mut panes = BTreeMap::new();
        panes.insert("refiner".to_owned(), PaneState::Idle);
        let decisions = plan(&graph, &tasks, &ledger, &panes);
        assert_eq!(
            decisions[0].action,
            Action::Skip {
                reason: "already dispatched in this cycle".into()
            }
        );
        assert_eq!(decisions[1].action, Action::Queue);
    }

    #[test]
    fn busy_pane_queues() {
        let graph = graph_with(vec![node("a", Some("issue has label 'x'"))], 3);
        let snapshot = Snapshot {
            items: vec![issue(1, &["x"]), issue(2, &["x"])],
        };
        let tasks = evaluate(&graph, &snapshot);
        let mut panes = BTreeMap::new();
        panes.insert("a".into(), PaneState::Busy);
        let decisions = plan(&graph, &tasks, &Ledger::default(), &panes);
        assert_eq!(decisions[0].action, Action::Queue);
        assert_eq!(decisions[1].action, Action::Queue);
    }

    #[test]
    fn missing_pane_skips() {
        let graph = graph_with(vec![node("a", Some("issue has label 'x'"))], 3);
        let snapshot = Snapshot {
            items: vec![issue(1, &["x"])],
        };
        let tasks = evaluate(&graph, &snapshot);
        let decisions = plan(&graph, &tasks, &Ledger::default(), &BTreeMap::new());
        assert_eq!(
            decisions[0].action,
            Action::Skip {
                reason: "no a pane in the session".into()
            }
        );
    }

    #[test]
    fn limit_caps_concurrent_dispatch() {
        let graph = graph_with(
            vec![
                node("a", Some("issue has label 'x'")),
                node("b", Some("issue has label 'x'")),
            ],
            1,
        );
        let snapshot = Snapshot {
            items: vec![issue(1, &["x"])],
        };
        let tasks = evaluate(&graph, &snapshot);
        let mut panes = BTreeMap::new();
        panes.insert("a".into(), PaneState::Idle);
        panes.insert("b".into(), PaneState::Idle);
        let decisions = plan(&graph, &tasks, &Ledger::default(), &panes);
        assert_eq!(
            decisions[0].action,
            Action::Dispatch {
                pane: String::new()
            }
        );
        assert_eq!(decisions[1].action, Action::Queue);
    }

    #[test]
    fn ledger_prunes_stale_keys() {
        let mut ledger = Ledger::default();
        ledger.mark("issue#1", "refiner");
        ledger.mark("issue#2", "refiner");
        ledger.prune_to(&["issue#2".to_owned()]);
        assert!(!ledger.contains("issue#1"));
        assert!(ledger.contains("issue#2"));
    }

    #[test]
    fn prompt_placeholders_fill() {
        let spec = node("refiner", Some("issue has label 'to-refine'"));
        let dir = std::env::temp_dir().join("aif-prompt-test");
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(
            dir.join("prompts/x.md"),
            "See ticket {github_issue_no} or {gh_ticket_no}",
        )
        .unwrap();
        let task = Task {
            node: "refiner".into(),
            kind: ItemKind::Issue,
            number: 42,
            title: String::new(),
            url: String::new(),
        };
        let rendered = render_prompt(&spec, &dir, &task).unwrap();
        assert_eq!(rendered, "See ticket 42 or 42");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_nodes_are_skipped() {
        let mut spec = node("ghost", Some("issue has label 'x'"));
        spec.exec = Exec::Auto;
        let graph = graph_with(vec![spec], 3);
        let snapshot = Snapshot {
            items: vec![issue(1, &["x"])],
        };
        let tasks = evaluate(&graph, &snapshot);
        let decisions = plan(&graph, &tasks, &Ledger::default(), &BTreeMap::new());
        assert!(matches!(decisions[0].action, Action::Skip { .. }));
    }
}
