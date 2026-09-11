//! The theory cadences: the persisted schedules and their due moments.
//!
//! One governed repository carries one schedule of each kind. The daemon
//! persists the schedules in `state.json` next to `last_fire_ms`, fires
//! the ones whose moment passed, and stamps `last_ms` at every fire. The
//! daily kind runs the sweep that derives the calibration share, the rung
//! counts, the events of the last day, and the stale entries.

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::config::{Config, Weekday};
use crate::theory::records::{DeltaBlock, DeltaOutcome, PredictionTag, LADDER_LABELS};

/// One day in milliseconds.
pub(crate) const MS_PER_DAY: u64 = 86_400_000;

/// What one cadence runs when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    /// The daily sweep of the theory records.
    Daily,
    /// The weekly interview.
    Interview,
    /// The audit sweep after `sweep.days` days.
    Audit,
}

/// One persisted cadence of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// What runs when the cadence fires.
    pub kind: ScheduleKind,
    /// The repository alias the cadence belongs to.
    pub repo: String,
    /// The last fire, in milliseconds. `None` before the first fire.
    pub last_ms: Option<u64>,
}

/// The moment one schedule fires next, from its last fire.
///
/// The `Daily` kind fires at the first moment at or after the UTC
/// midnight that follows `last_ms`; the `Audit` kind fires `sweep.days`
/// days after `last_ms`; the `Interview` kind fires at the UTC midnight
/// of `interview.weekday` in the week after `last_ms`. A schedule with
/// no last fire is due at once. `None` when the schedule names no
/// configured repository.
pub fn schedule_due(schedule: &Schedule, config: &Config, now: u64) -> Option<u64> {
    let theory = &config.repos.get(&schedule.repo)?.theory;
    let Some(last) = schedule.last_ms else {
        return Some(now);
    };
    Some(match schedule.kind {
        ScheduleKind::Daily => next_utc_midnight(last),
        ScheduleKind::Audit => last.saturating_add(theory.sweep.days.saturating_mul(MS_PER_DAY)),
        ScheduleKind::Interview => next_weekday_midnight(last, theory.interview.weekday),
    })
}

/// The next fire moment over every schedule, in milliseconds.
///
/// This is the one value `next_deadline` adds to its wake computation.
pub fn due(schedules: &[Schedule], config: &Config, now: u64) -> Option<u64> {
    schedules
        .iter()
        .filter_map(|schedule| schedule_due(schedule, config, now))
        .min()
}

/// The first UTC midnight strictly after `last_ms`.
fn next_utc_midnight(last_ms: u64) -> u64 {
    (last_ms / MS_PER_DAY + 1) * MS_PER_DAY
}

/// The first UTC midnight of `weekday` strictly after `last_ms`.
///
/// The walk runs seven steps at most: a week holds every weekday once,
/// so a longer walk can name no moment.
fn next_weekday_midnight(last_ms: u64, weekday: Weekday) -> u64 {
    let mut day = last_ms / MS_PER_DAY + 1;
    for _ in 0..7 {
        if day_of_week(day) == weekday {
            return day * MS_PER_DAY;
        }
        day += 1;
    }
    day * MS_PER_DAY
}

/// The day of week of one day count since the epoch, Monday is 0.
fn day_of_week(days: u64) -> Weekday {
    // 1970-01-01, day 0 of the count, was a Thursday.
    match (days + 3) % 7 {
        0 => Weekday::Monday,
        1 => Weekday::Tuesday,
        2 => Weekday::Wednesday,
        3 => Weekday::Thursday,
        4 => Weekday::Friday,
        5 => Weekday::Saturday,
        _ => Weekday::Sunday,
    }
}

/// What one daily sweep derived for one repository.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepStats {
    /// The sure hits over the sure slots; `None` with no sure slot.
    pub calibration: Option<f64>,
    /// The record count that carries each ladder label.
    pub rungs: [usize; 3],
    /// The theory events of the last day.
    pub events_per_day: usize,
    /// The model entries no change touched in the stale window.
    pub stale_entries: Vec<String>,
}

/// The calibration tally of one sweep: the sure slots and their hits.
///
/// A slot whose block carries no tag reads the tag of the record's own
/// full prediction: the prediction of the same record named the entry,
/// and the slot of that prediction carries the operator's confidence. A
/// slot with neither falls out of the tally. An unsure slot counts on
/// neither side: only a sure prediction can prove the model wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Calibration {
    sure: usize,
    hits: usize,
}

