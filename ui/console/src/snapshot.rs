use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::graph::conditions::Item as CondItem;
use crate::graph::{Graph, Retrigger};
use crate::probe::parse_blocked_by;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    PullRequest,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::Issue => "issue",
            ItemKind::PullRequest => "pr",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "issue" => Some(ItemKind::Issue),
            "pr" => Some(ItemKind::PullRequest),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemState {
    pub repo_id: u64,
    pub node_id: String,
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub open: bool,
    pub draft: bool,
    pub labels: Vec<String>,
    pub blocked_by: Vec<u64>,
    pub head: Option<String>,
}

impl ItemState {
    pub fn material_key(&self) -> String {
        let mut labels = self.labels.clone();
        labels.sort();
        let mut blocked = self.blocked_by.clone();
        blocked.sort();
        format!(
            "{}|{}|{}|{:?}|{:?}|{:?}",
            self.kind.as_str(),
            self.open,
            self.draft,
            labels,
            blocked,
            self.head
        )
    }

    pub fn cond_kind(&self) -> crate::graph::conditions::ItemKind {
        match self.kind {
            ItemKind::Issue => crate::graph::conditions::ItemKind::Issue,
            ItemKind::PullRequest => crate::graph::conditions::ItemKind::PullRequest,
        }
    }
}

pub fn apply_batch(
    snapshot: &mut BTreeMap<String, ItemState>,
    items: Vec<ItemState>,
) -> Vec<String> {
    let mut changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        seen.insert(item.node_id.clone());
        let is_new = !snapshot.contains_key(&item.node_id);
        let material_changed = snapshot
            .get(&item.node_id)
            .map(|old| old.material_key() != item.material_key())
            .unwrap_or(true);
        if is_new || material_changed {
            changed.push(item.node_id.clone());
        }
        snapshot.insert(item.node_id.clone(), item);
    }
    let removed: Vec<String> = snapshot
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    for key in removed {
        snapshot.remove(&key);
        changed.push(key);
    }
    changed
}

pub fn node_label(item: &ItemState) -> String {
    item.node_id.clone()
}

#[derive(Debug, Clone, Default)]
pub struct GateTracker {
    open: BTreeMap<String, bool>,
    gate_gen: BTreeMap<String, u64>,
    mat_serial: BTreeMap<String, u64>,
    pub last_tasked_rev: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadyWork {
    pub node: String,
    pub item_node_id: String,
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub revision: u64,
}

impl GateTracker {
    pub fn gate_key(node: &str, item_node_id: &str) -> String {
        format!("{node}/{item_node_id}")
    }

