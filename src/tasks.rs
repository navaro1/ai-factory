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
            state: TaskState::Queued,
            attempt: 1,
            session_id: None,
            log_path,
            head_sha: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }
}

/// The task id for one item, per the naming rules.
fn id_for(repo: &str, stage: Stage, kind: ItemKind, number: u64) -> String {
    format!("{}/{}-{}{}", repo, stage.as_str(), kind.as_str(), number)
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
        if let Some(existing) = self.by_id.get(&id) {
            if !existing.state.is_terminal() {
                let what = if kind == ItemKind::Issue {
                    "issue"
                } else {
                    "pull request"
                };
                return Err(anyhow!(
                    "task \"{}\" ({}) already covers {repo} {stage} {what} {number}",
                    existing.id,
                    existing.state,
                ));
            }
        }
        let task = Task::new(repo, stage, kind, number, log_path, now_ms);
        self.by_id.insert(id.clone(), task);
        if let Some(position) = self.order.iter().position(|existing| existing == &id) {
            self.order.remove(position);
        }
        self.order.push(id.clone());
        self.by_id
            .get_mut(&id)
            .ok_or_else(|| anyhow!("task \"{id}\" vanished right after insertion"))
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
