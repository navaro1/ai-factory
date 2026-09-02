//! The task table and the task state machine.
//!
//! A task is one stage of one issue or pull request in one repository. The
//! table keeps tasks in insertion order, so the scheduler can dispatch in a
//! fair order. The state machine accepts only the legal transitions and
//! counts attempts, so a broken stage cannot retry forever.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::model::{ItemKind, Stage};

/// The most runs one task may get.
///
/// A failed task can go back to `Queued` while its attempt count is below
/// this limit. Past the limit the retry is refused.
pub const MAX_ATTEMPTS: u32 = 3;

/// The state of one task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// The task waits for the scheduler to start it.
    Queued,
    /// A session runs the task.
    Running,
    /// The session asked the user a question and waits for the answer.
    AwaitingUser,
    /// The task finished with success.
    Done,
    /// The task stopped with this reason.
    Failed(String),
}

impl TaskState {
    /// True when the task reached `Done` or `Failed`.
    ///
    /// A terminal task no longer blocks a new task for the same item.
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Done | TaskState::Failed(_))
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskState::Queued => f.write_str("queued"),
            TaskState::Running => f.write_str("running"),
            TaskState::AwaitingUser => f.write_str("awaiting_user"),
            TaskState::Done => f.write_str("done"),
            TaskState::Failed(reason) => write!(f, "failed(\"{reason}\")"),
        }
    }
}

/// The workflow purpose of one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPurpose {
    /// A normal pipeline task.
    #[default]
    Pipeline,
    /// An interactive issue-creation task.
    TicketCreate,
    /// A read-only conversation about one open issue.
    TicketChat,
}

/// One stage of one item in one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// The task id: `<repo>/<stage>-<kind><number>`.
    pub id: String,
    /// The repository alias.
    pub repo: String,
    /// The pipeline stage the task belongs to.
    pub stage: Stage,
    /// Whether the item is an issue or a pull request.
    pub kind: ItemKind,
    /// The issue or pull request number.
    pub number: u64,
    /// The workflow purpose of this task.
    #[serde(default)]
    pub purpose: TaskPurpose,
    /// The current state.
    pub state: TaskState,
    /// The current attempt number. The first queued run is attempt 1.
    pub attempt: u32,
    /// The session id of the current or last run, when one exists.
    pub session_id: Option<String>,
    /// The JSON lines log file of the task.
    pub log_path: PathBuf,
    /// The head commit sha the task works against, when known.
    pub head_sha: Option<String>,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// Time of the last state change in milliseconds since the Unix epoch.
    pub updated_ms: u64,
}

impl Task {
    /// Create a queued task for one item.
    ///
    /// The id follows the naming rules: `<repo>/<stage>-<kind><number>`, for
    /// example `borsuk/implement-i142`. The task starts on attempt 1.
    pub fn new(
        repo: &str,
        stage: Stage,
        kind: ItemKind,
        number: u64,
        log_path: PathBuf,
        now_ms: u64,
    ) -> Self {
        Task {
            id: id_for(repo, stage, kind, number),
            repo: repo.to_string(),
            stage,
            kind,
            number,
            purpose: TaskPurpose::Pipeline,
            state: TaskState::Queued,
            attempt: 1,
            session_id: None,
            log_path,
            head_sha: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    /// Create one queued read-only conversation for an issue.
    fn ticket_chat(repo: &str, number: u64, log_path: PathBuf, now_ms: u64) -> Self {
        let mut task = Self::new(
            repo,
            Stage::Refine,
            ItemKind::Issue,
            number,
            log_path,
            now_ms,
        );
        task.id = ticket_chat_id(repo, number);
        task.purpose = TaskPurpose::TicketChat;
        task
    }
}

/// The identity of one queued task under an explicit id.
#[derive(Debug, Clone, Copy)]
pub struct ScopedTask<'a> {
    /// The task id, per the naming rules.
    pub id: &'a str,
    /// The repository alias.
    pub repo: &'a str,
    /// The pipeline stage.
    pub stage: Stage,
    /// Whether the item is a ticket or a PR.
    pub kind: ItemKind,
    /// The item number.
    pub number: u64,
}

/// The task id for one item, per the naming rules.
fn id_for(repo: &str, stage: Stage, kind: ItemKind, number: u64) -> String {
    format!("{}/{}-{}{}", repo, stage.as_str(), kind.as_str(), number)
}

/// The task id of one repository-scoped task: `<repo>/<scope>`.
///
/// The release train uses this form: the train, not one PR, is the unit of
/// work, so the id stays stable across batches.
pub fn scoped_id(repo: &str, scope: &str) -> String {
    format!("{repo}/{scope}")
}

/// The task id for one issue conversation.
pub fn ticket_chat_id(repo: &str, number: u64) -> String {
    format!("{repo}/ticket-i{number}")
}

/// All tasks of the daemon, in insertion order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TaskTable {
    /// Every task, keyed by its id.
    pub by_id: BTreeMap<String, Task>,
    /// The task ids in insertion order, for fair dispatch.
    pub order: Vec<String>,
}

