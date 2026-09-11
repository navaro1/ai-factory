//! The measure records: what one measure run reports and how it ships.
//!
//! A measurer prints one JSON object per stdout line. [`parse_lines`] turns
//! every line into one [`Record`]. A line that does not parse, and a line
//! without a direction, becomes one incomparable record that carries the
//! reason, so a broken measurer costs one record and never a failed run.
//!
//! A fast check has no output contract. Its record is its exit code under
//! the [`EXIT_UNIT`] unit, so a passing check reads 0 and every failure
//! reads the code the shell reported.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::records::{close_tag, MEASURE_BLOCK};
use super::verify::Measurer;
use crate::exec::{CmdOut, Exec};

/// The unit of a record whose value is a process exit code.
pub const EXIT_UNIT: &str = "exit";

/// The task id prefix of one fast check, after the repository alias.
const FAST_PREFIX: &str = "/fast-";

/// The task id prefix of one measurer run, after the repository alias.
const MEASURE_PREFIX: &str = "/measure-";

/// How many characters of the tree hash a task id carries.
pub const TREE_CHARS: usize = 8;

/// The directory under the state directory that holds the measure cache
/// and the temporary index of [`tree_hash`].
pub const MEASURE_DIR: &str = "measure";

/// What one comparison renders for a value that does not exist.
const NO_VALUE: &str = "none";

/// Which direction of one measurement is the better one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// A smaller value is better, for example a latency or an exit code.
    Lower,
    /// A larger value is better, for example a hit rate.
    Higher,
}

impl Direction {
    /// The lowercase name a measurer prints.
    pub fn name(self) -> &'static str {
        match self {
            Direction::Lower => "lower",
            Direction::Higher => "higher",
        }
    }

    /// Parse one of the two lowercase direction names.
    pub fn parse(text: &str) -> Option<Self> {
        [Direction::Lower, Direction::Higher]
            .into_iter()
            .find(|direction| direction.name() == text)
    }
}

impl Display for Direction {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One measurement of one measure run.
///
/// A comparable record carries a value, a unit, and a direction. An
/// incomparable record carries the reason instead, and the comparison of
/// v0.7 C27 reads that reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// What was measured: a feature id, or the output name of a measurer.
    pub id: String,
    /// The measured value. Absent when the record is incomparable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The unit of the value, for example `ms` or `exit`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Which direction is better. Absent when the record is incomparable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// Why the record is incomparable. Empty for a comparable record.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Record {
    /// One comparable record.
    pub fn value(id: &str, value: f64, unit: &str, direction: Direction) -> Self {
        Record {
            id: id.to_string(),
            value: Some(value),
            unit: unit.to_string(),
            direction: Some(direction),
            reason: String::new(),
        }
    }

    /// One record the daemon cannot compare, and why.
    pub fn incomparable(id: &str, reason: impl Into<String>) -> Self {
        Record {
            id: id.to_string(),
            value: None,
            unit: String::new(),
            direction: None,
            reason: reason.into(),
        }
    }

    /// True when the record carries no value.
    pub fn is_incomparable(&self) -> bool {
        self.value.is_none()
    }
}

impl Display for Record {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match (self.value, self.direction) {
            (Some(value), Some(direction)) => {
                write!(formatter, "{}  {value} {}  {direction}", self.id, self.unit)
            }
            _ => write!(formatter, "{}  incomparable: {}", self.id, self.reason),
        }
    }
}

/// The record of one fast check: its exit code under the `exit` unit.
pub fn exit_record(feature: &str, code: i32) -> Record {
    Record::value(feature, f64::from(code), EXIT_UNIT, Direction::Lower)
}

/// Parse the stdout of one measure run into records.
///
/// Every non-empty line yields exactly one record, in line order. A line
/// that is not a JSON object, and an object without a direction, yields
/// one incomparable record that names the reason. The parse never fails.
pub fn parse_lines(text: &str) -> Vec<Record> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(parse_line)
        .collect()
}

