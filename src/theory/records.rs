//! The theory records: the labels, the block tags, and the event blocks.
//!
//! A theory record is the GitHub issue that holds the theory state of one
//! item. [`RecordKey`] names the item; the daemon resolves it to an
//! `(owner_repo, number)` pair. Agents and the daemon ship facts as tagged
//! blocks, and [`parse_event_blocks`] reads the event blocks back.

use serde::{Deserialize, Serialize};

/// The label that marks a ticket with an accepted short prediction.
pub const THEORY_SHORT_LABEL: &str = "theory-short";
/// The label that marks a ticket with an accepted full prediction.
pub const THEORY_FULL_LABEL: &str = "theory-full";
/// The label that marks a record with an open delta.
pub const DELTA_OPEN_LABEL: &str = "delta-open";
/// The label that marks a record with an open theory event.
pub const EVENT_OPEN_LABEL: &str = "event-open";
/// The label that marks a pull request that changes the model.
pub const MODEL_PR_LABEL: &str = "model-pr";

/// The color GitHub renders `event-open` with, as six hex digits.
pub const EVENT_OPEN_COLOR: &str = "d4c5f9";

/// The label that marks a run skill ticket and its pull request.
pub const VERIFY_SKILL_LABEL: &str = "verify-skill";

/// The color GitHub renders `verify-skill` with, as six hex digits.
pub const VERIFY_SKILL_COLOR: &str = "0e8a16";

/// The opening tag of one prediction block.
pub const PREDICTION_BLOCK: &str = "<aif-prediction-v1>";
/// The opening tag of one theory event block.
pub const EVENT_BLOCK: &str = "<aif-event-v1>";
/// The opening tag of one measure block.
pub const MEASURE_BLOCK: &str = "<aif-measure-v1>";
/// The opening tag of one answer block.
pub const ANSWER_BLOCK: &str = "<aif-answer-v1>";

/// The item whose theory record the daemon talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordKey {
    /// The record of one issue.
    Issue(u64),
    /// The record of one pull request.
    Pr(u64),
    /// The record of the whole repository.
    Repo,
}

/// One theory event, as one `<aif-event-v1>` block holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// What kind of event this is, for example `violation`.
    pub kind: String,
    /// What the agent observed, in one sentence.
    pub text: String,
    /// The area the event names, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// The GitHub number the event names, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
}

/// Render one event as a complete `<aif-event-v1>` block.
///
/// The block carries the event as one JSON line between the tags, so a
/// transcript holds it as plain text and [`parse_event_blocks`] reads it
/// back.
pub fn event_block(event: &Event) -> String {
    let body = serde_json::to_string(event).expect("an event serializes");
    let close = close_tag(EVENT_BLOCK);
    format!("{EVENT_BLOCK}\n{body}\n{close}")
}

/// Parse every complete `<aif-event-v1>` block of one transcript.
///
/// A block whose body does not parse as an event, and a block with no
/// closing tag, is skipped. The order of the blocks is kept.
pub fn parse_event_blocks(text: &str) -> Vec<Event> {
    let close = close_tag(EVENT_BLOCK);
    let mut events = Vec::new();
    let mut rest = text;
    'scan: while let Some(start) = rest.find(EVENT_BLOCK) {
        let after_open = &rest[start + EVENT_BLOCK.len()..];
        let Some(end) = after_open.find(&close) else {
            break;
        };
        let span = &after_open[..end];
        if let Some(next_open) = span.find(EVENT_BLOCK) {
            // The opening tag is truncated: the later tag owns the close,
            // so the scan restarts there. The restart reads from
            // `after_open`, because `span` ends before the close.
            rest = &after_open[next_open..];
            continue 'scan;
        }
        let body = span.trim();
        rest = &after_open[end + close.len()..];
        if let Ok(event) = serde_json::from_str::<Event>(body) {
            events.push(event);
        }
    }
    events
}

/// The closing tag of one opening tag: `<aif-event-v1>` closes as
/// `</aif-event-v1>`.
fn close_tag(open: &str) -> String {
    format!("</{}>", open.trim_start_matches('<').trim_end_matches('>'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event with every field set.
    fn full_event() -> Event {
        Event {
            kind: "violation".to_string(),
            text: "INV-3 broke: the poller never parked.".to_string(),
            area: Some("poll".to_string()),
            number: Some(142),
        }
    }

    #[test]
    fn event_block_round_trips_through_the_parser() {
        let event = full_event();
        let block = event_block(&event);
        assert!(block.starts_with(EVENT_BLOCK));
        assert!(block.ends_with("</aif-event-v1>"));
        assert_eq!(parse_event_blocks(&block), vec![event]);
    }

    #[test]
    fn parse_event_blocks_reads_two_blocks_and_keeps_the_order() {
        let first = full_event();
        let second = Event {
            kind: "miss".to_string(),
            text: "FM-2 missed its fast check.".to_string(),
            area: None,
            number: None,
        };
        let transcript = format!(
            "prose before\n{}\nprose between\n{}\nprose after",
            event_block(&first),
            event_block(&second)
        );
        assert_eq!(parse_event_blocks(&transcript), vec![first, second]);
    }

    #[test]
    fn parse_event_blocks_ignores_text_without_a_block() {
        assert!(parse_event_blocks("no blocks here").is_empty());
        assert!(parse_event_blocks("").is_empty());
    }

    #[test]
    fn parse_event_blocks_skips_a_broken_block_and_keeps_the_next() {
        let good = full_event();
        let close = close_tag(EVENT_BLOCK);
        let transcript = format!(
            "{EVENT_BLOCK}\n{{not json}}\n{close}\n{}",
            event_block(&good)
        );
        assert_eq!(parse_event_blocks(&transcript), vec![good]);
    }

    #[test]
    fn parse_event_blocks_skips_a_block_with_no_closing_tag() {
        let transcript = format!("{EVENT_BLOCK}\n{{}}\nand the agent stopped here");
        assert!(parse_event_blocks(&transcript).is_empty());
    }

    #[test]
    fn parse_event_blocks_skips_a_truncated_block_before_a_good_block() {
        let good = full_event();
        let transcript = format!(
            "{EVENT_BLOCK}\n{{\"kind\":\"miss\" and the agent stopped mid-block\n{}",
            event_block(&good)
        );
        assert_eq!(parse_event_blocks(&transcript), vec![good]);
    }

    #[test]
    fn an_event_without_optional_fields_parses() {
        let transcript = format!(
            "{EVENT_BLOCK}\n{{\"kind\":\"miss\",\"text\":\"late\"}}\n{}",
            close_tag(EVENT_BLOCK)
        );
        assert_eq!(
            parse_event_blocks(&transcript),
            vec![Event {
                kind: "miss".to_string(),
                text: "late".to_string(),
                area: None,
                number: None,
            }]
        );
    }
}