impl TaskTable {
    /// An empty table.
    pub fn new() -> Self {
        TaskTable::default()
    }

    /// Queue a task for one item, or re-queue it after a terminal state.
    ///
    /// The call refuses a second task for the same
    /// `(repo, stage, kind, number)` while the existing task is `Queued`,
    /// `Running`, or `AwaitingUser`. The error names the blocking task. When
    /// the existing task is terminal, the call replaces it with a fresh
    /// queued task on attempt 1, and the task moves to the back of the
    /// insertion order.
    pub fn upsert_queued(
        &mut self,
        repo: &str,
        stage: Stage,
        kind: ItemKind,
        number: u64,
        log_path: PathBuf,
        now_ms: u64,
    ) -> Result<&mut Task> {
        let id = id_for(repo, stage, kind, number);
        self.upsert_with_id(
            ScopedTask {
                id: &id,
                repo,
                stage,
                kind,
                number,
            },
            log_path,
            now_ms,
        )
    }

    /// Queue one task under an explicit id, or re-queue it after a terminal
    /// state.
    ///
    /// The id follows the naming rules. [`TaskTable::upsert_queued`] derives
    /// it from the item; the release train passes its scoped id. The call
    /// refuses a second task under the same id while the existing task is
    /// `Queued`, `Running`, or `AwaitingUser`. The error names the blocking
    /// task. When the existing task is terminal, the call replaces it with a
    /// fresh queued task on attempt 1, and the task moves to the back of the
    /// insertion order.
    pub fn upsert_with_id(
        &mut self,
        spec: ScopedTask<'_>,
        log_path: PathBuf,
        now_ms: u64,
    ) -> Result<&mut Task> {
        let ScopedTask {
            id,
            repo,
            stage,
            kind,
            number,
        } = spec;
        if let Some(existing) = self.by_id.get(id) {
            if !existing.state.is_terminal() {
                return Err(anyhow!(
                    "task \"{}\" ({}) already covers {repo} {stage} {} {number}",
                    existing.id,
                    existing.state,
                    kind.noun(),
                ));
            }
        }
        let mut task = Task::new(repo, stage, kind, number, log_path, now_ms);
        task.id = id.to_string();
        self.insert_task(id.to_string(), task)
    }

    /// Insert one task and keep the insertion order in step.
    fn insert_task(&mut self, id: String, task: Task) -> Result<&mut Task> {
        self.by_id.insert(id.clone(), task);
        if let Some(position) = self.order.iter().position(|existing| existing == &id) {
            self.order.remove(position);
        }
        self.order.push(id.clone());
        self.by_id
            .get_mut(&id)
            .ok_or_else(|| anyhow!("task \"{id}\" vanished right after insertion"))
    }

    /// Queue one issue conversation or reuse its active task.
    pub fn upsert_ticket_chat(
        &mut self,
        repo: &str,
        number: u64,
        log_path: PathBuf,
        now_ms: u64,
    ) -> Result<&mut Task> {
        let id = ticket_chat_id(repo, number);
        if self
            .by_id
            .get(&id)
            .is_some_and(|task| !task.state.is_terminal())
        {
            return self
                .by_id
                .get_mut(&id)
                .ok_or_else(|| anyhow!("task \"{id}\" vanished before reuse"));
        }
        let task = Task::ticket_chat(repo, number, log_path, now_ms);
        self.insert_task(id.clone(), task)
    }

    /// Remove one task and its insertion-order entry.
    pub fn remove(&mut self, id: &str) -> Option<Task> {
        self.order.retain(|existing| existing != id);
        self.by_id.remove(id)
    }