/// The permissive input shape of one record line.
///
/// The value arrives as any JSON value, so a value that is not a number
/// names itself in the reason instead of failing the whole line.
#[derive(Debug, Default, Deserialize)]
struct RawRecord {
    id: Option<String>,
    value: Option<serde_json::Value>,
    unit: Option<String>,
    direction: Option<String>,
}

fn parse_line(line: &str) -> Record {
    let Ok(raw) = serde_json::from_str::<RawRecord>(line) else {
        return Record::incomparable("", "the line is not a measure record");
    };
    let id = raw.id.unwrap_or_default();
    let Some(raw_value) = raw.value else {
        return Record::incomparable(&id, "the record has no value");
    };
    let Some(value) = raw_value.as_f64() else {
        return Record::incomparable(&id, format!("value {raw_value} is not a number"));
    };
    let Some(text) = raw.direction else {
        return Record::incomparable(&id, "the record has no direction");
    };
    let Some(direction) = Direction::parse(&text) else {
        return Record::incomparable(&id, format!("unknown direction \"{text}\""));
    };
    Record::value(
        &id,
        value,
        raw.unit.as_deref().unwrap_or_default(),
        direction,
    )
}

/// Render every record of one run as one `<aif-measure-v1>` block.
///
/// One line per record, in run order, so the comment reads as a table and
/// an incomparable record names its reason in place.
pub fn block(records: &[Record]) -> String {
    let close = close_tag(MEASURE_BLOCK);
    let mut text = String::from(MEASURE_BLOCK);
    for record in records {
        text.push('\n');
        text.push_str(&record.to_string());
    }
    text.push('\n');
    text.push_str(&close);
    text
}

/// The task id of one fast check: `<alias>/fast-<tree8>-<feature>`.
pub fn fast_id(alias: &str, tree8: &str, feature: &str) -> String {
    format!("{alias}{FAST_PREFIX}{tree8}-{feature}")
}

/// The task id of one measurer run:
/// `<alias>/measure-<tree8>-<area>-<measurer>`.
pub fn measure_id(alias: &str, tree8: &str, area: &str, measurer: &str) -> String {
    format!("{alias}{MEASURE_PREFIX}{tree8}-{area}-{measurer}")
}

/// The feature of one fast task id, when the id names a fast check.
///
/// The tree hash is a fixed width, so the feature starts one character
/// after it. A feature id may itself carry a dash; the width decides, not
/// the last separator.
pub fn fast_feature(task_id: &str) -> Option<&str> {
    let (_, rest) = task_id.split_once(FAST_PREFIX)?;
    let feature = rest.get(TREE_CHARS + 1..)?;
    (!feature.is_empty()).then_some(feature)
}

/// The fast checks one review task waits for.
///
/// The daemon builds one value per admitted review of a governed pull
/// request and drops it when the review leaves the gate, so nothing here
/// outlives the head it measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastRun {
    /// The task id of every fast check, in queue order.
    pub tasks: Vec<String>,
    /// The tree hash the run measures, so a re-fire of the same tree can
    /// tell that its finding is the one the record already carries.
    pub tree: String,
    /// The record of each fast task that ended, by task id.
    pub records: BTreeMap<String, Record>,
}

impl FastRun {
    /// One run over the given fast task ids, against the given tree.
    pub fn new(tasks: Vec<String>, tree: String) -> Self {
        FastRun {
            tasks,
            tree,
            records: BTreeMap::new(),
        }
    }

    /// True when every fast task of the run reported its record.
    pub fn finished(&self) -> bool {
        self.tasks.iter().all(|id| self.records.contains_key(id))
    }

    /// Stop waiting for one check. The answer says whether the run held it.
    ///
    /// An aborted check reports no exit, so nothing will ever record it.
    /// It leaves the wait list instead, because an abort is neither a
    /// pass nor a failure of the check.
    pub fn drop_task(&mut self, task: &str) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|id| id != task);
        self.records.remove(task);
        self.tasks.len() != before
    }

    /// The first record that did not report a zero value, in queue order.
    pub fn failure(&self) -> Option<&Record> {
        self.tasks
            .iter()
            .filter_map(|id| self.records.get(id))
            .find(|record| record.value != Some(0.0))
    }
}