    pub fn apply(
        &mut self,
        graph: &Graph,
        snapshot: &BTreeMap<String, ItemState>,
        material_changed: &[String],
    ) -> Vec<ReadyWork> {
        let open_numbers: BTreeMap<u64, bool> = snapshot
            .values()
            .map(|item| (item.number, item.open))
            .collect();
        let is_open = |n: u64| open_numbers.get(&n).copied().unwrap_or(false);
        let changed: std::collections::BTreeSet<&String> = material_changed.iter().collect();

        let mut ready = Vec::new();
        for node in &graph.nodes {
            let Some(when) = &node.when else {
                continue;
            };
            for item in snapshot.values() {
                let blockers_open: Vec<u64> = item
                    .blocked_by
                    .iter()
                    .filter(|n| is_open(**n))
                    .copied()
                    .collect();
                let cond = CondItem {
                    kind: item.cond_kind(),
                    number: item.number,
                    labels: &item.labels,
                    open: item.open,
                    draft: item.draft,
                    blocked_by: &item.blocked_by,
                    blockers_open: &blockers_open,
                };
                let key = Self::gate_key(&node.name, &item.node_id);
                let was_open = self.open.get(&key).copied().unwrap_or(false);
                let now_open = item.open && when.evaluate(&cond);
                if now_open && !was_open {
                    *self.gate_gen.entry(key.clone()).or_insert(0) += 1;
                }
                self.open.insert(key.clone(), now_open);
                let serial = self.mat_serial.entry(item.node_id.clone()).or_insert(0);
                if changed.contains(&item.node_id) {
                    *serial += 1;
                }
                if !now_open {
                    continue;
                }
                let gen = *self.gate_gen.get(&key).unwrap_or(&1);
                let minor = match node.retrigger {
                    Retrigger::HeadSha => *serial,
                    Retrigger::Gate => 0,
                };
                let revision = gen.saturating_mul(1_000_003) + minor;
                if self.last_tasked_rev.get(&key).copied() != Some(revision) {
                    ready.push(ReadyWork {
                        node: node.name.clone(),
                        item_node_id: item.node_id.clone(),
                        kind: item.kind,
                        number: item.number,
                        title: item.title.clone(),
                        revision,
                    });
                }
            }
        }
        ready
    }
}

#[derive(Debug, Deserialize)]
pub struct IssueJson {
    pub id: u64,
    pub node_id: String,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub state: String,
    #[serde(default)]
    pub labels: Vec<LabelJson>,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct PullJson {
    pub id: u64,
    pub node_id: String,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub state: String,
    pub draft: bool,
    #[serde(default)]
    pub labels: Vec<LabelJson>,
    pub head: Option<HeadJson>,
}

#[derive(Debug, Deserialize)]
pub struct HeadJson {
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LabelJson {
    pub name: String,
}

pub fn issue_to_state(repo_id: u64, raw: &IssueJson) -> Option<ItemState> {
    if raw.pull_request.is_some() {
        return None;
    }
    Some(ItemState {
        repo_id,
        node_id: raw.node_id.clone(),
        kind: ItemKind::Issue,
        number: raw.number,
        title: raw.title.clone(),
        open: raw.state == "open",
        draft: false,
        labels: raw.labels.iter().map(|l| l.name.clone()).collect(),
        blocked_by: parse_blocked_by(&raw.body),
        head: None,
    })
}

pub fn pull_to_state(repo_id: u64, raw: &PullJson) -> ItemState {
    ItemState {
        repo_id,
        node_id: raw.node_id.clone(),
        kind: ItemKind::PullRequest,
        number: raw.number,
        title: raw.title.clone(),
        open: raw.state == "open",
        draft: raw.draft,
        labels: raw.labels.iter().map(|l| l.name.clone()).collect(),
        blocked_by: parse_blocked_by(&raw.body),
        head: raw.head.as_ref().and_then(|h| h.sha.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::conditions::Condition;

    fn issue(node_id: &str, number: u64, labels: &[&str]) -> ItemState {
        ItemState {
            repo_id: 1,
            node_id: node_id.into(),
            kind: ItemKind::Issue,
            number,
            title: format!("item {number}"),
            open: true,
            draft: false,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            blocked_by: vec![],
            head: None,
        }
    }

    fn graph_for(retrigger: Retrigger) -> Graph {
        use crate::graph::{Agent, Exec, NodeSpec};
        let spec = |name: &str, when: &str| NodeSpec {
            name: name.into(),
            agent: Agent::Codex,
            model: "m".into(),
            exec: Exec::Auto,
            when: Some(Condition::parse(when).unwrap()),
            prompt: Some("p.md".into()),
            limit: None,
            retrigger: Retrigger::Gate,
        };
        Graph {
            version: 4,
            tick_secs: 600,
            limit: 3,
            nodes: vec![
                spec(
                    "refiner",
                    "issue has label 'to-refine' and dependencies met",
                ),
                NodeSpec {
                    name: "reviewer".into(),
                    retrigger,
                    ..spec("reviewer", "pr is draft")
                },
            ],
            edges: vec![],
        }
    }

    #[test]
    fn material_key_ignores_title() {
        let mut a = issue("I_1", 1, &["x"]);
        let b = issue("I_1", 1, &["x"]);
        a.title = "renamed".into();
        assert_eq!(a.material_key(), b.material_key());
        let c = issue("I_1", 1, &["x", "y"]);
        assert_ne!(a.material_key(), c.material_key());
    }

    #[test]
    fn unchanged_batch_reports_no_change() {
        let mut snap = BTreeMap::new();
        let one = issue("I_1", 1, &[]);
        apply_batch(&mut snap, vec![one.clone()]);
        let changes = apply_batch(&mut snap, vec![one]);
        assert!(changes.is_empty());
    }

    #[test]
    fn gate_rise_creates_task_once_per_generation() {
        let graph = graph_for(Retrigger::Gate);
        let mut snap = BTreeMap::new();
        apply_batch(&mut snap, vec![issue("I_1", 1, &["to-refine"])]);
        let mut tracker = GateTracker::default();
        let ready = tracker.apply(&graph, &snap, &["I_1".into()]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].node, "refiner");
        tracker
            .last_tasked_rev
            .insert(GateTracker::gate_key("refiner", "I_1"), ready[0].revision);
        let again = tracker.apply(&graph, &snap, &[]);
        assert!(again.is_empty());
    }

    #[test]
    fn gate_fall_and_rise_bumps_generation() {
        let graph = graph_for(Retrigger::Gate);
        let mut snap = BTreeMap::new();
        let mut tracker = GateTracker::default();
        apply_batch(&mut snap, vec![issue("I_1", 1, &["to-refine"])]);
        let first = tracker.apply(&graph, &snap, &["I_1".into()]);
        assert_eq!(first.len(), 1);
        tracker
            .last_tasked_rev
            .insert(GateTracker::gate_key("refiner", "I_1"), first[0].revision);
        apply_batch(&mut snap, vec![issue("I_1", 1, &[])]);
        assert!(tracker.apply(&graph, &snap, &["I_1".into()]).is_empty());
        apply_batch(&mut snap, vec![issue("I_1", 1, &["to-refine"])]);
        let second = tracker.apply(&graph, &snap, &["I_1".into()]);
        assert_eq!(second.len(), 1);
        assert!(second[0].revision > first[0].revision);
    }

    #[test]
    fn head_sha_retrigger_creates_new_revision() {
        let graph = graph_for(Retrigger::HeadSha);
        let mut snap = BTreeMap::new();
        let mut tracker = GateTracker::default();
        let mut pr = ItemState {
            kind: ItemKind::PullRequest,
            node_id: "P_1".into(),
            draft: true,
            head: Some("aa".into()),
            ..issue("P_1", 7, &[])
        };
        apply_batch(&mut snap, vec![pr.clone()]);
        let first = tracker.apply(&graph, &snap, &["P_1".into()]);
        assert_eq!(first.len(), 1);
        tracker
            .last_tasked_rev
            .insert(GateTracker::gate_key("reviewer", "P_1"), first[0].revision);
        assert!(tracker.apply(&graph, &snap, &[]).is_empty());
        pr.head = Some("bb".into());
        apply_batch(&mut snap, vec![pr]);
        let second = tracker.apply(&graph, &snap, &["P_1".into()]);
        assert_eq!(second.len(), 1);
        assert!(second[0].revision > first[0].revision);
    }

    #[test]
    fn blocker_closure_enables_dependent_in_same_batch() {
        let graph = graph_for(Retrigger::Gate);
        let mut blocker = issue("I_1", 2, &[]);
        blocker.open = true;
        let mut dep = issue("I_2", 3, &["to-refine"]);
        dep.blocked_by = vec![2];
        let mut snap = BTreeMap::new();
        apply_batch(&mut snap, vec![blocker.clone(), dep]);
        let mut tracker = GateTracker::default();
        let ready = tracker.apply(&graph, &snap, &[]);
        assert!(ready.is_empty());
        let mut closed = blocker;
        closed.open = false;
        let dep_again = snap.get("I_2").cloned().unwrap();
        apply_batch(&mut snap, vec![closed, dep_again]);
        let ready = tracker.apply(&graph, &snap, &["I_1".into()]);
        assert!(ready.iter().any(|r| r.number == 3 && r.node == "refiner"));
    }
}