    /// Move a task to the state `to`.
    ///
    /// The legal transitions are: `Queued` to `Running` or `Failed`;
    /// `Running` to `AwaitingUser`, `Done`, or `Failed`; `AwaitingUser` to
    /// `Running`, `Done`, or `Failed`; and `Failed` to `Queued` while the
    /// attempt count is below [`MAX_ATTEMPTS`]. `Queued` to `Failed` exists
    /// so a task can be cancelled or dropped before it ever starts. A retry
    /// past the limit is refused. Every other transition is an error that
    /// names both states. A successful transition stamps `updated_ms` with
    /// `now_ms`.
    pub fn transition(&mut self, id: &str, to: TaskState, now_ms: u64) -> Result<()> {
        let task = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| anyhow!("no task \"{id}\" in the table"))?;
        let from = task.state.clone();
        let plain = matches!(
            (&from, &to),
            (TaskState::Queued, TaskState::Running)
                | (TaskState::Queued, TaskState::Failed(_))
                | (TaskState::Running, TaskState::AwaitingUser)
                | (TaskState::Running, TaskState::Done)
                | (TaskState::Running, TaskState::Failed(_))
                | (TaskState::AwaitingUser, TaskState::Running)
                | (TaskState::AwaitingUser, TaskState::Done)
                | (TaskState::AwaitingUser, TaskState::Failed(_))
        );
        if plain {
            task.state = to;
            task.updated_ms = now_ms;
            return Ok(());
        }
        if matches!(&from, TaskState::Failed(_)) && matches!(to, TaskState::Queued) {
            if task.attempt < MAX_ATTEMPTS {
                task.attempt += 1;
                task.state = to;
                task.updated_ms = now_ms;
                return Ok(());
            }
            return Err(anyhow!(
                "task \"{id}\" is {from} on attempt {attempt} of {MAX_ATTEMPTS}; \
                 the retry from failed to queued is refused",
                attempt = task.attempt,
            ));
        }
        Err(anyhow!(
            "illegal transition {from} -> {to} for task \"{id}\""
        ))
    }

    /// Reopen a terminal task for one more human-requested turn.
    ///
    /// The call accepts a task in `Done` or `Failed` and sets `Queued`. It
    /// does not raise the attempt count and it ignores [`MAX_ATTEMPTS`]: a
    /// human asked for the turn, so the automatic retry budget stays
    /// untouched. The call refuses a task in `Queued`, `Running`, or
    /// `AwaitingUser` with a clear error.
    pub fn reopen(&mut self, id: &str, now_ms: u64) -> Result<()> {
        let task = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| anyhow!("no task \"{id}\" in the table"))?;
        if !task.state.is_terminal() {
            return Err(anyhow!(
                "task \"{id}\" is {}, not terminal; reopen accepts only done or failed",
                task.state
            ));
        }
        task.state = TaskState::Queued;
        task.updated_ms = now_ms;
        Ok(())
    }

    /// Cancel a task: the state becomes `Failed("cancelled")`.
    ///
    /// Cancelling follows the transition rules. A queued, running, or
    /// awaiting task can be cancelled.
    pub fn cancel(&mut self, id: &str, now_ms: u64) -> Result<()> {
        self.transition(id, TaskState::Failed("cancelled".to_string()), now_ms)
    }

    /// The running tasks, in insertion order.
    pub fn running(&self) -> Vec<&Task> {
        self.in_order()
            .filter(|task| task.state == TaskState::Running)
            .collect()
    }

    /// The tasks that are not terminal: `Queued`, `Running`, or
    /// `AwaitingUser`, in insertion order.
    pub fn active(&self) -> Vec<&Task> {
        self.in_order()
            .filter(|task| !task.state.is_terminal())
            .collect()
    }

    /// The number of running tasks of each stage.
    ///
    /// Queued, awaiting, and terminal tasks do not use a scheduler slot.
    /// Every stage appears, with 0 when nothing runs.
    pub fn counts_by_stage(&self) -> BTreeMap<Stage, usize> {
        let mut counts: BTreeMap<Stage, usize> =
            Stage::ALL.iter().map(|stage| (*stage, 0)).collect();
        for task in self.by_id.values() {
            if task.state == TaskState::Running {
                *counts.entry(task.stage).or_insert(0) += 1;
            }
        }
        counts
    }

    /// The number of running tasks per repository and stage.
    ///
    /// Queued, awaiting, and terminal tasks do not use a scheduler slot.
    /// Every stage of every repository in the table appears, with 0 when
    /// nothing runs.
    pub fn counts_by_stage_repo(&self) -> BTreeMap<(String, Stage), usize> {
        let mut counts: BTreeMap<(String, Stage), usize> = BTreeMap::new();
        let repos: BTreeSet<&str> = self.by_id.values().map(|task| task.repo.as_str()).collect();
        for repo in repos {
            for stage in Stage::ALL {
                counts.insert((repo.to_string(), stage), 0);
            }
        }
        for task in self.by_id.values() {
            if task.state == TaskState::Running {
                *counts.entry((task.repo.clone(), task.stage)).or_default() += 1;
            }
        }
        counts
    }

    /// The tasks in insertion order.
    fn in_order(&self) -> impl Iterator<Item = &Task> {
        self.order.iter().filter_map(|id| self.by_id.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000;
    const LATER: u64 = 2_000;

    /// Queue one task and give back its id.
    fn queued(
        table: &mut TaskTable,
        repo: &str,
        stage: Stage,
        kind: ItemKind,
        number: u64,
    ) -> String {
        table
            .upsert_queued(repo, stage, kind, number, PathBuf::from("log"), NOW)
            .unwrap()
            .id
            .clone()
    }

    /// A table with one borsuk implement issue 142 task in the given state.
    fn table_in_state(state: TaskState) -> (TaskTable, String) {
        let mut table = TaskTable::new();
        let id = queued(&mut table, "borsuk", Stage::Implement, ItemKind::Issue, 142);
        match state {
            TaskState::Queued => {}
            TaskState::Running => {
                table.transition(&id, TaskState::Running, LATER).unwrap();
            }
            TaskState::AwaitingUser => {
                table.transition(&id, TaskState::Running, LATER).unwrap();
                table
                    .transition(&id, TaskState::AwaitingUser, LATER)
                    .unwrap();
            }
            TaskState::Done => {
                table.transition(&id, TaskState::Running, LATER).unwrap();
                table.transition(&id, TaskState::Done, LATER).unwrap();
            }
            TaskState::Failed(_) => {
                table.transition(&id, TaskState::Running, LATER).unwrap();
                table
                    .transition(&id, TaskState::Failed("boom".to_string()), LATER)
                    .unwrap();
            }
        }
        assert_eq!(table.by_id[&id].state, state);
        (table, id)
    }

    #[test]
    fn ticket_chat_has_a_distinct_id_and_reuses_the_active_task() {
        let mut table = TaskTable::new();
        let first = table
            .upsert_ticket_chat("borsuk", 42, PathBuf::from("ticket-42.jsonl"), 10)
            .unwrap()
            .id
            .clone();
        let second = table
            .upsert_ticket_chat("borsuk", 42, PathBuf::from("ignored.jsonl"), 20)
            .unwrap()
            .id
            .clone();

        assert_eq!(first, "borsuk/ticket-i42");
        assert_eq!(second, first);
        assert_eq!(table.order, vec![first]);
        assert_eq!(
            table.by_id["borsuk/ticket-i42"].purpose,
            TaskPurpose::TicketChat
        );
    }

    #[test]
    fn new_builds_the_id_per_the_naming_rules() {
        let task = Task::new(
            "borsuk",
            Stage::Implement,
            ItemKind::Issue,
            142,
            PathBuf::from("log.jsonl"),
            NOW,
        );
        assert_eq!(task.id, "borsuk/implement-i142");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.attempt, 1);
        assert_eq!(task.session_id, None);
        assert_eq!(task.head_sha, None);
        assert_eq!(task.created_ms, NOW);
        assert_eq!(task.updated_ms, NOW);

        let pr = Task::new(
            "borsuk",
            Stage::Review,
            ItemKind::Pr,
            7,
            PathBuf::from("l"),
            NOW,
        );
        assert_eq!(pr.id, "borsuk/review-p7");
    }

    #[test]
    fn upsert_inserts_new_tasks_in_insertion_order() {
        let mut table = TaskTable::new();
        let first = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        let second = queued(&mut table, "borsuk", Stage::Implement, ItemKind::Issue, 2);
        assert_eq!(table.order, vec![first.clone(), second.clone()]);
        let ids: Vec<&str> = table.active().iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, vec![first.as_str(), second.as_str()]);
    }

    #[test]
    fn upsert_refuses_while_the_existing_task_is_active() {
        for state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
        ] {
            let (mut table, id) = table_in_state(state);
            let error = table
                .upsert_queued(
                    "borsuk",
                    Stage::Implement,
                    ItemKind::Issue,
                    142,
                    PathBuf::from("log"),
                    LATER,
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains(id.as_str()), "message: {error}");
            assert!(error.contains("already covers"), "message: {error}");
            assert_eq!(table.by_id.len(), 1, "the blocking task stays");
        }
    }

    #[test]
    fn the_upsert_refusal_names_the_item_with_the_vocabulary() {
        let (mut table, _) = table_in_state(TaskState::Queued);
        let error = table
            .upsert_queued(
                "borsuk",
                Stage::Implement,
                ItemKind::Issue,
                142,
                PathBuf::from("log"),
                LATER,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("ticket 142"), "message: {error}");

        let mut table = TaskTable::new();
        queued(&mut table, "borsuk", Stage::Review, ItemKind::Pr, 7);
        let error = table
            .upsert_queued(
                "borsuk",
                Stage::Review,
                ItemKind::Pr,
                7,
                PathBuf::from("log"),
                LATER,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("PR 7"), "message: {error}");
    }

    #[test]
    fn upsert_replaces_a_terminal_task_with_a_fresh_queued_task() {
        for state in [TaskState::Done, TaskState::Failed("boom".to_string())] {
            let (mut table, id) = table_in_state(state);
            table
                .upsert_queued(
                    "borsuk",
                    Stage::Implement,
                    ItemKind::Issue,
                    142,
                    PathBuf::from("new-log"),
                    LATER,
                )
                .unwrap();
            let task = &table.by_id[&id];
            assert_eq!(task.state, TaskState::Queued);
            assert_eq!(task.attempt, 1);
            assert_eq!(task.session_id, None);
            assert_eq!(task.log_path, PathBuf::from("new-log"));
            assert_eq!(table.by_id.len(), 1);
        }
    }

    #[test]
    fn upsert_keys_on_repo_stage_kind_and_number() {
        let mut table = TaskTable::new();
        queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 7);
        // The same number in another repository, stage, or kind is free.
        queued(&mut table, "qubitsok", Stage::Refine, ItemKind::Issue, 7);
        queued(&mut table, "borsuk", Stage::Implement, ItemKind::Issue, 7);
        queued(&mut table, "borsuk", Stage::Refine, ItemKind::Pr, 7);
        assert_eq!(table.by_id.len(), 4);
    }

    #[test]
    fn every_transition_in_the_matrix_follows_the_rules() {
        let states = [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
            TaskState::Done,
            TaskState::Failed("boom".to_string()),
        ];
        // Indexes into `states`. Attempt 1 is below the limit, so
        // failed -> queued is legal here.
        let legal: Vec<(usize, usize)> = vec![
            (0, 1), // queued -> running
            (0, 4), // queued -> failed, for a cancel or a dropped trigger
            (1, 2), // running -> awaiting user
            (1, 3), // running -> done
            (1, 4), // running -> failed
            (2, 1), // awaiting user -> running
            (2, 3), // awaiting user -> done
            (2, 4), // awaiting user -> failed
            (4, 0), // failed -> queued
        ];
        for (from_index, from) in states.iter().enumerate() {
            for (to_index, to) in states.iter().enumerate() {
                let (mut table, id) = table_in_state(from.clone());
                let before = table.by_id[&id].clone();
                let result = table.transition(&id, to.clone(), LATER);
                if legal.contains(&(from_index, to_index)) {
                    assert!(result.is_ok(), "transition {from} -> {to} was rejected");
                    assert_eq!(&table.by_id[&id].state, to);
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains(&format!("{from} -> {to}")),
                        "message: {error}"
                    );
                    assert_eq!(table.by_id[&id], before);
                }
            }
        }
    }

    #[test]
    fn retries_count_attempts_up_to_the_limit() {
        let mut table = TaskTable::new();
        let id = queued(&mut table, "borsuk", Stage::Implement, ItemKind::Issue, 142);
        assert_eq!(table.by_id[&id].attempt, 1);
        for expected in [2_u32, 3] {
            table.transition(&id, TaskState::Running, LATER).unwrap();
            table
                .transition(&id, TaskState::Failed("crash".to_string()), LATER)
                .unwrap();
            table.transition(&id, TaskState::Queued, LATER).unwrap();
            assert_eq!(table.by_id[&id].attempt, expected);
        }
    }

    #[test]
    fn retry_past_max_attempts_is_refused_with_a_clear_message() {
        let (mut table, id) = table_in_state(TaskState::Failed("boom".to_string()));
        table.by_id.get_mut(&id).unwrap().attempt = MAX_ATTEMPTS;

        let error = table
            .transition(&id, TaskState::Queued, LATER)
            .unwrap_err()
            .to_string();

        assert!(error.contains(id.as_str()), "message: {error}");
        assert!(error.contains("attempt 3 of 3"), "message: {error}");
        assert!(error.contains("failed"), "message: {error}");
        assert!(error.contains("queued"), "message: {error}");
        assert_eq!(
            table.by_id[&id].state,
            TaskState::Failed("boom".to_string())
        );
        assert_eq!(table.by_id[&id].attempt, MAX_ATTEMPTS);
    }

    #[test]
    fn reopen_refuses_a_task_that_is_not_terminal() {
        for state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
        ] {
            let (mut table, id) = table_in_state(state);
            let before = table.by_id[&id].clone();

            let error = table.reopen(&id, LATER).unwrap_err().to_string();

            assert!(error.contains(id.as_str()), "message: {error}");
            assert!(error.contains("not terminal"), "message: {error}");
            assert_eq!(table.by_id[&id], before, "reopen changed the task");
        }
    }

    #[test]
    fn reopen_queues_a_terminal_task_without_raising_the_attempt_count() {
        for state in [TaskState::Done, TaskState::Failed("boom".to_string())] {
            let (mut table, id) = table_in_state(state);
            table.by_id.get_mut(&id).unwrap().attempt = MAX_ATTEMPTS;

            table.reopen(&id, LATER).unwrap();

            let task = &table.by_id[&id];
            assert_eq!(task.state, TaskState::Queued);
            assert_eq!(task.attempt, MAX_ATTEMPTS, "reopen keeps the count");
            assert_eq!(task.updated_ms, LATER);
        }
    }

    #[test]
    fn reopen_refuses_an_unknown_task() {
        let mut table = TaskTable::new();

        let error = table.reopen("borsuk/implement-i142", LATER).unwrap_err();

        assert!(error.to_string().contains("no task"));
    }

    #[test]
    fn cancelling_records_the_cancelled_reason() {
        for state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
        ] {
            let (mut table, id) = table_in_state(state);
            table.cancel(&id, LATER).unwrap();
            assert_eq!(
                table.by_id[&id].state,
                TaskState::Failed("cancelled".to_string())
            );
        }
    }

    #[test]
    fn cancelling_a_queued_task_removes_it_from_active_tasks() {
        let mut table = TaskTable::new();
        let id = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        assert_eq!(table.active().len(), 1);
        assert_eq!(table.counts_by_stage()[&Stage::Refine], 0);
        assert_eq!(
            table.counts_by_stage_repo()[&("borsuk".to_string(), Stage::Refine)],
            0
        );

        table.cancel(&id, LATER).unwrap();

        assert_eq!(
            table.by_id[&id].state,
            TaskState::Failed("cancelled".to_string())
        );
        assert!(table.active().is_empty(), "a cancelled task is terminal");
        assert_eq!(table.counts_by_stage()[&Stage::Refine], 0);
        assert_eq!(
            table.counts_by_stage_repo()[&("borsuk".to_string(), Stage::Refine)],
            0
        );
    }

    #[test]
    fn a_transition_stamps_updated_ms_and_keeps_created_ms() {
        let (mut table, id) = table_in_state(TaskState::Queued);
        let created = table.by_id[&id].created_ms;
        table.transition(&id, TaskState::Running, LATER).unwrap();
        assert_eq!(table.by_id[&id].updated_ms, LATER);
        assert_eq!(table.by_id[&id].created_ms, created);
    }

    #[test]
    fn an_unknown_task_id_is_an_error() {
        let mut table = TaskTable::new();
        let error = table
            .transition("nope/there-i1", TaskState::Running, LATER)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no task"), "message: {error}");
    }

    #[test]
    fn a_requeued_task_moves_to_the_back_of_the_order() {
        let mut table = TaskTable::new();
        let a = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        let b = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 2);
        let c = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 3);
        table.transition(&a, TaskState::Running, LATER).unwrap();
        table.transition(&a, TaskState::Done, LATER).unwrap();

        table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                1,
                PathBuf::from("log"),
                LATER,
            )
            .unwrap();

        assert_eq!(table.order, vec![b, c, a]);
    }

    #[test]
    fn running_and_active_keep_the_insertion_order() {
        let mut table = TaskTable::new();
        let a = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        let b = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 2);
        let c = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 3);
        table.transition(&b, TaskState::Running, LATER).unwrap();
        table.transition(&c, TaskState::Running, LATER).unwrap();
        table
            .transition(&c, TaskState::AwaitingUser, LATER)
            .unwrap();

        let running: Vec<&str> = table
            .running()
            .iter()
            .map(|task| task.id.as_str())
            .collect();
        assert_eq!(running, vec![b.as_str()]);
        let active: Vec<&str> = table.active().iter().map(|task| task.id.as_str()).collect();
        assert_eq!(active, vec![a.as_str(), b.as_str(), c.as_str()]);

        table
            .transition(&b, TaskState::Failed("boom".to_string()), LATER)
            .unwrap();
        assert!(table.running().is_empty());
        assert_eq!(table.active().len(), 2);
    }

    #[test]
    fn counts_by_stage_count_running_tasks_only() {
        let mut table = TaskTable::new();
        let refine = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 2);
        let implement = queued(&mut table, "borsuk", Stage::Implement, ItemKind::Issue, 3);
        queued(&mut table, "borsuk", Stage::Release, ItemKind::Pr, 4);
        table
            .transition(&refine, TaskState::Running, LATER)
            .unwrap();
        table.transition(&refine, TaskState::Done, LATER).unwrap();
        table
            .transition(&implement, TaskState::Running, LATER)
            .unwrap();

        let counts = table.counts_by_stage();
        assert_eq!(counts.len(), Stage::ALL.len(), "every stage appears");
        assert_eq!(
            counts[&Stage::Refine],
            0,
            "queued and done tasks do not count"
        );
        assert_eq!(counts[&Stage::Implement], 1);
        assert_eq!(counts[&Stage::Review], 0);
        assert_eq!(counts[&Stage::Release], 0);
    }

    #[test]
    fn counts_by_stage_repo_count_per_repository_and_stage() {
        let mut table = TaskTable::new();
        let a = queued(&mut table, "borsuk", Stage::Refine, ItemKind::Issue, 1);
        queued(&mut table, "qubitsok", Stage::Refine, ItemKind::Issue, 2);
        table.transition(&a, TaskState::Running, LATER).unwrap();

        let counts = table.counts_by_stage_repo();
        assert_eq!(counts[&("borsuk".to_string(), Stage::Refine)], 1);
        assert_eq!(counts[&("borsuk".to_string(), Stage::Implement)], 0);
        assert_eq!(counts[&("qubitsok".to_string(), Stage::Refine)], 0);
        assert_eq!(counts[&("qubitsok".to_string(), Stage::Review)], 0);
        assert_eq!(counts.len(), 8, "two repositories times four stages");
    }

    #[test]
    fn no_literal_release_dash_p_task_id_stays_in_src() {
        // The needle is built from parts, so this test file cannot match
        // itself.
        let needle: String = ["/", "release", "-", "p"].concat();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src must be readable") {
                let entry = entry.expect("src entries must be readable");
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source files must be readable");
                assert!(
                    !text.contains(&needle),
                    "{} still names the old release task id form",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn task_state_round_trips_through_json() {
        for state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::AwaitingUser,
            TaskState::Done,
            TaskState::Failed("boom".to_string()),
        ] {
            let text = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<TaskState>(&text).unwrap(), state);
        }
        assert_eq!(
            serde_json::to_string(&TaskState::Failed("boom".to_string())).unwrap(),
            "{\"failed\":\"boom\"}"
        );
        assert_eq!(
            serde_json::to_string(&TaskState::AwaitingUser).unwrap(),
            "\"awaiting_user\""
        );
    }
}
