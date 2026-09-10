//! The operator's answer to one theory row, and the rows one record opens.
//!
//! A delta with a miss or a violation opens one row per miss and one per
//! violation. A delta with only hits opens one confirmation row. An
//! `event-open` record opens one row per event block. Each row carries a
//! slot key, and the answer of the row posts an `<aif-answer-v1>` block
//! that names the same key. The daemon closes the label of a record once
//! every key of the record has an answer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::records::{close_tag, scan_block_bodies, DeltaBlock, DeltaOutcome, ANSWER_BLOCK};
use crate::decisions::Decision;
use crate::model::ItemKind;
use crate::sock::RecordView;

/// The slot key of the confirmation row of a hit-only delta.
pub const DELTA_SLOT: &str = "delta";

/// The `source` field of a row that a missed slot opened.
pub const SOURCE_MISS: &str = "miss";
/// The `source` field of a row that a broken model rule opened.
pub const SOURCE_VIOLATION: &str = "violation";
/// The `source` field of a row that an event block opened.
pub const SOURCE_EVENT: &str = "event";

/// Why one miss or one violation happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The entry is wrong or absent, so the operator edits the model.
    Model,
    /// The entry is right and the pull request broke it.
    Pr,
    /// The entry is right and present, and the operator forgot it.
    Recall,
}

impl Cause {
    /// The lowercase name the block writes.
    pub fn word(self) -> &'static str {
        match self {
            Cause::Model => "model",
            Cause::Pr => "pr",
            Cause::Recall => "recall",
        }
    }
}

/// One `<aif-answer-v1>` block: the operator's answer to one row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerBlock {
    /// Why the miss or the violation happened.
    pub cause: Cause,
    /// The model entry the operator named.
    pub entry: String,
    /// The rung of the ladder, 1, 2, or 3.
    pub rung: u8,
    /// The area the entry belongs to, empty when it maps to none.
    pub area: String,
    /// What the operator added, empty when nothing.
    #[serde(default)]
    pub note: String,
    /// The slot key of the row this answer closes.
    pub slot: String,
    /// The answer time in milliseconds since the Unix epoch.
    ///
    /// A `model` answer carries the time, because the daemon reads the
    /// git history of the model file after it. Every other cause writes
    /// zero.
    #[serde(default)]
    pub answered_ms: u64,
}

/// Render one answer as a complete `<aif-answer-v1>` block.
pub fn answer_block(answer: &AnswerBlock) -> String {
    let body = serde_json::to_string(answer).expect("an answer serializes");
    let close = close_tag(ANSWER_BLOCK);
    format!("{ANSWER_BLOCK}\n{body}\n{close}")
}

/// Parse every complete `<aif-answer-v1>` block of one text.
///
/// A block whose body does not parse as an answer, and a block with no
/// closing tag, is skipped. The order of the blocks is kept.
pub fn parse_answer_blocks(text: &str) -> Vec<AnswerBlock> {
    scan_block_bodies(text, ANSWER_BLOCK)
        .into_iter()
        .filter_map(|body| serde_json::from_str::<AnswerBlock>(body).ok())
        .collect()
}

/// The slot key of the event at `index` of one record.
pub fn event_slot(index: usize) -> String {
    format!("{SOURCE_EVENT}:{index}")
}

/// The slot keys the misses and the violations of one delta open.
///
/// A delta with only hits opens no key here: it closes on the operator's
/// confirmation instead, and the confirmation writes [`DELTA_SLOT`].
pub fn delta_slots(delta: &DeltaBlock) -> Vec<String> {
    let mut slots: Vec<String> = delta
        .slots
        .iter()
        .filter(|slot| slot.outcome == DeltaOutcome::Miss)
        .map(|slot| slot.id.clone())
        .collect();
    slots.extend(
        delta
            .violations
            .iter()
            .map(|one| format!("{SOURCE_VIOLATION}:{}", one.entry)),
    );
    if slots.is_empty() {
        return vec![DELTA_SLOT.to_string()];
    }
    slots
}

/// The slot keys the event blocks of one record open.
pub fn event_slots(view: &RecordView) -> Vec<String> {
    (0..view.events.len()).map(event_slot).collect()
}

/// The answer of one slot key of one record, when the record holds one.
pub fn answer_of<'a>(view: &'a RecordView, slot: &str) -> Option<&'a AnswerBlock> {
    view.answers.iter().find(|answer| answer.slot == slot)
}

/// True when every key of `slots` has an answer on the record.
pub fn answered(view: &RecordView, slots: &[String]) -> bool {
    slots.iter().all(|slot| answer_of(view, slot).is_some())
}

