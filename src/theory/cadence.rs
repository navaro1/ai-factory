//! The theory cadences: the persisted schedules and their due moments.
//!
//! One governed repository carries one schedule of each kind. The daemon
//! persists the schedules in `state.json` next to `last_fire_ms`, fires
//! the ones whose moment passed, and stamps `last_ms` at every fire. The
//! daily kind runs the sweep that derives the calibration share, the rung
//! counts, the events of the last day, and the stale entries.

use serde::{Deserialize, Serialize};

use crate::config::{Config, Weekday};
use crate::theory::records::{DeltaBlock, DeltaOutcome, PredictionTag};

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
fn next_weekday_midnight(last_ms: u64, weekday: Weekday) -> u64 {
    let mut day = last_ms / MS_PER_DAY + 1;
    while day_of_week(day) != weekday {
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
        _ => Weekday::Friday,
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

/// The sure hits over the sure slots of the delta blocks.
///
/// An unsure slot counts on neither side: only a sure prediction can
/// prove the model wrong.
pub fn calibration_share(deltas: &[DeltaBlock]) -> Option<f64> {
    let mut sure = 0usize;
    let mut hits = 0usize;
    for delta in deltas {
        for slot in &delta.slots {
            if slot.tag == PredictionTag::Sure {
                sure += 1;
                if slot.outcome == DeltaOutcome::Hit {
                    hits += 1;
                }
            }
        }
    }
    (sure > 0).then(|| hits as f64 / sure as f64)
}

/// The count of records that carry each ladder label.
pub fn rung_counts(labels_of_records: &[&[String]]) -> [usize; 3] {
    let names = ["ladder-1", "ladder-2", "ladder-3"];
    let mut rungs = [0usize; 3];
    for labels in labels_of_records {
        for (rung, name) in names.iter().enumerate() {
            if labels.iter().any(|label| label == name) {
                rungs[rung] += 1;
            }
        }
    }
    rungs
}

/// The model entries whose id appears in no hunk of the model log.
pub fn stale_entries(ids: &[String], log: &str) -> Vec<String> {
    ids.iter()
        .filter(|id| !log.contains(id.as_str()))
        .cloned()
        .collect()
}

/// True when one record takes part in the sweep.
///
/// A record with a parseable update time takes part when the update is
/// in the window. A record without one takes part when it carries a
/// theory label.
pub fn in_sweep_window(updated_at: &str, labels: &[String], cutoff_ms: u64) -> bool {
    match rfc3339_ms(updated_at) {
        Some(updated) => updated >= cutoff_ms,
        None => labels.iter().any(|label| is_theory_label(label)),
    }
}

/// True for every label the theory governor puts on a record.
fn is_theory_label(label: &str) -> bool {
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
    fn calibration_share_counts_sure_hits_over_sure_slots() {
        let slot = |id: &str, outcome: DeltaOutcome, tag: PredictionTag| DeltaSlot {
            id: id.to_string(),
            outcome,
            tag,
        };
        let delta = |slots: Vec<DeltaSlot>| DeltaBlock {
            slots,
            touched: Vec::new(),
            violations: Vec::new(),
            question: None,
        };
        let seven_of_ten = delta(
            (0..7)
                .map(|index| {
                    slot(
                        &format!("INV-{index}"),
                        DeltaOutcome::Hit,
                        PredictionTag::Sure,
                    )
                })
                .chain((7..10).map(|index| {
                    slot(
                        &format!("INV-{index}"),
                        DeltaOutcome::Miss,
                        PredictionTag::Sure,
                    )
                }))
                .collect(),
        );
        let unsure = delta(vec![slot(
            "INV-0",
            DeltaOutcome::Miss,
            PredictionTag::Unsure,
        )]);

        assert_eq!(
            calibration_share(&[seven_of_ten, unsure.clone()]),
            Some(0.7),
            "the unsure slot counts on neither side"
        );
        assert_eq!(calibration_share(&[]), None);
        assert_eq!(calibration_share(&[unsure]), None);
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
    }

    #[test]
    fn the_sweep_window_reads_the_update_time_or_the_labels() {
        let labels: Vec<String> = vec!["ladder-1".to_string()];
        let plain: Vec<String> = vec!["bug".to_string()];

        assert!(in_sweep_window("2026-09-10T00:00:00Z", &plain, 0));
        assert!(!in_sweep_window("2026-09-10T00:00:00Z", &plain, u64::MAX));
        assert!(
            in_sweep_window("", &labels, u64::MAX),
            "a record without a time joins on its theory label"
        );
        assert!(!in_sweep_window("", &plain, 0));
        assert!(!in_sweep_window("not a time", &plain, 0));
    }
}
