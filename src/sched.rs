//! The scheduler: what may start, and in which order.
//!
//! The scheduler holds the stage limits and the strict lane reservations. It
//! answers two questions. May one `(stage, repository)` start a task now?
//! Which queued task should start next? The scheduler is pure logic. It runs
//! no process, does no IO, and owns no state beyond the limits themselves.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

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

impl Verdict {
    /// True when the request may start.
    pub fn is_yes(self) -> bool {
        matches!(self, Verdict::Yes)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Yes => f.write_str("yes"),
            Verdict::No(reason) => write!(f, "no: {reason}"),
        }
    }
}

/// Why the scheduler refuses one start request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The stage runs at its limit already.
    StageFull,
    /// The stage has room in total, but the strict lane reservations of the
    /// other repositories leave none for this repository.
    LaneBlocked,
    /// The operator paused this stage, this repository, or the whole factory.
    Paused,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::StageFull => f.write_str("stage full"),
            Reason::LaneBlocked => f.write_str("lane blocked"),
            Reason::Paused => f.write_str("paused"),
        }
    }
}

/// What the operator paused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Paused {
    /// The whole factory takes no new work.
    pub global: bool,
    /// The paused stages.
    pub stages: BTreeSet<Stage>,
    /// The paused repositories, by alias.
    pub repos: BTreeSet<String>,
}

impl Paused {
    /// True when this stage of this repository may not start.
    pub fn blocks(&self, stage: Stage, repo: &str) -> bool {
        self.global || self.stages.contains(&stage) || self.repos.contains(repo)
    }
}

/// Whether one stage of one repository may start a task now.
///
/// This is the only start predicate. A paused stage, repository, or factory
/// reports
/// `Verdict::No(Reason::Paused)` at once. Otherwise the check counts running
/// tasks only. A count at or above the stage limit reports
/// `Verdict::No(Reason::StageFull)`. Otherwise the strict lane reservations
/// apply. The free capacity for this repository is
///
/// `limit(stage) - running(stage)` minus, over every other repository with a
/// reservation on this stage, `max(0, reserve(other) - running(stage, other))`.
///
/// A free capacity of 0 or less reports `Verdict::No(Reason::LaneBlocked)`.
/// The repository's own reservation never works against itself. Queued,
/// awaiting, and terminal tasks hold no slot; the reservation of a repository
/// stays blocked for others even while that repository has nothing to run.
pub fn can_start(
    limits: &Limits,
    paused: &Paused,
    table: &TaskTable,
    stage: Stage,
    repo: &str,
) -> Verdict {
    if paused.blocks(stage, repo) {
        return Verdict::No(Reason::Paused);
    }
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
        reserved += count.saturating_sub(busy);
    }
    if limit - running <= reserved {
        return Verdict::No(Reason::LaneBlocked);
    }
    Verdict::Yes
}

/// The next queued task to dispatch, or None.
///
/// The walk goes in insertion order. It skips tasks whose repository or stage
/// is paused and tasks the capacity check refuses, and it returns the first
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
        if can_start(limits, paused, table, task.stage, &task.repo).is_yes() {
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
            .sum();
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

    /// Pausing a stage, a repository, or everything blocks dispatch and the
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
            stages: BTreeSet::from([Stage::Implement]),
            ..Paused::default()
        };
        assert_eq!(next_dispatch(&limits, &table, &stage), None);
        assert_eq!(
            can_start(&limits, &stage, &table, Stage::Implement, "qubitsok"),
            Verdict::No(Reason::Paused)
        );

        let repo = Paused {
            repos: BTreeSet::from(["borsuk".to_string()]),
            ..Paused::default()
        };
        assert_eq!(
            can_start(&limits, &repo, &table, Stage::Implement, "borsuk"),
            Verdict::No(Reason::Paused)
        );
        assert_eq!(
            next_dispatch(&limits, &table, &repo),
            Some(ids[1].clone()),
            "the paused repository is skipped, the free one still dispatches"
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
            stages: BTreeSet::from([Stage::Implement]),
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

    /// The limits come out of a parsed config.
    #[test]
    fn limits_build_from_a_config() {
        let text = concat!(
            "[stage.refine]\nmodel = \"m\"\nrunner = \"claude\"\n",
            "[stage.implement]\nmodel = \"m\"\nrunner = \"opencode\"\nlimit = 3\n",
            "[stage.review]\nmodel = \"m\"\nrunner = \"opencode\"\n",
            "[stage.release]\nmodel = \"m\"\nrunner = \"claude\"\nlimit = 1\n",
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

    /// The reason texts stay short, for logs and the doctor output.
    #[test]
    fn reasons_display_as_short_text() {
        assert_eq!(Verdict::Yes.to_string(), "yes");
        assert_eq!(Verdict::No(Reason::StageFull).to_string(), "no: stage full");
        assert_eq!(
            Verdict::No(Reason::LaneBlocked).to_string(),
            "no: lane blocked"
        );
        assert_eq!(Verdict::No(Reason::Paused).to_string(), "no: paused");
    }
}