/// The `model` answers of `slots`, in slot order.
///
/// Each one waits for a change to the model file that names its entry.
pub fn model_answers<'a>(view: &'a RecordView, slots: &[String]) -> Vec<&'a AnswerBlock> {
    slots
        .iter()
        .filter_map(|slot| answer_of(view, slot))
        .filter(|answer| answer.cause == Cause::Model)
        .collect()
}

/// The inbox rows the delta of one record opens this poll.
///
/// A hit-only delta opens one `DELTA` row. Every other delta opens one
/// `THEORY` row per miss and one per violation. A row whose slot key
/// already carries an answer opens nothing. `predicted` holds the entries
/// the full prediction named for each slot; a slot it leaves empty names
/// itself.
pub fn delta_rows(
    repo: &str,
    kind: ItemKind,
    number: u64,
    view: &RecordView,
    predicted: &BTreeMap<String, Vec<String>>,
    opened_ms: u64,
) -> Vec<Decision> {
    let Some(delta) = view.delta.as_ref() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for slot in delta_slots(delta) {
        if answer_of(view, &slot).is_some() {
            continue;
        }
        if slot == DELTA_SLOT {
            let hits = delta
                .slots
                .iter()
                .filter(|one| one.outcome == DeltaOutcome::Hit)
                .count();
            rows.push(Decision::delta_hit(repo, kind, number, hits, opened_ms));
            continue;
        }
        rows.push(match slot.strip_prefix(&format!("{SOURCE_VIOLATION}:")) {
            Some(entry) => {
                let finding = delta
                    .violations
                    .iter()
                    .find(|one| one.entry == entry)
                    .map(|one| one.finding.clone())
                    .unwrap_or_default();
                Decision::theory_event(
                    repo,
                    TheoryRow {
                        kind,
                        number,
                        slot: slot.clone(),
                        entry: entry.to_string(),
                        tag: String::new(),
                        question: finding,
                        source: SOURCE_VIOLATION.to_string(),
                    },
                    opened_ms,
                )
            }
            None => {
                let missed = delta.slots.iter().find(|one| one.id == slot);
                let tag = missed.map(super::records::slot_outcome).unwrap_or_default();
                Decision::theory_event(
                    repo,
                    TheoryRow {
                        kind,
                        number,
                        slot: slot.clone(),
                        entry: predicted_entries(predicted, &slot).unwrap_or_else(|| slot.clone()),
                        tag,
                        question: delta.question.clone(),
                        source: SOURCE_MISS.to_string(),
                    },
                    opened_ms,
                )
            }
        });
    }
    rows
}

/// The entries the full prediction named for one slot, as one line.
fn predicted_entries(predicted: &BTreeMap<String, Vec<String>>, slot: &str) -> Option<String> {
    let named = predicted.get(slot)?;
    (!named.is_empty()).then(|| named.join(", "))
}

/// The inbox rows the event blocks of one record open this poll.
pub fn event_rows(
    repo: &str,
    kind: ItemKind,
    number: u64,
    view: &RecordView,
    opened_ms: u64,
) -> Vec<Decision> {
    view.events
        .iter()
        .enumerate()
        .filter(|(index, _)| answer_of(view, &event_slot(*index)).is_none())
        .map(|(index, event)| {
            Decision::theory_event(
                repo,
                TheoryRow {
                    kind,
                    number,
                    slot: event_slot(index),
                    entry: event.area.clone().unwrap_or_default(),
                    tag: event.kind.clone(),
                    question: event.text.clone(),
                    source: SOURCE_EVENT.to_string(),
                },
                opened_ms,
            )
        })
        .collect()
}

