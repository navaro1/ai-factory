//! This module holds one queue for each question that needs a human.
//!
//! A [`Decision`] identifies one condition that requires a human response.
//! The condition can be a permission, question, failed task, labeled item,
//! or release approval. Each condition gets one [`Response`]. The daemon
//! connects each source and handles each response.
//!
//! Each constructor derives a stable identifier (ID). The same condition
//! always gets the same ID. Thus, a repeat push cannot open a second row.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::model::{ItemKind, Stage};
use crate::tasks::Task;

/// One condition that waits for a human answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// The claude CLI asks permission to use one tool.
    Permission {
        /// The task id of the session that asks, for example
        /// `borsuk/implement-i142`.
        task: String,
        /// The request id of the control request from the CLI.
        request_id: String,
        /// The tool name, for example `Write`.
        tool: String,
        /// The tool input the CLI sent.
        input: serde_json::Value,
    },
    /// The agent asked a real question with `AskUserQuestion`.
    Question {
        /// The task id of the session that asks.
        task: String,
        /// The request id of the control request from the CLI.
        request_id: String,
        /// The `questions` array of the request, as the CLI sent it.
        questions: serde_json::Value,
    },
    /// A task failed on its last attempt.
    Stuck {
        /// The ID of the failed task.
        task: String,
        /// Why the task gave up.
        reason: String,
    },
    /// An item has the `needs-human` label and waits for a human.
    ///
    /// This decision does not require a current task. The label can appear
    /// after the task ends. `Text` adds a comment and removes the label.
    /// `Cancel` removes the label without a comment.
    NeedsHuman {
        /// Whether the item is an issue or a pull request.
        kind: ItemKind,
        /// The issue or pull request number.
        number: u64,
        /// The item title.
        title: String,
    },
    /// A release train waits for human approval.
    ReleaseGate {
        /// The pull request numbers stacked at the gate.
        prs: Vec<u64>,
    },
}

impl DecisionKind {
    /// Return the lowercase name for an error message.
    fn name(&self) -> &'static str {
        match self {
            DecisionKind::Permission { .. } => "permission",
            DecisionKind::Question { .. } => "question",
            DecisionKind::Stuck { .. } => "stuck",
            DecisionKind::NeedsHuman { .. } => "needs_human",
            DecisionKind::ReleaseGate { .. } => "release_gate",
        }
    }

    /// Return the accepted response names for an error message.
    fn accepted(&self) -> &'static str {
        match self {
            DecisionKind::Permission { .. } => "allow or deny",
            DecisionKind::Question { .. } => "answers or text",
            DecisionKind::Stuck { .. } => "retry or cancel",
            DecisionKind::NeedsHuman { .. } => "text or cancel",
            DecisionKind::ReleaseGate { .. } => "go",
        }
    }
}

/// Return the stable ID for one failed task attempt.
fn stuck_id(task: &str, attempt: u32) -> String {
    format!("stuck:{task}:{attempt}")
}

/// One open condition that requires a human response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// The stable ID that a constructor derives.
    pub id: String,
    /// The repository alias the decision belongs to.
    pub repo: String,
    /// The pipeline stage, when the decision belongs to one stage.
    pub stage: Option<Stage>,
    /// The condition that waits for a response.
    pub kind: DecisionKind,
    /// Open time in milliseconds since the Unix epoch.
    pub opened_ms: u64,
}

impl Decision {
    fn from_parts(
        id: String,
        repo: String,
        stage: Option<Stage>,
        kind: DecisionKind,
        opened_ms: u64,
    ) -> Self {
        Decision {
            id,
            repo,
            stage,
            kind,
            opened_ms,
        }
    }

    /// Build a tool permission decision for one task.
    pub fn permission(
        task: &Task,
        request_id: &str,
        tool: &str,
        input: serde_json::Value,
        opened_ms: u64,
    ) -> Self {
        Self::from_parts(
            format!("perm:{}:{request_id}", task.id),
            task.repo.clone(),
            Some(task.stage),
            DecisionKind::Permission {
                task: task.id.clone(),
                request_id: request_id.to_string(),
                tool: tool.to_string(),
                input,
            },
            opened_ms,
        )
    }

