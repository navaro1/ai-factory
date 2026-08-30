use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Presenting,
    AwaitingUser,
    Reserved,
    Accepted,
    Running,
    CancelRequested,
    Uncertain,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Presenting => "presenting",
            TaskState::AwaitingUser => "awaiting_user",
            TaskState::Reserved => "reserved",
            TaskState::Accepted => "accepted",
            TaskState::Running => "running",
            TaskState::CancelRequested => "cancel_requested",
            TaskState::Uncertain => "uncertain",
            TaskState::Succeeded => "succeeded",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
            TaskState::Superseded => "superseded",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "queued" => TaskState::Queued,
            "presenting" => TaskState::Presenting,
            "awaiting_user" => TaskState::AwaitingUser,
            "reserved" => TaskState::Reserved,
            "accepted" => TaskState::Accepted,
            "running" => TaskState::Running,
            "cancel_requested" => TaskState::CancelRequested,
            "uncertain" => TaskState::Uncertain,
            "succeeded" => TaskState::Succeeded,
            "failed" => TaskState::Failed,
            "cancelled" => TaskState::Cancelled,
            "superseded" => TaskState::Superseded,
            _ => return None,
        })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Succeeded
                | TaskState::Failed
                | TaskState::Cancelled
                | TaskState::Superseded
        )
    }

    pub fn consumes_node(&self) -> bool {
        !self.is_terminal()
    }

    pub fn consumes_global(&self) -> bool {
        matches!(
            self,
            TaskState::Reserved
                | TaskState::Accepted
                | TaskState::Running
                | TaskState::CancelRequested
                | TaskState::Uncertain
        )
    }

    pub fn can_reach(self, to: TaskState) -> bool {
        use TaskState::*;
        if self == to {
            return false;
        }
        if self.is_terminal() {
            return false;
        }
        match self {
            Queued => matches!(to, Reserved | Presenting | Superseded | Failed),
            Presenting => matches!(to, AwaitingUser | Superseded | Failed),
            AwaitingUser => matches!(to, Reserved | Superseded | Failed),
            Reserved => matches!(to, Accepted | Queued | Uncertain | Cancelled),
            Accepted => matches!(to, Running | CancelRequested | Failed | Uncertain | Cancelled),
            Running => matches!(
                to,
                Succeeded | Failed | CancelRequested | Uncertain | Cancelled
            ),
            CancelRequested => matches!(to, Cancelled | Uncertain | Succeeded | Failed),
            Uncertain => matches!(to, Succeeded | Failed | Cancelled),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ExtIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: String,
    pub node: String,
    pub agent: crate::graph::Agent,
    pub exec: crate::graph::Exec,
    pub kind: crate::snapshot::ItemKind,
    pub number: u64,
    pub item_node_id: String,
    pub title: String,
    pub revision: u64,
    pub attempt: u32,
    pub state: TaskState,
    pub ext: ExtIds,
    pub detail: String,
    pub created_seq: u64,
    pub worktree: Option<String>,
}

impl TaskRecord {
    pub fn task_id(node: &str, kind: crate::snapshot::ItemKind, number: u64, revision: u64, attempt: u32) -> String {
        format!(
            "{}-{}{}-r{}a{}",
            node,
            kind.as_str(),
            number,
            revision,
            attempt
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_classification_matches_plan() {
        assert!(TaskState::Reserved.consumes_node());
        assert!(TaskState::Reserved.consumes_global());
        assert!(TaskState::Uncertain.consumes_global());
        assert!(TaskState::CancelRequested.consumes_global());
        assert!(TaskState::AwaitingUser.consumes_node());
        assert!(!TaskState::AwaitingUser.consumes_global());
        assert!(!TaskState::Succeeded.consumes_node());
        assert!(!TaskState::Failed.consumes_global());
    }

    #[test]
    fn terminal_states_are_final() {
        for state in [TaskState::Succeeded, TaskState::Failed, TaskState::Cancelled, TaskState::Superseded] {
            assert!(state.is_terminal());
            assert!(!state.can_reach(TaskState::Queued));
        }
    }

    #[test]
    fn uncertain_requires_manual_resolution() {
        assert!(TaskState::Uncertain.can_reach(TaskState::Cancelled));
        assert!(!TaskState::Uncertain.can_reach(TaskState::Queued));
        assert!(!TaskState::Uncertain.can_reach(TaskState::Running));
    }

    #[test]
    fn reserved_can_requeue_or_go_uncertain() {
        assert!(TaskState::Reserved.can_reach(TaskState::Queued));
        assert!(TaskState::Reserved.can_reach(TaskState::Uncertain));
        assert!(TaskState::Reserved.can_reach(TaskState::Accepted));
        assert!(!TaskState::Reserved.can_reach(TaskState::Running));
    }

    #[test]
    fn gate_close_table() {
        assert!(TaskState::Queued.can_reach(TaskState::Superseded));
        assert!(TaskState::Presenting.can_reach(TaskState::Superseded));
        assert!(TaskState::AwaitingUser.can_reach(TaskState::Superseded));
        assert!(TaskState::Reserved.can_reach(TaskState::Cancelled));
        assert!(TaskState::Running.can_reach(TaskState::Uncertain));
    }
}