/// The parts of one `THEORY` row, as [`Decision::theory_event`] takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TheoryRow {
    /// Whether the record is an issue or a pull request.
    pub kind: ItemKind,
    /// The issue or pull request number of the record.
    pub number: u64,
    /// The slot key the answer of the row writes.
    pub slot: String,
    /// The model entries in scope, empty when the row names none.
    pub entry: String,
    /// The short classifier of the row: the slot outcome of a miss, the
    /// event kind of an event, empty for a violation.
    pub tag: String,
    /// The one question the row asks.
    pub question: String,
    /// Where the row came from: `miss`, `violation`, or `event`.
    pub source: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::DecisionKind;
    use crate::theory::records::{DeltaSlot, DeltaViolation, Event, PredictionTag};

    fn miss_delta() -> DeltaBlock {
        DeltaBlock {
            slots: vec![
                DeltaSlot {
                    id: "behaviours".to_string(),
                    outcome: DeltaOutcome::Hit,
                    tag: PredictionTag::Sure,
                },
                DeltaSlot {
                    id: "invariants".to_string(),
                    outcome: DeltaOutcome::Miss,
                    tag: PredictionTag::Sure,
                },
            ],
            touched: Vec::new(),
            violations: vec![DeltaViolation {
                entry: "INV-3".to_string(),
                finding: "the retry crosses the boundary".to_string(),
            }],
            question: "which entry is wrong?".to_string(),
        }
    }

    fn hit_delta() -> DeltaBlock {
        DeltaBlock {
            slots: vec![DeltaSlot {
                id: "behaviours".to_string(),
                outcome: DeltaOutcome::Hit,
                tag: PredictionTag::Sure,
            }],
            touched: Vec::new(),
            violations: Vec::new(),
            question: String::new(),
        }
    }

    fn answer(slot: &str, cause: Cause) -> AnswerBlock {
        AnswerBlock {
            cause,
            entry: "INV-3".to_string(),
            rung: 2,
            area: "web-checkout".to_string(),
            note: String::new(),
            slot: slot.to_string(),
            answered_ms: 0,
        }
    }

    #[test]
    fn an_answer_block_round_trips_through_its_tags() {
        let one = answer("invariants", Cause::Model);
        let text = format!("a report\n\n{}\n\ntail", answer_block(&one));
        assert_eq!(parse_answer_blocks(&text), vec![one]);
    }

    #[test]
    fn a_hit_only_delta_opens_one_confirmation_row() {
        let view = RecordView {
            delta: Some(hit_delta()),
            ..RecordView::default()
        };
        assert_eq!(delta_slots(&hit_delta()), vec![DELTA_SLOT.to_string()]);
        let rows = delta_rows("borsuk", ItemKind::Pr, 7, &view, &BTreeMap::new(), 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].kind,
            DecisionKind::DeltaHit {
                kind: ItemKind::Pr,
                number: 7,
                hits: 1,
            }
        );
    }

    #[test]
    fn a_delta_opens_one_row_per_miss_and_one_per_violation() {
        let view = RecordView {
            delta: Some(miss_delta()),
            ..RecordView::default()
        };
        let predicted = BTreeMap::from([("invariants".to_string(), vec!["INV-3".to_string()])]);
        let rows = delta_rows("borsuk", ItemKind::Pr, 7, &view, &predicted, 10);
        let sources: Vec<String> = rows
            .iter()
            .map(|row| match &row.kind {
                DecisionKind::TheoryEvent { source, .. } => source.clone(),
                other => panic!("unexpected row {other:?}"),
            })
            .collect();
        assert_eq!(
            sources,
            vec![SOURCE_MISS.to_string(), SOURCE_VIOLATION.to_string()]
        );
        assert_eq!(
            rows[0].kind,
            DecisionKind::TheoryEvent {
                kind: ItemKind::Pr,
                number: 7,
                slot: "invariants".to_string(),
                entry: "INV-3".to_string(),
                tag: "sure-miss".to_string(),
                question: "which entry is wrong?".to_string(),
                source: SOURCE_MISS.to_string(),
            }
        );
    }

    #[test]
    fn an_answered_slot_opens_no_row_and_reads_answered() {
        let mut view = RecordView {
            delta: Some(miss_delta()),
            ..RecordView::default()
        };
        let slots = delta_slots(&miss_delta());
        assert!(!answered(&view, &slots));
        view.answers.push(answer("invariants", Cause::Recall));
        assert_eq!(
            delta_rows("borsuk", ItemKind::Pr, 7, &view, &BTreeMap::new(), 10).len(),
            1
        );
        view.answers.push(answer("violation:INV-3", Cause::Model));
        assert!(delta_rows("borsuk", ItemKind::Pr, 7, &view, &BTreeMap::new(), 10).is_empty());
        assert!(answered(&view, &slots));
        assert_eq!(model_answers(&view, &slots).len(), 1);
    }

    #[test]
    fn each_event_block_opens_one_row_with_the_event_source() {
        let view = RecordView {
            events: vec![Event {
                kind: "floor".to_string(),
                text: "the area asks for a higher tier".to_string(),
                area: Some("web-checkout".to_string()),
                number: Some(7),
                surface: None,
            }],
            ..RecordView::default()
        };
        assert_eq!(event_slots(&view), vec!["event:0".to_string()]);
        let rows = event_rows("borsuk", ItemKind::Issue, 142, &view, 10);
        assert_eq!(
            rows[0].kind,
            DecisionKind::TheoryEvent {
                kind: ItemKind::Issue,
                number: 142,
                slot: "event:0".to_string(),
                entry: "web-checkout".to_string(),
                tag: "floor".to_string(),
                question: "the area asks for a higher tier".to_string(),
                source: SOURCE_EVENT.to_string(),
            }
        );
    }
}