    /// Build a question decision for one task.
    ///
    /// The ID uses the `perm` prefix. One request can cause a question or a
    /// permission. Thus, both variants use the same ID namespace.
    pub fn question(
        task: &Task,
        request_id: &str,
        questions: serde_json::Value,
        opened_ms: u64,
    ) -> Self {
        Self::from_parts(
            format!("perm:{}:{request_id}", task.id),
            task.repo.clone(),
            Some(task.stage),
            DecisionKind::Question {
                task: task.id.clone(),
                request_id: request_id.to_string(),
                questions,
            },
            opened_ms,
        )
    }

    /// Build a `Stuck` decision from the failed task.
    ///
    /// The ID includes the current task attempt. A later attempt gets a
    /// different ID.
    pub fn stuck(task: &Task, reason: &str, opened_ms: u64) -> Self {
        Self::from_parts(
            stuck_id(&task.id, task.attempt),
            task.repo.clone(),
            Some(task.stage),
            DecisionKind::Stuck {
                task: task.id.clone(),
                reason: reason.to_string(),
            },
            opened_ms,
        )
    }

    /// Build a decision for an item with the `needs-human` label.
    pub fn needs_human(
        repo: &str,
        kind: ItemKind,
        number: u64,
        title: &str,
        opened_ms: u64,
    ) -> Self {
        Self::from_parts(
            format!("human:{repo}:{}{number}", kind.as_str()),
            repo.to_string(),
            None,
            DecisionKind::NeedsHuman {
                kind,
                number,
                title: title.to_string(),
            },
            opened_ms,
        )
    }

    /// Build a manual release decision for one repository.
    pub fn release_gate(repo: &str, prs: Vec<u64>, opened_ms: u64) -> Self {
        Self::from_parts(
            format!("gate:{repo}"),
            repo.to_string(),
            None,
            DecisionKind::ReleaseGate { prs },
            opened_ms,
        )
    }
}

/// One response from a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// Approve the tool use, with the input as stored.
    Allow,
    /// Refuse the tool use and give the agent a reason.
    Deny {
        /// The reason the answer carries to the agent.
        message: String,
    },
    /// Answer the questions, as an updated tool input for the CLI.
    Answers {
        /// The tool input with the answers filled in.
        updated_input: serde_json::Value,
    },
    /// A text answer or instruction.
    ///
    /// For a `Question`, the daemon sends the text to the agent. For a
    /// `NeedsHuman` decision, the daemon adds a comment and removes the label.
    Text {
        /// The text the human typed.
        text: String,
    },
    /// Start the work again.
    Retry,
    /// Stop the related work.
    Cancel,
    /// Release the stacked pull requests.
    Go {
        /// The pull request numbers the human released.
        prs: Vec<u64>,
    },
}

impl Response {
    /// Return the lowercase name for an error message.
    fn name(&self) -> &'static str {
        match self {
            Response::Allow => "allow",
            Response::Deny { .. } => "deny",
            Response::Answers { .. } => "answers",
            Response::Text { .. } => "text",
            Response::Retry => "retry",
            Response::Cancel => "cancel",
            Response::Go { .. } => "go",
        }
    }
}

/// Check that `response` fits the kind of `decision`.
///
/// The legal combinations are:
///
/// | Kind | Responses |
/// |---|---|
/// | `Permission` | `Allow`, `Deny` |
/// | `Question` | `Answers`, `Text` |
/// | `Stuck` | `Retry`, `Cancel` |
/// | `NeedsHuman` | `Text`, `Cancel` |
/// | `ReleaseGate` | `Go` |
///
/// Every other combination is an error.
///
/// For `NeedsHuman`, `Text` adds a comment and removes the label. `Cancel`
/// removes the label without a comment. The function refuses `Retry`. The
/// label can remain after its task ends.
pub fn validate(decision: &Decision, response: &Response) -> Result<()> {
    let legal = match response {
        Response::Allow => matches!(decision.kind, DecisionKind::Permission { .. }),
        Response::Deny { .. } => matches!(decision.kind, DecisionKind::Permission { .. }),
        Response::Answers { .. } => matches!(decision.kind, DecisionKind::Question { .. }),
        Response::Text { .. } => matches!(
            decision.kind,
            DecisionKind::Question { .. } | DecisionKind::NeedsHuman { .. }
        ),
        Response::Retry => matches!(decision.kind, DecisionKind::Stuck { .. }),
        Response::Cancel => matches!(
            decision.kind,
            DecisionKind::Stuck { .. } | DecisionKind::NeedsHuman { .. }
        ),
        Response::Go { .. } => matches!(decision.kind, DecisionKind::ReleaseGate { .. }),
    };
    if legal {
        Ok(())
    } else {
        bail!(
            "a {} decision does not accept the response {}; it accepts {}",
            decision.kind.name(),
            response.name(),
            decision.kind.accepted(),
        );
    }
}

