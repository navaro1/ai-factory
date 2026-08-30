//! The one decision queue for every question that needs a human.
//!
//! A [`Decision`] is one condition that an agent or a pipeline stage cannot
//! settle alone: a tool permission, a real question, a stuck task, an item
//! with the `needs-human` label, or a release gate that waits for a human
//! go. Every condition lands in one [`Decisions`] queue and gets one
//! answer, a [`Response`]. The daemon wires the sources and routes each
//! answer to its sink. This module owns the type, the queue, the id rules,
//! and the validation.
//!
//! Decision ids are derived, never random. The same underlying condition
//! always derives the same id, so a repeated push cannot open a second
//! row.

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
    /// A task used its last attempt and stopped.
    Stuck {
        /// The task id of the task that gave up.
        task: String,
        /// Why the task gave up.
        reason: String,
    },
    /// An item carries the `needs-human` label and waits for a human.
    ///
    /// The label can appear after the task ended, so this kind needs no
    /// running task. The daemon routes the two answers: `Text` posts the
    /// text as a comment on the item and removes the label. `Cancel`
    /// removes the label without a comment.
    NeedsHuman {
        /// Whether the item is an issue or a pull request.
        kind: ItemKind,
        /// The issue or pull request number.
        number: u64,
        /// The item title.
        title: String,
    },
    /// A release train waits for a human go.
    ReleaseGate {
        /// The pull request numbers stacked at the gate.
        prs: Vec<u64>,
    },
}

impl DecisionKind {
    /// The lowercase name, for error messages.
    fn name(&self) -> &'static str {
        match self {
            DecisionKind::Permission { .. } => "permission",
            DecisionKind::Question { .. } => "question",
            DecisionKind::Stuck { .. } => "stuck",
            DecisionKind::NeedsHuman { .. } => "needs_human",
            DecisionKind::ReleaseGate { .. } => "release_gate",
        }
    }

    /// The accepted response names, for error messages.
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

/// The stable id for a stuck condition, with the attempt number.
fn stuck_id(task: &str, attempt: u32) -> String {
    format!("stuck:{task}:{attempt}")
}

/// One open question for a human, with its derived id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// The stable, derived id. See [`Decision::new`] for the rules.
    pub id: String,
    /// The repository alias the decision belongs to.
    pub repo: String,
    /// The pipeline stage, when the decision belongs to one stage.
    pub stage: Option<Stage>,
    /// The condition that waits for an answer.
    pub kind: DecisionKind,
    /// Open time in milliseconds since the Unix epoch.
    pub opened_ms: u64,
}

impl Decision {
    /// Build a decision and derive its stable id from `repo` and `kind`.
    ///
    /// The id rules:
    ///
    /// | Kind | Id |
    /// |---|---|
    /// | `Permission` | `perm:<task>:<request_id>` |
    /// | `Question` | `perm:<task>:<request_id>` |
    /// | `Stuck` | `stuck:<task>:<attempt>` |
    /// | `NeedsHuman` | `human:<repo>:<kind><number>` |
    /// | `ReleaseGate` | `gate:<repo>` |
    ///
    /// A `Question` shares the `Permission` shape. Both kinds arrive on the
    /// same control channel of the claude CLI, and one request id never
    /// names both a permission ask and a question.
    ///
    /// A `Stuck` decision built here gets the attempt number 1. Use
    /// [`Decision::stuck`] for a task that gave up on a later attempt.
    pub fn new(repo: &str, stage: Option<Stage>, kind: DecisionKind, opened_ms: u64) -> Self {
        let id = match &kind {
            DecisionKind::Permission {
                task, request_id, ..
            }
            | DecisionKind::Question {
                task, request_id, ..
            } => {
                format!("perm:{task}:{request_id}")
            }
            DecisionKind::Stuck { task, .. } => stuck_id(task, 1),
            DecisionKind::NeedsHuman { kind, number, .. } => {
                format!("human:{repo}:{}{number}", kind.as_str())
            }
            DecisionKind::ReleaseGate { .. } => format!("gate:{repo}"),
        };
        Decision {
            id,
            repo: repo.to_string(),
            stage,
            kind,
            opened_ms,
        }
    }

    /// Build a `Stuck` decision from the task that gave up.
    ///
    /// The id records the attempt number of the task, so a second collapse
    /// after a retry opens a row of its own.
    pub fn stuck(task: &Task, reason: &str, opened_ms: u64) -> Self {
        let id = stuck_id(&task.id, task.attempt);
        Decision {
            id,
            repo: task.repo.clone(),
            stage: Some(task.stage),
            kind: DecisionKind::Stuck {
                task: task.id.clone(),
                reason: reason.to_string(),
            },
            opened_ms,
        }
    }
}