/// The tree hash of one worktree, with the uncommitted work included.
///
/// A measure run reads the files on disk, not the last commit, so the
/// records belong to the working tree. The hash therefore comes from a
/// temporary index at `<state_dir>/measure/tmp-index-<pid>`: the call
/// reads `HEAD` into it, adds every change to it, and writes it out as a
/// tree. The live index of the worktree never moves, so an agent that
/// staged a file keeps its staging.
pub fn tree_hash(exec: &dyn Exec, worktree: &Path, state_dir: &Path) -> Result<String> {
    let dir = state_dir.join(MEASURE_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let index = dir.join(format!("tmp-index-{}", std::process::id()));
    let hash = write_tree(exec, worktree, &index);
    let _ = fs::remove_file(&index);
    hash
}

/// Read `HEAD` into the temporary index, add every change, and write the
/// tree out.
fn write_tree(exec: &dyn Exec, worktree: &Path, index: &Path) -> Result<String> {
    let index = index.to_string_lossy().into_owned();
    for args in [["read-tree", "HEAD"], ["add", "-A"]] {
        let out = index_git(exec, worktree, &index, &args)?;
        require_git(out, &args.join(" "))?;
    }
    let out = index_git(exec, worktree, &index, &["write-tree"])?;
    let out = require_git(out, "write-tree")?;
    let hash = out.stdout.trim().to_string();
    if hash.len() < TREE_CHARS {
        bail!("git write-tree printed no tree hash");
    }
    Ok(hash)
}

/// Run one git command against the temporary index of `worktree`.
///
/// [`Exec`] carries no environment, so the call runs `env` with the one
/// assignment the index needs in front of git.
fn index_git(exec: &dyn Exec, worktree: &Path, index: &str, args: &[&str]) -> Result<CmdOut> {
    let assignment = format!("GIT_INDEX_FILE={index}");
    let dir = worktree.to_string_lossy().into_owned();
    let mut argv = vec![assignment.as_str(), "git", "-C", dir.as_str()];
    argv.extend_from_slice(args);
    exec.run("env", &argv, None)
        .context("env could not run git")
}

/// Fail when one git call of the temporary index did not exit zero.
fn require_git(out: CmdOut, what: &str) -> Result<CmdOut> {
    if out.status != 0 {
        let detail = out.stderr.lines().next().unwrap_or("no stderr");
        bail!("git {what} exited with status {}: {detail}", out.status);
    }
    Ok(out)
}

/// The cache file of one measurer at one tree.
///
/// The path is `<state_dir>/measure/<alias>/<tree>/<area>-<hash>.json`.
/// The hash covers the identity of the command, so an edited measurer
/// misses the cache of the tree it already ran.
pub fn cache_path(state_dir: &Path, alias: &str, tree: &str, measurer: &Measurer) -> PathBuf {
    state_dir
        .join(MEASURE_DIR)
        .join(alias)
        .join(tree)
        .join(format!(
            "{}-{}.json",
            measurer.area,
            measurer_hash(measurer)
        ))
}

/// The identity hash of one measurer: its id, command, mode, and timeout.
///
/// The digest is FNV-1a over those four fields, so the same measurer
/// always names the same file, on every host and every build.
fn measurer_hash(measurer: &Measurer) -> String {
    let text = format!(
        "{}\n{}\n{}\n{}",
        measurer.id,
        measurer.command,
        measurer.mode.name(),
        measurer.timeout_s
    );
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The records one tree already produced for one measurer, when the cache
/// holds them.
///
/// An unreadable or broken file is a miss, not an error. The measurer
/// then runs again and writes the file afresh.
pub fn read_cache(path: &Path) -> Option<Vec<Record>> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Store the records of one finished run under its cache path.
pub fn write_cache(path: &Path, records: &[Record]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = serde_json::to_string(records).context("cannot render the measure records")?;
    fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// What one measurement did between the merge base and the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The head reports a measurement the base did not.
    New,
    /// The base reported a measurement the head does not.
    Resolved,
    /// The value moved against the direction of the record.
    Worsened,
    /// The value moved with the direction of the record.
    Improved,
    /// The two values are equal.
    Unchanged,
    /// The two records do not describe the same measurement.
    Incomparable,
}

impl State {
    /// The lowercase name the comparison table prints.
    pub fn name(self) -> &'static str {
        match self {
            State::New => "new",
            State::Resolved => "resolved",
            State::Worsened => "worsened",
            State::Improved => "improved",
            State::Unchanged => "unchanged",
            State::Incomparable => "incomparable",
        }
    }
}

impl Display for State {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One row of the comparison of one area.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// The record id the two sides share.
    pub id: String,
    /// The value at the merge base, absent when the base had none.
    pub before: Option<f64>,
    /// The value at the head, absent when the head has none.
    pub after: Option<f64>,
    /// The unit of both values.
    pub unit: String,
    /// What the row says about the change.
    pub state: State,
    /// Why the row is incomparable. Empty for every other state.
    pub reason: String,
}

impl Display for Comparison {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        if self.state == State::Incomparable {
            return match self.reason.is_empty() {
                true => write!(formatter, "{}  {}", self.id, self.state),
                false => write!(formatter, "{}  {}: {}", self.id, self.state, self.reason),
            };
        }
        write!(
            formatter,
            "{}  {} \u{2192} {}  {}  {}",
            self.id,
            number(self.before),
            number(self.after),
            self.unit,
            self.state
        )
    }
}

