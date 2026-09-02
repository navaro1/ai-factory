//! The scheduler: what may start, and in which order.
//!
//! The scheduler holds the stage limits and the strict lane reservations. It
//! answers two questions. May one `(stage, repository)` start a task now?
//! Which queued task should start next? The scheduler is pure logic. It runs
//! no process and performs no input or output.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::model::Stage;
use crate::tasks::{TaskState, TaskTable};

/// A task id, as [`TaskTable`] keys it.
pub type TaskId = String;

/// The stage limits and the strict lane reservations.
///
/// Build it with [`Limits::from_config`]. The daemon owns the value and may
/// edit the fields at run time when the operator changes a limit or a lane.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limits {
    /// How many tasks each stage may run at once.
    pub stage: BTreeMap<Stage, usize>,
    /// How many slots of one stage stay reserved for one repository, even
    /// when that repository has nothing to run.
    pub lanes: BTreeMap<(Stage, String), usize>,
}

impl Limits {
    /// Read the limits from a parsed configuration.
    ///
    /// Every stage of a parsed config appears in `stage`. Every lane of
    /// every repository appears in `lanes`.
    pub fn from_config(config: &Config) -> Self {
        let stage = config
            .stages
            .iter()
            .map(|(stage, stage_config)| (*stage, stage_config.limit))
            .collect();
        let mut lanes = BTreeMap::new();
        for repo in config.repos.values() {
            for (lane_stage, count) in &repo.lanes {
                lanes.insert((*lane_stage, repo.alias.clone()), *count);
            }
        }
        Limits { stage, lanes }
    }

    /// The limit of one stage.
    ///
    /// A stage with no entry has limit 0, so nothing may start there.
    pub fn limit(&self, stage: Stage) -> usize {
        self.stage.get(&stage).copied().unwrap_or(0)
    }

    /// The slots one stage reserves for one repository.
    ///
    /// A repository with no reservation reserves 0.
    pub fn reserve(&self, stage: Stage, repo: &str) -> usize {
        self.lanes
            .get(&(stage, repo.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

/// The answer of the scheduler to one start request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The request may start.
    Yes,
    /// The request may not start, for the reason inside.
    No(Reason),
}

/// Why the scheduler refuses one start request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The stage runs at its limit already.
    StageFull,
    /// The stage has room in total, but the strict lane reservations of the
    /// other repositories leave none for this repository.
    LaneBlocked,
    /// The operator paused this task, lane, stage, or the whole factory.
    Paused,
}

/// What the operator paused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Paused {
    /// The whole factory takes no new work.
    pub global: bool,
    /// Explicit states for stages.
    pub stages: BTreeMap<Stage, bool>,
    /// Explicit states for repository lanes.
    pub lanes: BTreeMap<(Stage, String), bool>,
    /// Explicit states for tasks, by stable task id.
    pub tasks: BTreeMap<String, bool>,
}

impl Paused {
    /// Set the whole-factory state and remove all narrower states.
    pub fn set_global(&mut self, paused: bool) {
        self.global = paused;
        self.stages.clear();
        self.lanes.clear();
        self.tasks.clear();
    }

    /// Set one stage state.
    pub fn set_stage(&mut self, stage: Stage, paused: bool) {
        self.stages.insert(stage, paused);
    }

    /// Set one repository lane state.
    pub fn set_lane(&mut self, stage: Stage, repo: String, paused: bool) {
        self.lanes.insert((stage, repo), paused);
    }

    /// Set one task state.
    pub fn set_task(&mut self, task: String, paused: bool) {
        self.tasks.insert(task, paused);
    }

    /// True when this repository lane may not start.
    pub fn blocks(&self, stage: Stage, repo: &str) -> bool {
        self.lanes
            .get(&(stage, repo.to_string()))
            .copied()
            .or_else(|| self.stages.get(&stage).copied())
            .unwrap_or(self.global)
    }