impl Calibration {
    /// Add the delta blocks of one record.
    ///
    /// `tags` maps a slot name to the confidence tag of the slot of
    /// the same name in the record's own full prediction. The tag of
    /// the map wins; the tag of the block, which the parser reads as
    /// `unsure` when the block carries none, falls back.
    pub fn add(&mut self, deltas: &[DeltaBlock], tags: &BTreeMap<String, PredictionTag>) {
        for delta in deltas {
            for slot in &delta.slots {
                if tags.get(&slot.id).copied().unwrap_or(slot.tag) == PredictionTag::Sure {
                    self.sure += 1;
                    if slot.outcome == DeltaOutcome::Hit {
                        self.hits += 1;
                    }
                }
            }
        }
    }

    /// The sure hits over the sure slots; `None` with no sure slot.
    pub fn share(&self) -> Option<f64> {
        (self.sure > 0).then(|| self.hits as f64 / self.sure as f64)
    }
}

/// The count of records that carry each ladder label.
pub fn rung_counts(labels_of_records: &[&[String]]) -> [usize; 3] {
    let mut rungs = [0usize; 3];
    for labels in labels_of_records {
        for (rung, name) in LADDER_LABELS.iter().enumerate() {
            if labels.iter().any(|label| label == name) {
                rungs[rung] += 1;
            }
        }
    }
    rungs
}

/// The model entries whose id appears in no hunk of the model log.
///
/// The log names an id only as a whole token: the log that names
/// `INV-10` says nothing about `INV-1`.
pub fn stale_entries(ids: &[String], log: &str) -> Vec<String> {
    ids.iter()
        .filter(|id| !log_names_entry(log, id))
        .cloned()
        .collect()
}

/// True when the log names one id as a whole token.
///
/// A hit needs a boundary on both sides: neither neighbor may carry a
/// character of an id, so the text `INV-10` never matches the id
/// `INV-1`.
fn log_names_entry(log: &str, id: &str) -> bool {
    let boundary = |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_';
    let mut rest = log;
    while let Some(at) = rest.find(id) {
        let after = rest[at + id.len()..].chars().next().is_none_or(boundary);
        let before = rest[..at].chars().next_back().is_none_or(boundary);
        if before && after {
            return true;
        }
        rest = &rest[at + id.len()..];
    }
    false
}

/// True for every label the theory governor puts on a record.
pub(crate) fn is_theory_label(label: &str) -> bool {
    use crate::theory::records::{
        DELTA_OPEN_LABEL, EVENT_OPEN_LABEL, MODEL_PR_LABEL, THEORY_FULL_LABEL, THEORY_SHORT_LABEL,
    };
    matches!(
        label,
        THEORY_SHORT_LABEL
            | THEORY_FULL_LABEL
            | DELTA_OPEN_LABEL
            | EVENT_OPEN_LABEL
            | MODEL_PR_LABEL
    ) || label.starts_with("ladder-")
}

/// Format one moment as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// The form is the exact RFC 3339 form the GitHub `since` parameter
/// reads. The moment must sit after the epoch.
pub(crate) fn ms_rfc3339(ms: u64) -> String {
    let rest = ms % MS_PER_DAY;
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        ms_date(ms),
        rest / 3_600_000,
        (rest % 3_600_000) / 60_000,
        (rest % 60_000) / 1_000,
    )
}

