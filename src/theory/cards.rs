//! The cards of one day: the retrieval questions the sweep derives.
//!
//! Two sources feed the batch. The first is a pull request that merged
//! since the last sweep whose tickets carry no prediction by the
//! operator. The second is spaced repetition over the model: an entry
//! that no change touched in the stale window. The daily sweep builds at
//! most `cards.per_day` cards per repository and holds them in memory,
//! so a restart drops the day's batch until the next sweep.

use serde::{Deserialize, Serialize};

/// The source name of a card that names one merged pull request.
pub const MERGED_PR_SOURCE: &str = "merged-pr";

/// The source name of a card that names one stale model entry.
pub const STALE_ENTRY_SOURCE: &str = "stale-entry";

/// One card of the day, as the Theory view and the inbox show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardView {
    /// Where the card came from: `merged-pr` or `stale-entry`.
    #[serde(default)]
    pub source: String,
    /// The question the card asks.
    #[serde(default)]
    pub prompt: String,
    /// The merged pull request of a `merged-pr` card.
    #[serde(default)]
    pub number: Option<u64>,
    /// The model entry of a `stale-entry` card.
    #[serde(default)]
    pub entry: Option<String>,
}

impl CardView {
    /// The id fragment of one card: the number, else the entry.
    pub fn slug(&self) -> String {
        match (self.number, self.entry.as_deref()) {
            (Some(number), _) => number.to_string(),
            (None, Some(entry)) => entry.to_string(),
            (None, None) => String::new(),
        }
    }
}

/// Build the cards of one day from the two sources.
///
/// `prs` names the merged pull requests with no operator prediction, in
/// merge order; `entries` names the stale model entries. The first
/// source fills the batch first, and the batch stops at `cap`.
pub fn build(prs: &[u64], entries: &[String], cap: usize) -> Vec<CardView> {
    let merged = prs.iter().map(|number| CardView {
        source: MERGED_PR_SOURCE.to_string(),
        prompt: format!("PR #{number} merged. Which entries changed, and how?"),
        number: Some(*number),
        entry: None,
    });
    let stale = entries.iter().map(|entry| CardView {
        source: STALE_ENTRY_SOURCE.to_string(),
        prompt: format!("State {entry}. What would violate it?"),
        number: None,
        entry: Some(entry.clone()),
    });
    merged.chain(stale).take(cap).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_fills_from_the_pull_requests_first_and_stops_at_the_cap() {
        let entries = vec!["INV-9".to_string(), "INV-8".to_string()];
        let cards = build(&[7, 9], &entries, 3);

        assert_eq!(cards.len(), 3, "the cap holds the batch to three cards");
        assert_eq!(
            cards[0],
            CardView {
                source: MERGED_PR_SOURCE.to_string(),
                prompt: "PR #7 merged. Which entries changed, and how?".to_string(),
                number: Some(7),
                entry: None,
            }
        );
        assert_eq!(cards[1].number, Some(9));
        assert_eq!(
            cards[2],
            CardView {
                source: STALE_ENTRY_SOURCE.to_string(),
                prompt: "State INV-9. What would violate it?".to_string(),
                number: None,
                entry: Some("INV-9".to_string()),
            },
            "the stale entries fill what the pull requests left"
        );
    }

    #[test]
    fn build_with_no_source_and_a_zero_cap_yields_no_card() {
        assert!(build(&[], &[], 3).is_empty());
        assert!(build(&[7], &["INV-9".to_string()], 0).is_empty());
    }

    #[test]
    fn the_slug_names_the_number_of_a_pull_request_card_and_the_entry_of_an_entry_card() {
        let cards = build(&[7], &["INV-9".to_string()], 2);
        assert_eq!(cards[0].slug(), "7");
        assert_eq!(cards[1].slug(), "INV-9");
    }
}