    /// True when the pause hierarchy blocks one exact task.
    pub fn blocks_task(&self, stage: Stage, repo: &str, task: &str) -> bool {
        self.tasks
            .get(task)
            .copied()
            .unwrap_or_else(|| self.blocks(stage, repo))
    }
}

/// Whether one exact task may start now.
///
/// A paused lane, stage, or factory reports
/// `Verdict::No(Reason::Paused)` at once. Otherwise the check counts running
/// tasks only. A count at or above the stage limit reports
/// `Verdict::No(Reason::StageFull)`. Otherwise the strict lane reservations
/// apply. The free capacity for this repository is
///
/// `limit(stage) - running(stage)` minus, over every other repository with a
/// reservation on this stage, `max(0, reserve(other) - running(stage, other))`.
///
/// No free capacity reports `Verdict::No(Reason::LaneBlocked)`.
/// The repository's own reservation never works against itself. Queued,
/// awaiting, and terminal tasks hold no slot; the reservation of a repository
/// stays blocked for others even while that repository has nothing to run.
pub fn can_start(
    limits: &Limits,
    paused: &Paused,
    table: &TaskTable,
    stage: Stage,
    repo: &str,
    task: &str,
) -> Verdict {
    if paused.blocks_task(stage, repo, task) {
        return Verdict::No(Reason::Paused);
    }
    capacity_verdict(limits, table, stage, repo)
}

/// The capacity result for one repository lane, without a pause check.
fn capacity_verdict(limits: &Limits, table: &TaskTable, stage: Stage, repo: &str) -> Verdict {
    let running = table.counts_by_stage()[&stage];
    let limit = limits.limit(stage);
    if running >= limit {
        return Verdict::No(Reason::StageFull);
    }
    let running_by_repo = table.counts_by_stage_repo();
    let mut reserved = 0usize;
    for ((lane_stage, lane_repo), count) in &limits.lanes {
        if *lane_stage != stage || lane_repo.as_str() == repo {
            continue;
        }
        let busy = running_by_repo
            .get(&(lane_repo.clone(), stage))
            .copied()
            .unwrap_or(0);
        reserved = reserved.saturating_add(count.saturating_sub(busy));
    }
    if limit - running <= reserved {
        return Verdict::No(Reason::LaneBlocked);
    }
    Verdict::Yes
}

/// The next queued task to dispatch, or None.
///
/// The walk goes in insertion order. It skips tasks whose exact pause state
/// blocks a start. It also skips tasks that fail the capacity check. It returns the first
/// task that may start. It never reorders: an earlier task that may start
/// always wins over a later one, so a later task from another repository
/// never starves the head of the queue.
pub fn next_dispatch(limits: &Limits, table: &TaskTable, paused: &Paused) -> Option<TaskId> {
    for id in &table.order {
        let Some(task) = table.by_id.get(id) else {
            continue;
        };
        if task.state != TaskState::Queued {
            continue;
        }
        if matches!(
            can_start(limits, paused, table, task.stage, &task.repo, &task.id),
            Verdict::Yes
        ) {
            return Some(task.id.clone());
        }
    }
    None
}

/// The scheduler warnings for `aif doctor`.
///
/// A stage whose lane reservations cover its whole limit leaves no slot for
/// any repository without a reservation, ever. That is almost always a
/// mistake, so the warning names the stage. Reservations that exceed the
/// limit produce the warning too, for the same reason.
pub fn warnings(limits: &Limits) -> Vec<String> {
    let mut out = Vec::new();
    for stage in Stage::ALL {
        let sum: usize = limits
            .lanes
            .iter()
            .filter(|((lane_stage, _), _)| *lane_stage == stage)
            .map(|(_, count)| *count)
            .fold(0, usize::saturating_add);
        let limit = limits.limit(stage);
        if limit > 0 && sum >= limit {
            out.push(format!(
                "stage.{stage}: lane reservations cover {sum} of {limit} slots, so no \
                 repository without a reservation can ever run there"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use std::path::PathBuf;

    const NOW: u64 = 1_000;

    /// Limits with the given per-stage limits and `(stage, repo, slots)` lanes.
    fn limits(stage_limits: &[(Stage, usize)], lanes: &[(Stage, &str, usize)]) -> Limits {
        Limits {
            stage: stage_limits.iter().copied().collect(),
            lanes: lanes
                .iter()
                .map(|(stage, repo, count)| ((*stage, repo.to_string()), *count))
                .collect(),
        }
    }

    /// Plain implement limits of 3 and no reservations.
    fn plain_limits() -> Limits {
        limits(&[(Stage::Implement, 3)], &[])
    }

    /// Check capacity with a task that has no exact pause state.
    fn can_start(
        limits: &Limits,
        paused: &Paused,
        table: &TaskTable,
        stage: Stage,
        repo: &str,
    ) -> Verdict {
        super::can_start(limits, paused, table, stage, repo, "test-task")
    }

    /// A table with one queued implement task per `(repo, number)`.
    fn queued_implement(pairs: &[(&str, u64)]) -> (TaskTable, Vec<String>) {
        let mut table = TaskTable::new();
        let mut ids = Vec::new();
        for (repo, number) in pairs {
            let task = table
                .upsert_queued(
                    repo,
                    Stage::Implement,
                    ItemKind::Issue,
                    *number,
                    PathBuf::from("log"),
                    NOW,
                )
                .unwrap();
            ids.push(task.id.clone());
        }
        (table, ids)
    }

    /// Move one task to `Running`.
    fn start(table: &mut TaskTable, id: &str) {
        table.transition(id, TaskState::Running, NOW + 1).unwrap();
    }

    /// The strict reservation keeps a slot free: with implement limit 3,
    /// borsuk reserve 1, and three qubitsok tasks queued, only two start.
    #[test]
    fn a_reserved_slot_stays_free_while_another_repository_has_queued_work() {
        let limits = limits(&[(Stage::Implement, 3)], &[(Stage::Implement, "borsuk", 1)]);
        let (mut table, ids) =
            queued_implement(&[("qubitsok", 1), ("qubitsok", 2), ("qubitsok", 3)]);

        let first = next_dispatch(&limits, &table, &Paused::default()).unwrap();
        assert_eq!(first, ids[0]);
        start(&mut table, &first);
        let second = next_dispatch(&limits, &table, &Paused::default()).unwrap();
        assert_eq!(second, ids[1]);
        start(&mut table, &second);

        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::No(Reason::LaneBlocked)
        );
        assert_eq!(next_dispatch(&limits, &table, &Paused::default()), None);
    }

    /// The reserving repository takes its slot at once when its work arrives.
    #[test]
    fn the_reserving_repository_uses_its_slot_at_once() {
        let limits = limits(&[(Stage::Implement, 3)], &[(Stage::Implement, "borsuk", 1)]);
        let (mut table, ids) = queued_implement(&[("qubitsok", 1), ("qubitsok", 2), ("borsuk", 3)]);
        start(&mut table, &ids[0]);
        start(&mut table, &ids[1]);

        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::No(Reason::LaneBlocked)
        );
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "borsuk"
            ),
            Verdict::Yes
        );
        assert_eq!(
            next_dispatch(&limits, &table, &Paused::default()),
            Some(ids[2].clone())
        );
    }

    /// A repository with no reservation uses every slot nobody reserved.
    #[test]
    fn a_repository_without_reservation_uses_all_remaining_capacity() {
        let limits = plain_limits();
        let (mut table, ids) =
            queued_implement(&[("qubitsok", 1), ("qubitsok", 2), ("qubitsok", 3)]);

        for id in &ids {
            assert_eq!(
                can_start(
                    &limits,
                    &Paused::default(),
                    &table,
                    Stage::Implement,
                    "qubitsok"
                ),
                Verdict::Yes
            );
            let next = next_dispatch(&limits, &table, &Paused::default()).unwrap();
            assert_eq!(&next, id);
            start(&mut table, &next);
        }

        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::No(Reason::StageFull)
        );
    }