/// Format one moment as the UTC date `YYYY-MM-DD`.
///
/// A rule line carries this date. The moment must sit after the epoch.
pub(crate) fn ms_date(ms: u64) -> String {
    let (year, month, day) = civil_from_days((ms / MS_PER_DAY) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Read one day count as a civil date.
///
/// The inverse of [`days_from_civil`], in Howard Hinnant's form.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Convert one GitHub RFC 3339 timestamp to milliseconds since the epoch.
///
/// The parser accepts the exact `YYYY-MM-DDTHH:MM:SSZ` form that GitHub
/// reports. Any other form gives `None`.
pub(crate) fn rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if !shaped {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    let in_range = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && (0..=23).contains(&hour)
        && (0..=59).contains(&minute)
        && (0..=59).contains(&second);
    if !in_range {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(seconds)
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
}

/// Count the days from 1970-01-01 to one civil date.
///
/// The formula is the proleptic Gregorian day count of Howard
/// Hinnant's date algorithm. An extreme date gives `None`.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theory::records::DeltaSlot;

    /// The smallest config that names one governed repository.
    fn config() -> Config {
        Config::parse(
            r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "m"

[stage.implement]
harness = "claude"
model = "m"

[stage.review]
harness = "claude"
model = "m"

[stage.release]
harness = "claude"
model = "m"

[ticket.create]
harness = "claude"
model = "m"

[ticket.chat]
harness = "claude"
model = "m"

[repo.borsuk]
path = "/tmp/borsuk"
"#,
        )
        .expect("the test config parses")
    }

    /// The smallest config whose interview lands on one weekday.
    fn config_with_interview(weekday: &str) -> Config {
        let toml = format!(
            r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "m"

[stage.implement]
harness = "claude"
model = "m"

[stage.review]
harness = "claude"
model = "m"

[stage.release]
harness = "claude"
model = "m"

[ticket.create]
harness = "claude"
model = "m"

[ticket.chat]
harness = "claude"
model = "m"

[repo.borsuk]
path = "/tmp/borsuk"

[repo.borsuk.interview]
weekday = "{weekday}"
"#
        );
        Config::parse(&toml).expect("the test config parses")
    }

    fn schedule(kind: ScheduleKind, last_ms: Option<u64>) -> Schedule {
        Schedule {
            kind,
            repo: "borsuk".to_string(),
            last_ms,
        }
    }

    #[test]
    fn due_without_a_last_fire_is_due_at_once() {
        let config = config();
        let now = 1_788_091_200_000;

        assert_eq!(
            due(&[schedule(ScheduleKind::Daily, None)], &config, now),
            Some(now)
        );
        assert_eq!(
            due(
                &[
                    schedule(ScheduleKind::Interview, None),
                    schedule(ScheduleKind::Audit, None),
                ],
                &config,
                now
            ),
            Some(now)
        );
    }

    #[test]
    fn due_after_a_noon_fire_is_due_at_the_next_utc_midnight() {
        let config = config();
        let noon = rfc3339_ms("2026-09-10T12:00:00Z").unwrap();
        let midnight = rfc3339_ms("2026-09-11T00:00:00Z").unwrap();

        assert_eq!(
            due(&[schedule(ScheduleKind::Daily, Some(noon))], &config, noon),
            Some(midnight)
        );
    }

    #[test]
    fn due_audit_waits_for_the_sweep_days_and_the_interview_for_its_weekday() {
        let config = config();
        let thursday_noon = rfc3339_ms("2026-09-10T12:00:00Z").unwrap();
        let next_monday = rfc3339_ms("2026-09-14T00:00:00Z").unwrap();
        let week_later = thursday_noon + 7 * MS_PER_DAY;

        assert_eq!(
            due(
                &[schedule(ScheduleKind::Audit, Some(thursday_noon))],
                &config,
                thursday_noon
            ),
            Some(week_later)
        );
        assert_eq!(
            due(
                &[schedule(ScheduleKind::Interview, Some(thursday_noon))],
                &config,
                thursday_noon
            ),
            Some(next_monday),
            "the interview waits for the midnight of the next Monday"
        );
    }

    #[test]
    fn the_day_of_week_names_all_seven_days_of_the_week() {
        // Day 0, 1970-01-01, was a Thursday; the weekend sits in the
        // middle of the first week of the count.
        let days = [
            (0, Weekday::Thursday),
            (1, Weekday::Friday),
            (2, Weekday::Saturday),
            (3, Weekday::Sunday),
            (4, Weekday::Monday),
            (5, Weekday::Tuesday),
            (6, Weekday::Wednesday),
            (7, Weekday::Thursday),
            (9, Weekday::Saturday),
            (10, Weekday::Sunday),
        ];
        for (day, want) in days {
            assert_eq!(day_of_week(day), want, "day {day} of the count");
        }
    }

    #[test]
    fn ms_rfc3339_formats_the_moment_the_parser_reads_back() {
        let midnight = 1_789_084_800_000;
        assert_eq!(ms_rfc3339(midnight), "2026-09-11T00:00:00Z");
        assert_eq!(rfc3339_ms(&ms_rfc3339(midnight)), Some(midnight));

        let noon = 1_789_128_000_000;
        assert_eq!(ms_rfc3339(noon), "2026-09-11T12:00:00Z");
        assert_eq!(rfc3339_ms(&ms_rfc3339(noon)), Some(noon));
    }

    #[test]
    fn due_interview_waits_for_saturday_and_sunday() {
        let thursday_noon = rfc3339_ms("2026-09-10T12:00:00Z").unwrap();
        let saturday = rfc3339_ms("2026-09-12T00:00:00Z").unwrap();
        let sunday = rfc3339_ms("2026-09-13T00:00:00Z").unwrap();

        let config = config_with_interview("saturday");
        assert_eq!(
            due(
                &[schedule(ScheduleKind::Interview, Some(thursday_noon))],
                &config,
                thursday_noon
            ),
            Some(saturday),
            "the interview waits for the midnight of the Saturday"
        );

        let config = config_with_interview("sunday");
        assert_eq!(
            due(
                &[schedule(ScheduleKind::Interview, Some(thursday_noon))],
                &config,
                thursday_noon
            ),
            Some(sunday),
            "the interview waits for the midnight of the Sunday"
        );
    }

    #[test]
    fn due_names_no_moment_for_an_unknown_repository_and_the_earliest_of_all() {
        let config = config();
        let now = 1_000;
        let other = Schedule {
            kind: ScheduleKind::Daily,
            repo: "elsewhere".to_string(),
            last_ms: Some(now),
        };

        assert_eq!(due(&[other], &config, now), None);
    }

    #[test]
    fn calibration_counts_sure_hits_over_sure_slots_and_reads_the_prediction_tags() {
        let slot = |id: &str, outcome: DeltaOutcome, tag: PredictionTag| DeltaSlot {
            id: id.to_string(),
            outcome,
            tag,
        };
        let delta = |slots: Vec<DeltaSlot>| DeltaBlock {
            slots,
            touched: Vec::new(),
            violations: Vec::new(),
            question: String::new(),
        };
        // Three sure hits, one sure miss, and one unsure miss.
        let block = delta(vec![
            slot("behaviours", DeltaOutcome::Hit, PredictionTag::Sure),
            slot("states", DeltaOutcome::Hit, PredictionTag::Sure),
            slot("invariants", DeltaOutcome::Hit, PredictionTag::Sure),
            slot("failure-modes", DeltaOutcome::Miss, PredictionTag::Sure),
            slot("other-areas", DeltaOutcome::Miss, PredictionTag::Unsure),
        ]);

        let mut calibration = Calibration::default();
        calibration.add(std::slice::from_ref(&block), &BTreeMap::new());
        assert_eq!(
            calibration.share(),
            Some(0.75),
            "the unsure slot counts on neither side"
        );

        // A slot tag of the block holds only when the record's own full
        // prediction names no slot of the same id; the tag of the
        // prediction wins.
        let untagged = delta(vec![
            slot("behaviours", DeltaOutcome::Hit, PredictionTag::Unsure),
            slot("invariants", DeltaOutcome::Miss, PredictionTag::Sure),
        ]);
        let mut tags = BTreeMap::new();
        tags.insert("behaviours".to_string(), PredictionTag::Sure);

        let mut calibration = Calibration::default();
        calibration.add(&[untagged], &tags);
        assert_eq!(
            calibration.share(),
            Some(0.5),
            "the map tag counts the hit; the block tag counts the miss"
        );

        let mut calibration = Calibration::default();
        calibration.add(&[], &BTreeMap::new());
        assert_eq!(calibration.share(), None, "no sure slot gives no share");
    }

    #[test]
    fn rung_counts_read_the_ladder_labels_of_every_record() {
        let labels =
            |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };
        let records = [
            labels(&["ladder-1"]),
            labels(&["theory-short", "ladder-2"]),
            labels(&["ladder-2"]),
            labels(&[]),
        ];

        assert_eq!(
            rung_counts(&[&records[0], &records[1], &records[2], &records[3]]),
            [1, 2, 0]
        );
    }

    #[test]
    fn stale_entries_names_the_ids_the_log_never_touched() {
        let ids: Vec<String> = ["INV-3", "INV-9"].iter().map(|id| id.to_string()).collect();
        let log = "+INV-3 stays true on reload\n-INV-3 old form\n";

        assert_eq!(stale_entries(&ids, log), vec!["INV-9".to_string()]);

        // The id matches as a whole token: a log line that names INV-10
        // says nothing about INV-1.
        let ids: Vec<String> = ["INV-1", "INV-10"]
            .iter()
            .map(|id| id.to_string())
            .collect();
        let log = "+id = \"INV-10\" flips\n";

        assert_eq!(stale_entries(&ids, log), vec!["INV-1".to_string()]);
    }
}
