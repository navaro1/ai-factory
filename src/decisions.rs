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
use crate::theory::answers::Cause;
use crate::theory::cards::CardView;

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
    /// A delta hit every slot and waits for one confirmation.
    DeltaHit {
        /// Whether the record is an issue or a pull request.
        kind: ItemKind,
        /// The issue or pull request number of the record.
        number: u64,
        /// How many slots the delta hit.
        hits: usize,
    },
    /// One theory event waits for a cause and a rung.
    TheoryEvent {
        /// Whether the record is an issue or a pull request.
        kind: ItemKind,
        /// The issue or pull request number of the record.
        number: u64,
        /// The slot key the answer of the row writes.
        slot: String,
        /// The model entries in scope, empty when the row names none.
        entry: String,
        /// The short classifier of the row: the slot outcome of a miss,
        /// the event kind of an event, empty for a violation.
        tag: String,
        /// The one question the row asks.
        question: String,
        /// Where the row came from: `miss`, `violation`, or `event`.
        source: String,
    },
    /// One card waits for a typed answer.
    Card {
        /// Where the card came from.
        source: String,
        /// The question the card asks.
        prompt: String,
        /// The merged pull request the card names, when it names one.
        #[serde(default)]
        number: Option<u64>,
        /// The model entry the card names, when it names one.
        #[serde(default)]
        entry: Option<String>,
        /// Whether the operator gave the cause `recall` to an event the
        /// grading of this card opened.
        #[serde(default)]
        recalled: bool,
    },
    /// The first run of one measurer waits for a confirmation.
    FirstRun {
        /// The area the measurer belongs to.
        area: String,
        /// The measurer id.
        measurer: String,
        /// The first sample the measurer reported.
        sample: String,
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
            DecisionKind::DeltaHit { .. } => "delta_hit",
            DecisionKind::TheoryEvent { .. } => "theory_event",
            DecisionKind::Card { .. } => "card",
            DecisionKind::FirstRun { .. } => "first_run",
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
            DecisionKind::DeltaHit { .. } => "confirm",
            DecisionKind::TheoryEvent { .. } => "theory",
            DecisionKind::Card { .. } => "text",
            DecisionKind::FirstRun { .. } => "confirm or cancel",
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

    /// Build the confirmation row of one hit-only delta.
    pub fn delta_hit(repo: &str, kind: ItemKind, number: u64, hits: usize, opened_ms: u64) -> Self {
        Self::from_parts(
            format!("delta:{repo}:{}{number}", kind.as_str()),
            repo.to_string(),
            None,
            DecisionKind::DeltaHit { kind, number, hits },
            opened_ms,
        )
    }

    /// Build one theory row from the parts its record derived.
    #[allow(clippy::too_many_arguments)]
    pub fn theory_event(
        repo: &str,
        kind: ItemKind,
        number: u64,
        slot: String,
        entry: String,
        tag: String,
        question: String,
        source: String,
        opened_ms: u64,
    ) -> Self {
        Self::from_parts(
            format!("theory:{repo}:{}{number}:{slot}", kind.as_str()),
            repo.to_string(),
            None,
            DecisionKind::TheoryEvent {
                kind,
                number,
                slot,
                entry,
                tag,
                question,
                source,
            },
            opened_ms,
        )
    }

    /// Build one card row from the card of the day.
    ///
    /// `recalled` is true once the operator gave the cause `recall` to
    /// an event the grading of this card opened, and the row then
    /// offers the teach key instead of the answer key.
    pub fn card(repo: &str, card: &CardView, recalled: bool, opened_ms: u64) -> Self {
        Self::from_parts(
            format!("card:{repo}:{}", card.slug()),
            repo.to_string(),
            None,
            DecisionKind::Card {
                source: card.source.clone(),
                prompt: card.prompt.clone(),
                number: card.number,
                entry: card.entry.clone(),
                recalled,
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
    /// Confirm the row as it stands.
    Confirm,
    /// Answer one theory row with a cause and a rung.
    Theory {
        /// Why the miss or the violation happened.
        cause: Cause,
        /// The model entry the human named.
        entry: String,
        /// The rung of the ladder, 1, 2, or 3.
        rung: u8,
        /// The area the entry belongs to, empty when it maps to none.
        area: String,
        /// What the human added, empty when nothing.
        note: String,
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
            Response::Confirm => "confirm",
            Response::Theory { .. } => "theory",
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
/// | `DeltaHit` | `Confirm` |
/// | `TheoryEvent` | `Theory` |
/// | `Card` | `Text` |
/// | `FirstRun` | `Confirm`, `Cancel` |
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
            DecisionKind::Question { .. }
                | DecisionKind::NeedsHuman { .. }
                | DecisionKind::Card { .. }
        ),
        Response::Retry => matches!(decision.kind, DecisionKind::Stuck { .. }),
        Response::Cancel => matches!(
            decision.kind,
            DecisionKind::Stuck { .. }
                | DecisionKind::NeedsHuman { .. }
                | DecisionKind::FirstRun { .. }
        ),
        Response::Go { .. } => matches!(decision.kind, DecisionKind::ReleaseGate { .. }),
        Response::Confirm => matches!(
            decision.kind,
            DecisionKind::DeltaHit { .. } | DecisionKind::FirstRun { .. }
        ),
        Response::Theory { .. } => matches!(decision.kind, DecisionKind::TheoryEvent { .. }),
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
        self.drop_for_task_keep_asks(task, false)
    }

    /// Remove the task's rows, but keep its ask rows when asked.
    ///
    /// `keep_asks` preserves the `Permission` and `Question` rows of a task
    /// whose harness cannot answer a live ask, so a failure does not close
    /// the question. The `Stuck` rows always drop: a fresh failure replaces
    /// the row.
    pub fn drop_for_task_keep_asks(&mut self, task: &str, keep_asks: bool) -> Vec<Decision> {
        let mut dropped = Vec::new();
        let mut kept = Vec::new();
        for row in self.open.drain(..) {
            let belongs = match &row.kind {
                DecisionKind::Permission { task: row_task, .. }
                | DecisionKind::Question { task: row_task, .. } => row_task == task && !keep_asks,
                DecisionKind::Stuck { task: row_task, .. } => row_task == task,
                DecisionKind::NeedsHuman { .. }
                | DecisionKind::ReleaseGate { .. }
                | DecisionKind::DeltaHit { .. }
                | DecisionKind::TheoryEvent { .. }
                | DecisionKind::Card { .. }
                | DecisionKind::FirstRun { .. } => false,
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

/// How the inbox names and drives one decision kind.
///
/// One table answers every per-kind question the feed asks, so a new kind
/// joins the inbox in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// The visible kind name of the feed row.
    pub label: &'static str,
    /// The quick actions of the row: the key text and what it does.
    pub actions: &'static [(&'static str, &'static str)],
    /// The digit keys the row consumes itself, so the shell keeps them
    /// from the view switch.
    pub digits: &'static str,
}

impl Presentation {
    /// The quick action line the feed draws under a selected row.
    pub fn footer(&self) -> String {
        self.actions
            .iter()
            .map(|(key, what)| format!("[{key}] {what}"))
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    }
}

/// The presentation of one decision kind.
pub fn presentation(kind: &DecisionKind) -> Presentation {
    match kind {
        DecisionKind::Permission { .. } => Presentation {
            label: "PERMISSION",
            actions: &[("y", "allow"), ("n", "deny"), ("enter", "details")],
            digits: "",
        },
        DecisionKind::Question { .. } => Presentation {
            label: "QUESTION",
            actions: &[
                ("1-9", "pick"),
                ("s", "submit"),
                ("i", "write"),
                ("enter", "details"),
            ],
            digits: "123456789",
        },
        DecisionKind::Stuck { .. } => Presentation {
            label: "STUCK",
            actions: &[("r", "retry"), ("c", "cancel task"), ("enter", "details")],
            digits: "",
        },
        DecisionKind::NeedsHuman { .. } => Presentation {
            label: "NEEDS HUMAN",
            actions: &[("t", "comment"), ("c", "clear label"), ("enter", "details")],
            digits: "",
        },
        DecisionKind::ReleaseGate { .. } => Presentation {
            label: "RELEASE",
            actions: &[
                ("1-9", "include"),
                ("space", "all/none"),
                ("g", "release"),
                ("enter", "details"),
            ],
            digits: "123456789",
        },
        DecisionKind::DeltaHit { .. } => Presentation {
            label: "DELTA",
            actions: &[("y", "confirm")],
            digits: "",
        },
        DecisionKind::TheoryEvent { .. } => Presentation {
            label: "THEORY",
            actions: &[
                ("m", "model"),
                ("p", "pr"),
                ("r", "recall"),
                ("1-3", "rung"),
                ("a", "area"),
                ("n", "note"),
                ("s", "send"),
            ],
            digits: "123",
        },
        DecisionKind::Card { recalled, .. } if *recalled => Presentation {
            label: "CARD",
            actions: &[("t", "teach")],
            digits: "",
        },
        DecisionKind::Card { .. } => Presentation {
            label: "CARD",
            actions: &[("t", "answer")],
            digits: "",
        },
        DecisionKind::FirstRun { .. } => Presentation {
            label: "FIRST RUN",
            actions: &[("y", "keep"), ("c", "discard")],
            digits: "",
        },
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
            Decision::delta_hit("borsuk", ItemKind::Pr, 7, 4, NOW),
            Decision::theory_event(
                "borsuk",
                ItemKind::Pr,
                7,
                "invariants".to_string(),
                "INV-3".to_string(),
                "sure-miss".to_string(),
                "which entry is wrong?".to_string(),
                "miss".to_string(),
                NOW,
            ),
            Decision::card(
                "borsuk",
                &CardView {
                    source: "stale-entry".to_string(),
                    prompt: "State INV-3. What would violate it?".to_string(),
                    number: None,
                    entry: Some("INV-3".to_string()),
                },
                false,
                NOW,
            ),
            Decision::from_parts(
                "first:borsuk:web-checkout".to_string(),
                "borsuk".to_string(),
                None,
                DecisionKind::FirstRun {
                    area: "web-checkout".to_string(),
                    measurer: "pay-latency".to_string(),
                    sample: "180ms".to_string(),
                },
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
            Response::Confirm,
            Response::Theory {
                cause: Cause::Model,
                entry: "INV-3".to_string(),
                rung: 2,
                area: "web-checkout".to_string(),
                note: String::new(),
            },
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
        const DELTA_HIT: usize = 5;
        const THEORY_EVENT: usize = 6;
        const CARD: usize = 7;
        const FIRST_RUN: usize = 8;
        const ALLOW: usize = 0;
        const DENY: usize = 1;
        const ANSWERS: usize = 2;
        const TEXT: usize = 3;
        const RETRY: usize = 4;
        const CANCEL: usize = 5;
        const GO: usize = 6;
        const CONFIRM: usize = 7;
        const THEORY: usize = 8;

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
            (DELTA_HIT, CONFIRM),
            (THEORY_EVENT, THEORY),
            (CARD, TEXT),
            (FIRST_RUN, CONFIRM),
            (FIRST_RUN, CANCEL),
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

    #[test]
    fn a_theory_event_refuses_a_bare_confirmation() {
        let row = every_decision()[6].clone();
        assert!(matches!(row.kind, DecisionKind::TheoryEvent { .. }));

        let error = validate(&row, &Response::Confirm).unwrap_err().to_string();

        assert_eq!(
            error,
            "a theory_event decision does not accept the response confirm; it accepts theory"
        );
        validate(
            &row,
            &Response::Theory {
                cause: Cause::Recall,
                entry: "INV-3".to_string(),
                rung: 3,
                area: String::new(),
                note: String::new(),
            },
        )
        .expect("a theory event takes a theory answer");
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
    fn keeping_asks_spares_the_ask_rows_and_drops_the_stuck_row() {
        let mut queue = Decisions::new();
        let a = task("borsuk", Stage::Implement, ItemKind::Issue, 142);
        queue
            .push(Decision::permission(
                &a,
                "rej-1",
                "external_directory",
                serde_json::json!({"patterns": ["/tmp/*"]}),
                NOW,
            ))
            .unwrap();
        queue
            .push(Decision::question(&a, "rej-2", serde_json::json!([]), NOW))
            .unwrap();
        queue.push(Decision::stuck(&a, "3 failures", NOW)).unwrap();

        let dropped = queue.drop_for_task_keep_asks(&a.id, true);
        let dropped_ids: Vec<&str> = dropped.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(dropped_ids, vec!["stuck:borsuk/implement-i142:1"]);

        let remaining: Vec<&str> = queue.open().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            remaining,
            vec![
                "perm:borsuk/implement-i142:rej-1",
                "perm:borsuk/implement-i142:rej-2",
            ]
        );

        // The plain drop still removes everything.
        assert_eq!(queue.drop_for_task(&a.id).len(), 2);
        assert!(queue.open().is_empty());
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
