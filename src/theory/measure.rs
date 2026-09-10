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

use serde::{Deserialize, Serialize};

use super::records::{close_tag, MEASURE_BLOCK};

/// The unit of a record whose value is a process exit code.
pub const EXIT_UNIT: &str = "exit";

/// The task id prefix of one fast check, after the repository alias.
const FAST_PREFIX: &str = "/fast-";

/// The task id prefix of one measurer run, after the repository alias.
const MEASURE_PREFIX: &str = "/measure-";

/// How many characters of the tree hash a task id carries.
pub const TREE_CHARS: usize = 8;

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
    /// The record of each fast task that ended, by task id.
    pub records: BTreeMap<String, Record>,
}

impl FastRun {
    /// One run over the given fast task ids.
    pub fn new(tasks: Vec<String>) -> Self {
        FastRun {
            tasks,
            records: BTreeMap::new(),
        }
    }

    /// True when every fast task of the run reported its record.
    pub fn finished(&self) -> bool {
        self.tasks.iter().all(|id| self.records.contains_key(id))
    }

    /// The first record that did not report a zero value, in queue order.
    pub fn failure(&self) -> Option<&Record> {
        self.tasks
            .iter()
            .filter_map(|id| self.records.get(id))
            .find(|record| record.value != Some(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut run = FastRun::new(vec!["a".to_string(), "b".to_string()]);
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