/// All decisions that wait for a human.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Decisions {
    open: Vec<Decision>,
}

impl Decisions {
    /// An empty queue.
    pub fn new() -> Self {
        Decisions::default()
    }

    /// Return the open decisions in push order.
    pub fn open(&self) -> &[Decision] {
        &self.open
    }

    /// Open a decision or refresh the row with the same id.
    ///
    /// The same underlying condition always derives the same id, so a
    /// repeated push keeps one row. A refresh keeps the first open time.
    /// The call returns the id only when it opens a new row.
    pub fn push(&mut self, decision: Decision) -> Option<String> {
        if let Some(row) = self.open.iter_mut().find(|row| row.id == decision.id) {
            let opened_ms = row.opened_ms;
            *row = decision;
            row.opened_ms = opened_ms;
            return None;
        }
        let id = decision.id.clone();
        self.open.push(decision);
        Some(id)
    }

    /// Remove the open decision with `id` and return it.
    ///
    /// The call returns `None` when no open row carries the id.
    pub fn take(&mut self, id: &str) -> Option<Decision> {
        let position = self.open.iter().position(|row| row.id == id)?;
        Some(self.open.remove(position))
    }

    /// Remove and return each open decision for one task.
    ///
    /// The call removes the `Permission`, `Question`, and `Stuck` rows of
    /// the task. `NeedsHuman` and `ReleaseGate` rows belong to no task, so
    /// the call leaves them alone.
    pub fn drop_for_task(&mut self, task: &str) -> Vec<Decision> {
        let mut dropped = Vec::new();
        let mut kept = Vec::new();
        for row in self.open.drain(..) {
            let belongs = match &row.kind {
                DecisionKind::Permission { task: row_task, .. }
                | DecisionKind::Question { task: row_task, .. }
                | DecisionKind::Stuck { task: row_task, .. } => row_task == task,
                DecisionKind::NeedsHuman { .. } | DecisionKind::ReleaseGate { .. } => false,
            };
            if belongs {
                dropped.push(row);
            } else {
                kept.push(row);
            }
        }
        self.open = kept;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const NOW: u64 = 1_000;

    /// A fresh task on attempt 1.
    fn task(repo: &str, stage: Stage, kind: ItemKind, number: u64) -> Task {
        Task::new(repo, stage, kind, number, PathBuf::from("log.jsonl"), NOW)
    }

    /// One decision of every kind, in a fixed order.
    fn every_decision() -> Vec<Decision> {
        let worker = task("borsuk", Stage::Implement, ItemKind::Issue, 142);
        vec![
            Decision::permission(
                &worker,
                "req-1",
                "Write",
                serde_json::json!({"file_path": "src/main.rs"}),
                NOW,
            ),
            Decision::question(
                &worker,
                "req-1",
                serde_json::json!([{
                    "question": "Which database?",
                    "header": "Storage",
                    "options": [{"label": "SQLite", "description": "embedded"}],
                    "multiSelect": false,
                }]),
                NOW,
            ),
            Decision::stuck(&worker, "3 failures", NOW),
            Decision::needs_human("borsuk", ItemKind::Issue, 142, "Fix the flake", NOW),
            Decision::release_gate("borsuk", vec![7, 9], NOW),
        ]
    }

    /// One response of every variant, in a fixed order.
    fn every_response() -> Vec<Response> {
        vec![
            Response::Allow,
            Response::Deny {
                message: "not this file".to_string(),
            },
            Response::Answers {
                updated_input: serde_json::json!({"question": "SQLite"}),
            },
            Response::Text {
                text: "use sqlite".to_string(),
            },
            Response::Retry,
            Response::Cancel,
            Response::Go { prs: vec![7] },
        ]
    }

    #[test]
    fn permission_and_question_ids_derive_from_task_and_request() {
        assert_eq!(every_decision()[0].id, "perm:borsuk/implement-i142:req-1");
        assert_eq!(every_decision()[1].id, "perm:borsuk/implement-i142:req-1");
    }

    #[test]
    fn stuck_ids_derive_from_task_and_attempt() {
        let decision = every_decision()[2].clone();
        assert_eq!(decision.id, "stuck:borsuk/implement-i142:1");
        assert_eq!(decision.repo, "borsuk");
        assert_eq!(decision.stage, Some(Stage::Implement));
        assert_eq!(
            decision.kind,
            DecisionKind::Stuck {
                task: "borsuk/implement-i142".to_string(),
                reason: "3 failures".to_string(),
            }
        );

        let mut retried = task("borsuk", Stage::Review, ItemKind::Pr, 7);
        retried.attempt = 2;
        assert_eq!(
            Decision::stuck(&retried, "boom again", NOW).id,
            "stuck:borsuk/review-p7:2"
        );
    }

    #[test]
    fn needs_human_and_gate_ids_derive_from_repo_and_item() {
        let decisions = every_decision();
        assert_eq!(decisions[3].id, "human:borsuk:i142");
        assert_eq!(decisions[4].id, "gate:borsuk");

        let pr = Decision::needs_human("borsuk", ItemKind::Pr, 7, "Tidy the changelog", NOW);
        assert_eq!(pr.id, "human:borsuk:p7");
    }

    #[test]
    fn pushing_one_condition_twice_keeps_one_row() {
        for decision in every_decision() {
            let mut queue = Decisions::new();
            let id = decision.id.clone();
            assert_eq!(queue.push(decision.clone()).as_deref(), Some(id.as_str()));
            assert_eq!(queue.push(decision), None);
            assert_eq!(queue.open().len(), 1);
        }
    }

    #[test]
    fn pushing_one_condition_again_refreshes_its_data() {
        let mut queue = Decisions::new();
        queue
            .push(Decision::release_gate("borsuk", vec![7], NOW))
            .unwrap();

        let result = queue.push(Decision::release_gate("borsuk", vec![7, 9], NOW + 1));

        assert_eq!(result, None);
        assert_eq!(queue.open().len(), 1);
        assert_eq!(queue.open()[0].opened_ms, NOW);
        assert_eq!(
            queue.open()[0].kind,
            DecisionKind::ReleaseGate { prs: vec![7, 9] }
        );
    }

    #[test]
    fn different_conditions_open_separate_rows() {
        let mut queue = Decisions::new();
        let worker = task("borsuk", Stage::Implement, ItemKind::Issue, 142);
        queue
            .push(Decision::permission(
                &worker,
                "req-1",
                "Write",
                serde_json::json!({}),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::permission(
                &worker,
                "req-2",
                "Write",
                serde_json::json!({}),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::release_gate("qubitsok", vec![], NOW))
            .unwrap();
        assert_eq!(queue.open().len(), 3);
    }

    #[test]
    fn take_removes_the_row_and_a_repeat_push_reopens_it() {
        let mut queue = Decisions::new();
        let decision = Decision::release_gate("borsuk", vec![7, 9], NOW);
        let id = decision.id.clone();
        queue.push(decision);

        let taken = queue.take(&id).unwrap();
        assert_eq!(taken.id, id);
        assert_eq!(taken.kind, DecisionKind::ReleaseGate { prs: vec![7, 9] });
        assert!(queue.open().is_empty());
        assert!(queue.take(&id).is_none());

        // The first decision is closed. The same gate can open again.
        let fresh = Decision::release_gate("borsuk", vec![7, 9], NOW);
        assert_eq!(queue.push(fresh).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn open_lists_rows_in_push_order() {
        let mut queue = Decisions::new();
        queue.push(every_decision()[3].clone()).unwrap();
        queue.push(every_decision()[4].clone()).unwrap();
        let ids: Vec<&str> = queue.open().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["human:borsuk:i142", "gate:borsuk"]);
    }

    #[test]
    fn the_table_accepts_every_legal_pair_and_refuses_every_other() {
        const PERMISSION: usize = 0;
        const QUESTION: usize = 1;
        const STUCK: usize = 2;
        const NEEDS_HUMAN: usize = 3;
        const GATE: usize = 4;
        const ALLOW: usize = 0;
        const DENY: usize = 1;
        const ANSWERS: usize = 2;
        const TEXT: usize = 3;
        const RETRY: usize = 4;
        const CANCEL: usize = 5;
        const GO: usize = 6;

        let legal = [
            (PERMISSION, ALLOW),
            (PERMISSION, DENY),
            (QUESTION, ANSWERS),
            (QUESTION, TEXT),
            (STUCK, RETRY),
            (STUCK, CANCEL),
            (NEEDS_HUMAN, TEXT),
            (NEEDS_HUMAN, CANCEL),
            (GATE, GO),
        ];

        let kinds = every_decision();
        let responses = every_response();
        let mut accepted = 0;
        for (k, decision) in kinds.iter().enumerate() {
            for (r, response) in responses.iter().enumerate() {
                let result = validate(decision, response);
                if legal.contains(&(k, r)) {
                    result.unwrap_or_else(|error| {
                        panic!("{} must accept {:?}: {error}", decision.id, response)
                    });
                    accepted += 1;
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains(decision.kind.name()) && error.contains(response.name()),
                        "refusal for {} against {:?} names both sides: {error}",
                        decision.id,
                        response
                    );
                }
            }
        }
        assert_eq!(accepted, legal.len());
    }

    /// One needs-human decision.
    fn needs_human() -> Decision {
        every_decision()[3].clone()
    }

    #[test]
    fn needs_human_refuses_retry_because_the_label_can_outlive_its_task() {
        let error = validate(&needs_human(), &Response::Retry)
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs_human"), "message: {error}");
        assert!(error.contains("retry"), "message: {error}");
        assert!(error.contains("text or cancel"), "message: {error}");
    }

    #[test]
    fn drop_for_task_removes_only_that_tasks_rows() {
        let mut queue = Decisions::new();
        let a = task("borsuk", Stage::Implement, ItemKind::Issue, 142);
        let b = task("borsuk", Stage::Review, ItemKind::Pr, 7);
        queue
            .push(Decision::permission(
                &a,
                "req-1",
                "Write",
                serde_json::json!({}),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::question(&a, "req-2", serde_json::json!([]), NOW))
            .unwrap();
        queue.push(Decision::stuck(&a, "3 failures", NOW)).unwrap();
        queue
            .push(Decision::permission(
                &b,
                "req-3",
                "Write",
                serde_json::json!({}),
                NOW,
            ))
            .unwrap();
        queue.push(every_decision()[3].clone()).unwrap();
        queue.push(every_decision()[4].clone()).unwrap();

        let dropped = queue.drop_for_task(&a.id);
        let dropped_ids: Vec<&str> = dropped.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            dropped_ids,
            vec![
                "perm:borsuk/implement-i142:req-1",
                "perm:borsuk/implement-i142:req-2",
                "stuck:borsuk/implement-i142:1",
            ]
        );

        let remaining: Vec<&str> = queue.open().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            remaining,
            vec![
                "perm:borsuk/review-p7:req-3",
                "human:borsuk:i142",
                "gate:borsuk",
            ]
        );

        // A task with no rows drops nothing.
        assert!(queue.drop_for_task("borsuk/refine-i1").is_empty());
        assert_eq!(queue.open().len(), 3);
    }

    #[test]
    fn decisions_and_responses_round_trip_through_json() {
        let decision = every_decision()[1].clone();
        let text = serde_json::to_string(&decision).unwrap();
        assert_eq!(serde_json::from_str::<Decision>(&text).unwrap(), decision);
        assert!(text.contains("\"request_id\":\"req-1\""));

        let gate = Decision::release_gate("borsuk", vec![7], NOW);
        let text = serde_json::to_string(&gate.kind).unwrap();
        assert!(text.contains("release_gate"));

        let response = Response::Deny {
            message: "not today".to_string(),
        };
        let text = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&text).unwrap(), response);
    }
}