/// One value of a comparison row, or the word for its absence.
fn number(value: Option<f64>) -> String {
    value.map_or_else(|| NO_VALUE.to_string(), |value| value.to_string())
}

/// Compare the records of one area at the merge base with the records of
/// the same area at the head.
///
/// Every record of the head yields one row, in head order, and every
/// record the head dropped follows in base order. A record either side
/// could not measure, a unit change, and a direction change all yield one
/// incomparable row that names the reason.
pub fn compare(before: &[Record], after: &[Record]) -> Vec<Comparison> {
    let mut rows: Vec<Comparison> = after
        .iter()
        .map(|record| {
            row(
                before.iter().find(|entry| entry.id == record.id),
                Some(record),
            )
        })
        .collect();
    for record in before {
        if !after.iter().any(|entry| entry.id == record.id) {
            rows.push(row(Some(record), None));
        }
    }
    rows
}

/// One comparison row from the two records of one measurement.
fn row(before: Option<&Record>, after: Option<&Record>) -> Comparison {
    let named = after.or(before);
    let id = named.map(|record| record.id.clone()).unwrap_or_default();
    let unit = named.map(|record| record.unit.clone()).unwrap_or_default();
    let incomparable = |reason: &str| Comparison {
        id: id.clone(),
        before: before.and_then(|record| record.value),
        after: after.and_then(|record| record.value),
        unit: unit.clone(),
        state: State::Incomparable,
        reason: reason.to_string(),
    };
    for record in [before, after].into_iter().flatten() {
        if record.is_incomparable() {
            return incomparable(&record.reason);
        }
    }
    match (before, after) {
        (None, Some(record)) => Comparison {
            id,
            before: None,
            after: record.value,
            unit,
            state: State::New,
            reason: String::new(),
        },
        (Some(record), None) => Comparison {
            id,
            before: record.value,
            after: None,
            unit,
            state: State::Resolved,
            reason: String::new(),
        },
        (Some(base), Some(head)) => {
            if base.unit != head.unit {
                return incomparable(&format!(
                    "the unit changed from \"{}\" to \"{}\"",
                    base.unit, head.unit
                ));
            }
            if base.direction != head.direction {
                return incomparable("the direction changed");
            }
            let (Some(first), Some(second), Some(direction)) =
                (base.value, head.value, head.direction)
            else {
                return incomparable("the record has no value");
            };
            let state = if second == first {
                State::Unchanged
            } else if (second > first) == (direction == Direction::Higher) {
                State::Improved
            } else {
                State::Worsened
            };
            Comparison {
                id,
                before: Some(first),
                after: Some(second),
                unit,
                state,
                reason: String::new(),
            }
        }
        (None, None) => incomparable("neither side measured"),
    }
}