/// The one answer a human gives to a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// Approve the tool use, with the input as stored.
    Allow,
    /// Refuse the tool use, with a reason for the agent.
    Deny {
        /// The reason the answer carries to the agent.
        message: String,
    },
    /// Answer the questions, as an updated tool input for the CLI.
    Answers {
        /// The tool input with the answers filled in.
        updated_input: serde_json::Value,
    },
    /// A free-text answer or instruction.
    ///
    /// On a `Question` the text folds into the answer for the agent. On a
    /// `NeedsHuman` decision the daemon posts the text as a comment on the
    /// item and removes the `needs-human` label.
    Text {
        /// The text the human typed.
        text: String,
    },
    /// Run the work again.
    Retry,
    /// Abandon the work the decision belongs to.
    Cancel,
    /// Release the stacked pull requests.
    Go {
        /// The pull request numbers the human released.
        prs: Vec<u64>,
    },
}

impl Response {
    /// The lowercase name, for error messages.
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
/// The legal pairings:
///
/// | Kind | Responses |
/// |---|---|
/// | `Permission` | `Allow`, `Deny` |
/// | `Question` | `Answers`, `Text` |
/// | `Stuck` | `Retry`, `Cancel` |
/// | `NeedsHuman` | `Text`, `Cancel` |
/// | `ReleaseGate` | `Go` |
///
/// Every other pairing is an error.
///
/// The daemon routes the two `NeedsHuman` answers: `Text` posts the text
/// as a comment on the item and removes the `needs-human` label. `Cancel`
/// removes the label without a comment. `Retry` is refused: the label can
/// outlive the task that raised it, so there may be nothing to retry.
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

/// Every decision that waits for a human, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Decisions {
    /// The open decisions, oldest first.
    pub open: Vec<Decision>,
}

impl Decisions {
    /// An empty queue.
    pub fn new() -> Self {
        Decisions::default()
    }

    /// The open decisions, oldest first.
    pub fn open(&self) -> &[Decision] {
        &self.open
    }

    /// Open a decision, unless a row with the same id is already open.
    ///
    /// The same underlying condition always derives the same id, so a
    /// repeated push keeps one row. The call returns the id when it opened
    /// the row, and `None` when an open row with that id already exists.
    pub fn push(&mut self, decision: Decision) -> Option<String> {
        if self.open.iter().any(|row| row.id == decision.id) {
            return None;
        }
        let id = decision.id.clone();
        self.open.push(decision);
        Some(id)
    }

    /// Remove the open decision with `id` and give the row back.
    ///
    /// The call returns `None` when no open row carries the id.
    pub fn take(&mut self, id: &str) -> Option<Decision> {
        let position = self.open.iter().position(|row| row.id == id)?;
        Some(self.open.remove(position))
    }

    /// Remove every open decision of one task and give the rows back.
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
    use crate::tasks::{TaskState, TaskTable};

    const NOW: u64 = 1_000;

    /// A permission ask for one tool use.
    fn permission(task: &str, request_id: &str) -> DecisionKind {
        DecisionKind::Permission {
            task: task.to_string(),
            request_id: request_id.to_string(),
            tool: "Write".to_string(),
            input: serde_json::json!({"file_path": "src/main.rs"}),
        }
    }

    /// A real question from `AskUserQuestion`.
    fn question(task: &str, request_id: &str) -> DecisionKind {
        DecisionKind::Question {
            task: task.to_string(),
            request_id: request_id.to_string(),
            questions: serde_json::json!([{
                "question": "Which database?",
                "header": "Storage",
                "options": [{"label": "SQLite", "description": "embedded"}],
                "multiSelect": false,
            }]),
        }
    }

    /// A fresh task on attempt 1.
    fn task(repo: &str, stage: Stage, kind: ItemKind, number: u64) -> Task {
        Task::new(repo, stage, kind, number, PathBuf::from("log.jsonl"), NOW)
    }