    /// Reservations that cover the whole limit produce a warning.
    #[test]
    fn reservations_covering_the_limit_produce_a_warning() {
        let full = limits(
            &[(Stage::Implement, 2)],
            &[(Stage::Implement, "a", 1), (Stage::Implement, "b", 1)],
        );
        let out = warnings(&full);
        assert_eq!(out.len(), 1, "one warning: {out:?}");
        assert!(out[0].contains("stage.implement"), "message: {}", out[0]);

        let free = limits(&[(Stage::Implement, 3)], &[(Stage::Implement, "a", 1)]);
        assert!(warnings(&free).is_empty(), "free capacity stays silent");
        assert!(warnings(&plain_limits()).is_empty());
    }

    /// Runtime lane changes cannot make the warning sum overflow.
    #[test]
    fn excessive_runtime_lane_values_still_produce_a_warning() {
        let limits = limits(
            &[(Stage::Implement, 3)],
            &[
                (Stage::Implement, "a", usize::MAX),
                (Stage::Implement, "b", 1),
            ],
        );

        let out = warnings(&limits);
        assert_eq!(out.len(), 1, "one warning: {out:?}");
        assert!(out[0].contains("stage.implement"), "message: {}", out[0]);
    }

    /// Pausing a stage, a repository lane, or everything blocks dispatch and the
    /// refusal names the pause.
    #[test]
    fn pausing_blocks_dispatch_and_reports_the_right_reason() {
        let limits = plain_limits();
        let (table, ids) = queued_implement(&[("borsuk", 1), ("qubitsok", 2)]);
        let none = Paused::default();

        let global = Paused {
            global: true,
            ..Paused::default()
        };
        assert_eq!(next_dispatch(&limits, &table, &global), None);
        assert_eq!(
            can_start(&limits, &global, &table, Stage::Implement, "borsuk"),
            Verdict::No(Reason::Paused)
        );

        let stage = Paused {
            stages: BTreeMap::from([(Stage::Implement, true)]),
            ..Paused::default()
        };
        assert_eq!(next_dispatch(&limits, &table, &stage), None);
        assert_eq!(
            can_start(&limits, &stage, &table, Stage::Implement, "qubitsok"),
            Verdict::No(Reason::Paused)
        );

        let repo = Paused {
            lanes: BTreeMap::from([((Stage::Implement, "borsuk".to_string()), true)]),
            ..Paused::default()
        };
        assert_eq!(
            can_start(&limits, &repo, &table, Stage::Implement, "borsuk"),
            Verdict::No(Reason::Paused)
        );
        assert_eq!(
            next_dispatch(&limits, &table, &repo),
            Some(ids[1].clone()),
            "the paused lane is skipped, the free one still dispatches"
        );

        // Unpaused and free, can_start says yes.
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "borsuk"
            ),
            Verdict::Yes
        );
        assert_eq!(next_dispatch(&limits, &table, &none), Some(ids[0].clone()));
    }

    #[test]
    fn the_most_specific_pause_state_wins() {
        let mut paused = Paused {
            global: true,
            ..Paused::default()
        };
        paused.set_stage(Stage::Implement, false);
        paused.set_lane(Stage::Implement, "borsuk".to_string(), true);
        paused.set_task("borsuk/implement-i1".to_string(), false);

        assert!(!paused.blocks_task(Stage::Implement, "borsuk", "borsuk/implement-i1"));
        assert!(paused.blocks_task(Stage::Implement, "borsuk", "borsuk/implement-i2"));
        assert!(!paused.blocks_task(Stage::Implement, "qubitsok", "qubitsok/implement-i3"));
        assert!(paused.blocks_task(Stage::Review, "qubitsok", "qubitsok/review-p4"));
    }

    #[test]
    fn a_global_change_removes_all_narrower_states() {
        let mut paused = Paused::default();
        paused.set_stage(Stage::Implement, true);
        paused.set_lane(Stage::Review, "borsuk".to_string(), false);
        paused.set_task("borsuk/review-p7".to_string(), true);

        paused.set_global(true);

        assert!(paused.global);
        assert!(paused.stages.is_empty());
        assert!(paused.lanes.is_empty());
        assert!(paused.tasks.is_empty());
    }

    /// A pause on one stage blocks only that stage.
    #[test]
    fn a_paused_stage_leaves_other_stages_free() {
        let limits = limits(&[(Stage::Refine, 3), (Stage::Implement, 3)], &[]);
        let mut table = TaskTable::new();
        let refine = table
            .upsert_queued(
                "borsuk",
                Stage::Refine,
                ItemKind::Issue,
                5,
                PathBuf::from("log"),
                NOW,
            )
            .unwrap()
            .id
            .clone();
        table
            .upsert_queued(
                "borsuk",
                Stage::Implement,
                ItemKind::Issue,
                1,
                PathBuf::from("log"),
                NOW,
            )
            .unwrap();

        let paused = Paused {
            stages: BTreeMap::from([(Stage::Implement, true)]),
            ..Paused::default()
        };
        assert_eq!(
            next_dispatch(&limits, &table, &paused),
            Some(refine),
            "the refine task dispatches while implement is paused"
        );
    }

    /// Insertion order holds: the earliest startable task always wins, so no
    /// later task from another repository starves the head of the queue.
    #[test]
    fn dispatch_preserves_insertion_order_and_never_starves_the_head_task() {
        let limits = limits(&[(Stage::Implement, 4)], &[]);
        let (mut table, ids) = queued_implement(&[
            ("borsuk", 1),
            ("qubitsok", 2),
            ("borsuk", 3),
            ("qubitsok", 4),
        ]);

        for id in &ids {
            let next = next_dispatch(&limits, &table, &Paused::default())
                .unwrap_or_else(|| panic!("task {id} must stay dispatchable"));
            assert_eq!(&next, id, "the head task must never be passed over");
            start(&mut table, &next);
        }
        assert_eq!(next_dispatch(&limits, &table, &Paused::default()), None);
    }

    /// The stage limit binds before the lanes; the lanes bind before free
    /// capacity runs out.
    #[test]
    fn can_start_names_the_reasons_in_order() {
        // Full stage: even the reserving repository sees StageFull.
        let full = limits(&[(Stage::Implement, 1)], &[(Stage::Implement, "borsuk", 1)]);
        let (mut table, ids) = queued_implement(&[("borsuk", 1)]);
        start(&mut table, &ids[0]);
        assert_eq!(
            can_start(
                &full,
                &Paused::default(),
                &table,
                Stage::Implement,
                "borsuk"
            ),
            Verdict::No(Reason::StageFull)
        );
        assert_eq!(
            can_start(
                &full,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::No(Reason::StageFull)
        );

        // Room in total, but the other reservations eat it.
        let lanes = limits(
            &[(Stage::Implement, 2)],
            &[(Stage::Implement, "a", 1), (Stage::Implement, "b", 1)],
        );
        let (table, _) = queued_implement(&[("c", 9)]);
        assert_eq!(
            can_start(&lanes, &Paused::default(), &table, Stage::Implement, "c"),
            Verdict::No(Reason::LaneBlocked)
        );
    }

    /// Runtime lane changes cannot make the reservation sum overflow.
    #[test]
    fn excessive_runtime_lane_values_still_block_unreserved_work() {
        let limits = limits(
            &[(Stage::Implement, 3)],
            &[
                (Stage::Implement, "a", usize::MAX),
                (Stage::Implement, "b", 1),
            ],
        );

        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &TaskTable::new(),
                Stage::Implement,
                "c"
            ),
            Verdict::No(Reason::LaneBlocked)
        );
    }

    /// The limits come out of a parsed config.
    #[test]
    fn limits_build_from_a_config() {
        let text = concat!(
            "schema_version = 1\n",
            "[stage.refine]\nmodel = \"m\"\nharness = \"claude\"\n",
            "[stage.implement]\nmodel = \"m\"\nharness = \"opencode\"\nlimit = 3\n",
            "[stage.review]\nmodel = \"m\"\nharness = \"opencode\"\n",
            "[stage.release]\nmodel = \"m\"\nharness = \"claude\"\nlimit = 1\n",
            "[ticket.create]\nmodel = \"m\"\nharness = \"opencode\"\n",
            "[ticket.chat]\nmodel = \"m\"\nharness = \"claude\"\n",
            "[repo.borsuk]\npath = \"/tmp/b\"\nlanes = { implement = 1 }\n",
            "[repo.qubitsok]\npath = \"/tmp/q\"\n",
        );
        let config = Config::parse(text).unwrap();
        let limits = Limits::from_config(&config);

        assert_eq!(limits.limit(Stage::Refine), 3, "the config default applies");
        assert_eq!(limits.limit(Stage::Implement), 3);
        assert_eq!(limits.limit(Stage::Release), 1);
        assert_eq!(limits.reserve(Stage::Implement, "borsuk"), 1);
        assert_eq!(limits.reserve(Stage::Implement, "qubitsok"), 0);
        assert_eq!(limits.reserve(Stage::Review, "borsuk"), 0);
    }

    /// Only queued tasks dispatch.
    #[test]
    fn next_dispatch_skips_tasks_that_are_not_queued() {
        let limits = plain_limits();
        let (mut table, ids) = queued_implement(&[("borsuk", 1)]);
        start(&mut table, &ids[0]);
        table
            .transition(&ids[0], TaskState::AwaitingUser, NOW + 2)
            .unwrap();

        assert_eq!(next_dispatch(&limits, &table, &Paused::default()), None);
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "borsuk"
            ),
            Verdict::Yes
        );
    }

    /// An awaiting task holds no scheduler slot; the counts run over running
    /// tasks only. The daemon limits live processes separately.
    #[test]
    fn awaiting_user_tasks_hold_no_scheduler_slot() {
        let limits = limits(&[(Stage::Implement, 1)], &[]);
        let (mut table, ids) = queued_implement(&[("borsuk", 1)]);
        start(&mut table, &ids[0]);
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::No(Reason::StageFull)
        );

        table
            .transition(&ids[0], TaskState::AwaitingUser, NOW + 2)
            .unwrap();
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &table,
                Stage::Implement,
                "qubitsok"
            ),
            Verdict::Yes,
            "the parked chat freed the slot"
        );
    }

    /// An empty table dispatches nothing but blocks nothing.
    #[test]
    fn an_empty_table_yields_no_dispatch() {
        let limits = plain_limits();
        assert_eq!(
            next_dispatch(&limits, &TaskTable::new(), &Paused::default()),
            None
        );
        assert_eq!(
            can_start(
                &limits,
                &Paused::default(),
                &TaskTable::new(),
                Stage::Implement,
                "borsuk"
            ),
            Verdict::Yes
        );
    }
}