/// What one area's comparison means for the stage that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureVerdict {
    /// The stage may proceed.
    Pass,
    /// The stage must stop.
    Fail,
}

/// The verdict of one area over its comparison rows.
///
/// Every area counts as `observe` today, so the verdict never fails.
/// Chunk C25 of v0.7 builds the properties of an area and their policies,
/// and they plug in here. A `ratchet` policy will fail on a `worsened`
/// row, an `error` policy will fail on a `new` row, and `observe` will go
/// on reporting without a stop.
pub fn apply_policy(_area: &str, _comparisons: &[Comparison]) -> MeasureVerdict {
    MeasureVerdict::Pass
}

/// Render the comparison of every area as the table an agent reads.
///
/// One `AREA <id>` line opens each area, and one row per measurement
/// follows it. An empty comparison reads `none`, the word every prompt
/// slot of a run without measurers carries.
pub fn agent_text(areas: &[(String, Vec<Comparison>)]) -> String {
    if areas.is_empty() {
        return NO_VALUE.to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    for (area, rows) in areas {
        lines.push(format!("AREA {area}"));
        lines.extend(rows.iter().map(Comparison::to_string));
    }
    lines.join("\n")
}

/// Render the comparison of every area as one `<aif-measure-v1>` block.
pub fn comparison_block(areas: &[(String, Vec<Comparison>)]) -> String {
    format!(
        "{MEASURE_BLOCK}\n{}\n{}",
        agent_text(areas),
        close_tag(MEASURE_BLOCK)
    )
}

/// One measure target of a review: its task, its area, and its records
/// once they exist.
///
/// A slot that starts with its records came from the cache, so no task
/// ever runs for it.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasureSlot {
    /// The task id of the run.
    pub id: String,
    /// The area the measurer guards.
    pub area: String,
    /// The records of the run, absent until it ends.
    pub records: Option<Vec<Record>>,
}

/// The measure runs one review waits for, at the merge base and at the
/// head.
///
/// The daemon builds one value per admitted review of a governed pull
/// request and drops it when the last run ends, so nothing here outlives
/// the head it measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeasureRun {
    /// The slots of the merge base, in queue order.
    pub base: Vec<MeasureSlot>,
    /// The slots of the head, in queue order.
    pub head: Vec<MeasureSlot>,
}

impl MeasureRun {
    /// True when every slot of both sides holds its records.
    pub fn finished(&self) -> bool {
        self.slots().all(|slot| slot.records.is_some())
    }

    /// True when this review still waits for `task`.
    ///
    /// A slot that already holds its records waits for nothing, so an
    /// aborted or finished run stops holding the review even while the
    /// other side of the batch still works.
    pub fn holds(&self, task: &str) -> bool {
        self.slots()
            .any(|slot| slot.id == task && slot.records.is_none())
    }

    /// Store the records of one finished run. The answer says whether the
    /// run belongs here.
    pub fn settle(&mut self, task: &str, records: Vec<Record>) -> bool {
        for slot in self.base.iter_mut().chain(self.head.iter_mut()) {
            if slot.id == task {
                slot.records = Some(records);
                return true;
            }
        }
        false
    }