    /// One decision of every kind, in a fixed order.
    fn every_decision() -> Vec<Decision> {
        vec![
            Decision::new(
                "borsuk",
                Some(Stage::Implement),
                permission("borsuk/implement-i142", "req-1"),
                NOW,
            ),
            Decision::new(
                "borsuk",
                Some(Stage::Implement),
                question("borsuk/implement-i142", "req-2"),
                NOW,
            ),
            Decision::stuck(
                &task("borsuk", Stage::Implement, ItemKind::Issue, 142),
                "3 failures",
                NOW,
            ),
            Decision::new(
                "borsuk",
                None,
                DecisionKind::NeedsHuman {
                    kind: ItemKind::Issue,
                    number: 142,
                    title: "Fix the flake".to_string(),
                },
                NOW,
            ),
            Decision::new(
                "borsuk",
                None,
                DecisionKind::ReleaseGate { prs: vec![7, 9] },
                NOW,
            ),
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
        assert_eq!(every_decision()[1].id, "perm:borsuk/implement-i142:req-2");
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

        // A new Decision built from the kind alone counts as attempt 1.
        let plain = Decision::new(
            "borsuk",
            Some(Stage::Implement),
            DecisionKind::Stuck {
                task: "borsuk/implement-i142".to_string(),
                reason: "3 failures".to_string(),
            },
            NOW,
        );
        assert_eq!(plain.id, "stuck:borsuk/implement-i142:1");

        // After a retry the attempt rises, and the id follows it.
        let mut table = TaskTable::new();
        let id = table
            .upsert_queued(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                7,
                PathBuf::from("l.jsonl"),
                NOW,
            )
            .unwrap()
            .id
            .clone();
        table.transition(&id, TaskState::Running, NOW).unwrap();
        table
            .transition(&id, TaskState::Failed("boom".to_string()), NOW)
            .unwrap();
        table.transition(&id, TaskState::Queued, NOW).unwrap();
        let retried = table.by_id[&id].clone();
        assert_eq!(retried.attempt, 2);
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

        let pr = Decision::new(
            "borsuk",
            None,
            DecisionKind::NeedsHuman {
                kind: ItemKind::Pr,
                number: 7,
                title: "Tidy the changelog".to_string(),
            },
            NOW,
        );
        assert_eq!(pr.id, "human:borsuk:p7");
    }

    #[test]
    fn pushing_one_condition_twice_keeps_one_row() {
        for kind in [
            permission("borsuk/implement-i142", "req-1"),
            question("borsuk/implement-i142", "req-2"),
            DecisionKind::NeedsHuman {
                kind: ItemKind::Issue,
                number: 142,
                title: "Fix the flake".to_string(),
            },
            DecisionKind::ReleaseGate { prs: vec![7, 9] },
        ] {
            let mut queue = Decisions::new();
            let first = Decision::new("borsuk", None, kind.clone(), NOW);
            let id = first.id.clone();
            assert_eq!(queue.push(first).as_deref(), Some(id.as_str()));
            let again = Decision::new("borsuk", None, kind, NOW);
            assert_eq!(queue.push(again), None);
            assert_eq!(queue.open().len(), 1);
        }

        let mut queue = Decisions::new();
        let worker = task("borsuk", Stage::Implement, ItemKind::Issue, 142);
        let id = queue
            .push(Decision::stuck(&worker, "3 failures", NOW))
            .unwrap();
        assert_eq!(
            queue.push(Decision::stuck(&worker, "3 failures", NOW)),
            None
        );
        assert_eq!(queue.open().len(), 1);
        assert_eq!(id, "stuck:borsuk/implement-i142:1");
    }

    #[test]
    fn different_conditions_open_separate_rows() {
        let mut queue = Decisions::new();
        queue
            .push(Decision::new(
                "borsuk",
                None,
                permission("borsuk/implement-i142", "req-1"),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::new(
                "borsuk",
                None,
                permission("borsuk/implement-i142", "req-2"),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::new(
                "qubitsok",
                None,
                DecisionKind::ReleaseGate { prs: vec![] },
                NOW,
            ))
            .unwrap();
        assert_eq!(queue.open().len(), 3);
    }

    #[test]
    fn take_removes_the_row_and_a_repeat_push_reopens_it() {
        let mut queue = Decisions::new();
        let decision = Decision::new(
            "borsuk",
            None,
            DecisionKind::ReleaseGate { prs: vec![7, 9] },
            NOW,
        );
        let id = decision.id.clone();
        queue.push(decision);

        let taken = queue.take(&id).unwrap();
        assert_eq!(taken.id, id);
        assert_eq!(taken.kind, DecisionKind::ReleaseGate { prs: vec![7, 9] });
        assert!(queue.open().is_empty());
        assert!(queue.take(&id).is_none());

        // The answered episode is closed, so the same gate opens again.
        let fresh = Decision::new(
            "borsuk",
            None,
            DecisionKind::ReleaseGate { prs: vec![7, 9] },
            NOW,
        );
        assert_eq!(queue.push(fresh).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn open_lists_rows_oldest_first() {
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

    /// The needs-human row, for the routing tests below.
    fn needs_human() -> Decision {
        every_decision()[3].clone()
    }

    #[test]
    fn needs_human_text_posts_a_comment_and_removes_the_label() {
        let decision = needs_human();
        validate(
            &decision,
            &Response::Text {
                text: "run the seed jobs first".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn needs_human_cancel_removes_the_label_without_a_comment() {
        validate(&needs_human(), &Response::Cancel).unwrap();
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
        let a = "borsuk/implement-i142";
        let b = "borsuk/review-p7";
        queue
            .push(Decision::new("borsuk", None, permission(a, "req-1"), NOW))
            .unwrap();
        queue
            .push(Decision::new("borsuk", None, question(a, "req-2"), NOW))
            .unwrap();
        queue
            .push(Decision::stuck(
                &task("borsuk", Stage::Implement, ItemKind::Issue, 142),
                "3 failures",
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::new("borsuk", None, permission(b, "req-3"), NOW))
            .unwrap();
        queue.push(every_decision()[3].clone()).unwrap();
        queue.push(every_decision()[4].clone()).unwrap();

        let dropped = queue.drop_for_task(a);
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
        assert!(text.contains("\"request_id\":\"req-2\""));

        let gate = Decision::new(
            "borsuk",
            None,
            DecisionKind::ReleaseGate { prs: vec![7] },
            NOW,
        );
        let text = serde_json::to_string(&gate.kind).unwrap();
        assert!(text.contains("release_gate"));

        let response = Response::Deny {
            message: "not today".to_string(),
        };
        let text = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&text).unwrap(), response);
    }
}