    /// The comparison of every area, in head order first.
    ///
    /// An area the head no longer measures still reports, so a measurer
    /// the diff removed shows as resolved instead of vanishing.
    pub fn report(&self) -> Vec<(String, Vec<Comparison>)> {
        let mut areas: Vec<String> = Vec::new();
        for slot in self.head.iter().chain(self.base.iter()) {
            if !areas.iter().any(|area| area == &slot.area) {
                areas.push(slot.area.clone());
            }
        }
        areas
            .into_iter()
            .map(|area| {
                let before = side_records(&self.base, &area);
                let after = side_records(&self.head, &area);
                let rows = compare(&before, &after);
                (area, rows)
            })
            .collect()
    }

    /// Every slot of the run, base first.
    fn slots(&self) -> impl Iterator<Item = &MeasureSlot> {
        self.base.iter().chain(self.head.iter())
    }
}

/// The records of one area on one side of a run, in slot order.
fn side_records(slots: &[MeasureSlot], area: &str) -> Vec<Record> {
    slots
        .iter()
        .filter(|slot| slot.area == area)
        .filter_map(|slot| slot.records.as_ref())
        .flat_map(|records| records.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RealExec;
    use crate::theory::verify::Mode;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique temporary directory for one test.
    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-measure-{}-{}-{}",
            label,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the old temp dir must be removable");
        }
        fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }

    /// Run one real git command in `dir` and fail loudly when it does.
    fn run_git(dir: &Path, args: &[&str]) -> String {
        let mut argv = vec!["-C", dir.to_str().unwrap()];
        argv.extend_from_slice(args);
        let out = RealExec.run("git", &argv, None).expect("git must run");
        assert_eq!(out.status, 0, "git {args:?}: {}", out.stderr);
        out.stdout.trim().to_string()
    }

    /// One measurer of the cache tests.
    fn measurer(command: &str) -> Measurer {
        Measurer {
            id: "poll_p95".to_string(),
            area: "daemon".to_string(),
            command: command.to_string(),
            mode: Mode::Pr,
            timeout_s: 30,
        }
    }

    #[test]
    fn the_tree_hash_covers_the_working_tree_and_leaves_the_live_index_alone() {
        let root = temp_root("tree-hash");
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("tracked.txt"), "one\n").unwrap();
        run_git(&repo, &["add", "tracked.txt"]);
        run_git(&repo, &["commit", "--quiet", "-m", "first"]);
        fs::write(repo.join("tracked.txt"), "two\n").unwrap();
        fs::write(repo.join("fresh.txt"), "new\n").unwrap();
        let committed = run_git(&repo, &["rev-parse", "HEAD^{tree}"]);
        let before = run_git(&repo, &["status", "--porcelain"]);

        let hash = tree_hash(&RealExec, &repo, &state).expect("the tree hash must read");

        assert_eq!(hash.len(), 40, "git prints one full tree hash");
        assert_ne!(
            hash, committed,
            "the working tree differs from the committed tree"
        );
        assert_eq!(
            run_git(&repo, &["status", "--porcelain"]),
            before,
            "the live index never moves"
        );
        assert!(
            before.contains("tracked.txt") && before.contains("fresh.txt"),
            "the worktree holds one edit and one new file: {before}"
        );
        assert!(
            run_git(&repo, &["diff", "--cached", "--name-only"]).is_empty(),
            "nothing was staged, before or after"
        );
        assert!(
            !state
                .join(MEASURE_DIR)
                .join(format!("tmp-index-{}", std::process::id()))
                .exists(),
            "the temporary index goes when the hash is read"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_cache_answers_the_same_tree_and_misses_an_edited_measurer() {
        let root = temp_root("cache");
        let records = vec![Record::value("poll_p95", 12.0, "ms", Direction::Lower)];
        let path = cache_path(&root, "borsuk", "aabbccdd", &measurer("aif bench"));

        write_cache(&path, &records).expect("the cache must write");

        assert_eq!(read_cache(&path), Some(records.clone()));
        assert_eq!(
            read_cache(&cache_path(
                &root,
                "borsuk",
                "11223344",
                &measurer("aif bench")
            )),
            None,
            "another tree never reads these records"
        );
        assert_eq!(
            read_cache(&cache_path(
                &root,
                "borsuk",
                "aabbccdd",
                &measurer("aif bench --full")
            )),
            None,
            "an edited command misses the cache of the same tree"
        );
        assert!(path.starts_with(root.join(MEASURE_DIR).join("borsuk").join("aabbccdd")));
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("daemon-"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compare_names_each_of_the_six_states() {
        let before = vec![
            Record::value("poll_p95", 12.0, "ms", Direction::Lower),
            Record::value("faster", 14.0, "ms", Direction::Lower),
            Record::value("hits", 3.0, "n", Direction::Higher),
            Record::value("span", 5.0, "ms", Direction::Lower),
            Record::value("gone", 1.0, "n", Direction::Lower),
        ];
        let after = vec![
            Record::value("poll_p95", 14.0, "ms", Direction::Lower),
            Record::value("faster", 12.0, "ms", Direction::Lower),
            Record::value("hits", 3.0, "n", Direction::Higher),
            Record::value("span", 5.0, "s", Direction::Lower),
            Record::value("brand_new", 7.0, "n", Direction::Higher),
        ];

        let rows = compare(&before, &after);

        let states: Vec<(&str, State)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("poll_p95", State::Worsened),
                ("faster", State::Improved),
                ("hits", State::Unchanged),
                ("span", State::Incomparable),
                ("brand_new", State::New),
                ("gone", State::Resolved),
            ]
        );
        assert_eq!(rows[3].reason, "the unit changed from \"ms\" to \"s\"");
        assert_eq!(
            rows[0].to_string(),
            "poll_p95  12 \u{2192} 14  ms  worsened"
        );
        assert_eq!(rows[4].to_string(), "brand_new  none \u{2192} 7  n  new");
        assert_eq!(rows[5].to_string(), "gone  1 \u{2192} none  n  resolved");
        assert_eq!(
            rows[3].to_string(),
            "span  incomparable: the unit changed from \"ms\" to \"s\""
        );
    }

    #[test]
    fn a_broken_record_on_either_side_is_one_incomparable_row() {
        let rows = compare(
            &[Record::incomparable("poll_p95", "timeout")],
            &[Record::value("poll_p95", 14.0, "ms", Direction::Lower)],
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, State::Incomparable);
        assert_eq!(rows[0].reason, "timeout");
    }

    #[test]
    fn the_policy_only_observes_and_the_agent_text_names_the_area() {
        let rows = compare(
            &[Record::value("poll_p95", 12.0, "ms", Direction::Lower)],
            &[Record::value("poll_p95", 14.0, "ms", Direction::Lower)],
        );

        assert_eq!(rows[0].state, State::Worsened);
        assert_eq!(
            apply_policy("daemon", &rows),
            MeasureVerdict::Pass,
            "every area observes until C25 builds the policies"
        );
        let areas = vec![("daemon".to_string(), rows)];
        assert_eq!(
            agent_text(&areas),
            "AREA daemon\npoll_p95  12 \u{2192} 14  ms  worsened"
        );
        assert_eq!(
            comparison_block(&areas),
            concat!(
                "<aif-measure-v1>\n",
                "AREA daemon\n",
                "poll_p95  12 \u{2192} 14  ms  worsened\n",
                "</aif-measure-v1>",
            )
        );
        assert_eq!(agent_text(&[]), "none");
    }

    #[test]
    fn a_run_finishes_when_every_slot_holds_its_records_and_reports_per_area() {
        let mut run = MeasureRun {
            base: vec![MeasureSlot {
                id: "borsuk/measure-aabbccdd-daemon-poll_p95".to_string(),
                area: "daemon".to_string(),
                records: Some(vec![Record::value(
                    "poll_p95",
                    12.0,
                    "ms",
                    Direction::Lower,
                )]),
            }],
            head: vec![MeasureSlot {
                id: "borsuk/measure-11223344-daemon-poll_p95".to_string(),
                area: "daemon".to_string(),
                records: None,
            }],
        };
        assert!(!run.finished());
        assert!(run.holds("borsuk/measure-11223344-daemon-poll_p95"));
        assert!(!run.holds("borsuk/review-p7"));
        assert!(!run.settle("borsuk/review-p7", Vec::new()));

        assert!(run.settle(
            "borsuk/measure-11223344-daemon-poll_p95",
            vec![Record::value("poll_p95", 14.0, "ms", Direction::Lower)],
        ));

        assert!(run.finished());
        assert!(
            !run.holds("borsuk/measure-11223344-daemon-poll_p95"),
            "a settled slot waits for nothing"
        );
        let report = run.report();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, "daemon");
        assert_eq!(report[0].1[0].state, State::Worsened);
    }

    #[test]
    fn parse_lines_keeps_the_good_records_and_names_every_broken_one() {
        let text = concat!(
            "{\"id\":\"poll_p95\",\"value\":12.5,\"unit\":\"ms\",\"direction\":\"lower\"}\n",
            "not json at all\n",
            "{\"id\":\"hits\",\"value\":3,\"unit\":\"n\"}\n",
            "\n",
            "{\"id\":\"rate\",\"value\":1,\"unit\":\"n\",\"direction\":\"sideways\"}\n",
            "{\"id\":\"span\",\"value\":\"12ms\",\"unit\":\"ms\",\"direction\":\"lower\"}\n",
        );

        let records = parse_lines(text);

        assert_eq!(records.len(), 5);
        assert_eq!(
            records[0],
            Record::value("poll_p95", 12.5, "ms", Direction::Lower)
        );
        assert_eq!(
            records[1],
            Record::incomparable("", "the line is not a measure record")
        );
        assert_eq!(
            records[2],
            Record::incomparable("hits", "the record has no direction")
        );
        assert_eq!(
            records[3],
            Record::incomparable("rate", "unknown direction \"sideways\"")
        );
        assert_eq!(
            records[4],
            Record::incomparable("span", "value \"12ms\" is not a number")
        );
        assert!(parse_lines("").is_empty());
    }

    #[test]
    fn the_block_names_every_record_and_every_reason() {
        let records = vec![
            exit_record("checkout", 0),
            Record::incomparable("poll_p95", "timeout"),
        ];

        assert_eq!(
            block(&records),
            concat!(
                "<aif-measure-v1>\n",
                "checkout  0 exit  lower\n",
                "poll_p95  incomparable: timeout\n",
                "</aif-measure-v1>",
            )
        );
    }

    #[test]
    fn the_task_ids_carry_the_tree_and_the_feature_reads_back() {
        assert_eq!(
            fast_id("borsuk", "aabbccdd", "checkout"),
            "borsuk/fast-aabbccdd-checkout"
        );
        assert_eq!(
            measure_id("borsuk", "aabbccdd", "web-checkout", "poll_p95"),
            "borsuk/measure-aabbccdd-web-checkout-poll_p95"
        );
        assert_eq!(
            fast_feature("borsuk/fast-aabbccdd-check-out"),
            Some("check-out")
        );
        assert_eq!(fast_feature("borsuk/review-p5"), None);
        assert_eq!(fast_feature("borsuk/measure-aabbccdd-web-poll"), None);
    }

    #[test]
    fn a_run_finishes_only_when_every_record_landed_and_names_the_first_failure() {
        let mut run = FastRun::new(
            vec!["a".to_string(), "b".to_string()],
            "aaa11111".to_string(),
        );
        assert!(!run.finished());
        assert_eq!(run.failure(), None);

        run.records
            .insert("b".to_string(), exit_record("orders", 1));
        assert!(!run.finished());
        assert_eq!(run.failure(), Some(&exit_record("orders", 1)));

        run.records
            .insert("a".to_string(), exit_record("checkout", 0));
        assert!(run.finished());
        assert_eq!(
            run.failure(),
            Some(&exit_record("orders", 1)),
            "the queue order decides, not the map order"
        );

        run.records
            .insert("b".to_string(), exit_record("orders", 0));
        assert_eq!(run.failure(), None);
    }
}
