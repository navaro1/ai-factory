//! Holds the four stage predicates and the edge-triggered gate tracker.
//!
//! A predicate says which items are ready for a stage right now. The
//! tracker compares the answer with the previous poll and reports
//! `ReadyWork` only on a false to true edge. A gate that stays open
//! across polls reports once. The tracker only reports. It never creates
//! tasks and never touches a release queue; the daemon decides what to do
//! with the report, including the report of the release stage.

use std::collections::BTreeSet;

use crate::model::{Issue, ItemKind, Pr, RepoSnapshot, Stage};

/// The label that asks the factory to shape a raw issue.
pub const TO_REFINE: &str = "to-refine";

/// The label that marks a shaped issue as ready to implement.
pub const REFINED: &str = "refined";

/// Work that a stage gate reports as ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyWork {
    /// The repository alias the item belongs to.
    pub repo: String,
    /// The stage whose gate is open.
    pub stage: Stage,
    /// Whether the item is an issue or a pull request.
    pub kind: ItemKind,
    /// The issue or pull request number.
    pub number: u64,
    /// The head sha of a pull request, or `None` for an issue.
    pub head_sha: Option<String>,
}

/// True when the issue is open and carries `to-refine`.
pub fn refine_ready(issue: &Issue) -> bool {
    issue.open && has_label(&issue.labels, TO_REFINE)
}

/// True when the issue is open, carries `refined`, does not carry
/// `to-refine`, and every blocker named in the body is closed.
pub fn implement_ready(snap: &RepoSnapshot, issue: &Issue) -> bool {
    issue.open
        && has_label(&issue.labels, REFINED)
        && !has_label(&issue.labels, TO_REFINE)
        && parse_blocked_by(&issue.body)
            .iter()
            .all(|number| !snap.issues.contains_key(number))
}

/// True when the pull request is open and still a draft.
pub fn review_ready(pr: &Pr) -> bool {
    pr.open && pr.draft
}

/// True when the pull request is open and is no longer a draft.
pub fn release_ready(pr: &Pr) -> bool {
    pr.open && !pr.draft
}

/// True when the label list contains `wanted`.
fn has_label(labels: &[String], wanted: &str) -> bool {
    labels.iter().any(|label| label == wanted)
}

/// Collect the issue numbers that a body names as blockers.
///
/// The recognised phrasings are `blocked by`, `blocked-by`, and
/// `depends on`, in any letter case. Each phrase introduces a list: the
/// parser collects every `#N` after it, separated by commas, the word
/// `and`, or plain spaces. The list stops at the first token that is not
/// a separator or a `#N`, so `blocked by #1 then ship #9` reports only
/// `1`. A body may carry several phrases. A number without a phrasing in
/// front of it does not match, so a bare `#12` is not a blocker. The
/// result is sorted and has no duplicates.
pub fn parse_blocked_by(body: &str) -> Vec<u64> {
    const NEEDLES: [&str; 3] = ["blocked by", "blocked-by", "depends on"];
    let lower = body.to_lowercase();
    let mut found: Vec<u64> = Vec::new();
    for needle in NEEDLES {
        let mut search_from = 0;
        while let Some(offset) = lower[search_from..].find(needle) {
            let hit = search_from + offset;
            search_from = hit + needle.len();
            // A word character in front of the phrasing means a longer
            // word, so "unblocked by" does not count as "blocked by".
            let glued_to_word = lower[..hit]
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_ascii_alphanumeric());
            if glued_to_word {
                continue;
            }
            take_blocker_list(&lower, search_from, &mut found);
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Collect the `#N` list that follows one phrasing.
///
/// `pos` points just past the phrasing. Separators between numbers are
/// plain spaces, commas, and the standalone word `and`. The list
/// ends at the first other token or at the end of the text. Found
/// numbers are appended to `found`.
fn take_blocker_list(lower: &str, mut pos: usize, found: &mut Vec<u64>) {
    let mut has_number = false;
    loop {
        let before_separator = pos;
        loop {
            let rest = &lower[pos..];
            let trimmed = rest.trim_start_matches(' ');
            pos += rest.len() - trimmed.len();
            let rest = &lower[pos..];
            if rest.starts_with(',') {
                pos += 1;
                continue;
            }
            if standalone_and_at(lower, pos) {
                pos += "and".len();
                continue;
            }
            break;
        }
        if has_number && pos == before_separator {
            return;
        }
        let Some(rest) = lower[pos..].strip_prefix('#') else {
            return;
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return;
        }
        let Ok(number) = rest[..digits].parse::<u64>() else {
            return;
        };
        found.push(number);
        pos += 1 + digits;
        has_number = true;
    }
}

/// True when the standalone word `and` sits at `pos`.
///
/// The word counts only with a separator in front of it and whitespace
/// behind it, so the tail of `android` is not the word `and`.
fn standalone_and_at(lower: &str, pos: usize) -> bool {
    if !lower[pos..].starts_with("and") {
        return false;
    }
    let before_ok = pos > 0 && matches!(lower[..pos].chars().next_back(), Some(' ') | Some(','));
    let after_ok = matches!(lower[pos + "and".len()..].chars().next(), Some(' '));
    before_ok && after_ok
}

/// The tracker's memory of one item in one stage.
///
/// A key is present exactly when the gate for that item was open at the
/// last poll of its repository. The review key carries the head sha and
/// branch. A push or branch change opens the gate again. An unchanged draft
/// stays silent. The other stages carry no trigger.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GateKey {
    repo: String,
    stage: Stage,
    kind: ItemKind,
    number: u64,
    trigger: Option<String>,
}

/// Remembers the last gate truth and reports `ReadyWork` on each false to
/// true edge.
///
/// The tracker is report-only. It never creates tasks and never touches a
/// release queue.
#[derive(Debug, Clone, Default)]
pub struct GateTracker {
    was_ready: BTreeSet<GateKey>,
}

impl GateTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one repository's fresh snapshot into the tracker and return
    /// the work whose gate just opened.
    ///
    /// An item that vanished from the snapshot loses its memory, so a
    /// returned item reports again. A poll of one repository never
    /// disturbs the memory of another.
    pub fn observe(&mut self, repo: &str, snap: &RepoSnapshot) -> Vec<ReadyWork> {
        let mut now_ready = BTreeSet::new();
        for issue in snap.issues.values() {
            for (stage, open) in [
                (Stage::Refine, refine_ready(issue)),
                (Stage::Implement, implement_ready(snap, issue)),
            ] {
                if open {
                    now_ready.insert(GateKey {
                        repo: repo.to_string(),
                        stage,
                        kind: ItemKind::Issue,
                        number: issue.number,
                        trigger: None,
                    });
                }
            }
        }
        for pr in snap.prs.values() {
            for (stage, open) in [
                (Stage::Review, review_ready(pr)),
                (Stage::Release, release_ready(pr)),
            ] {
                if open {
                    let trigger = (stage == Stage::Review)
                        .then(|| format!("{}\0{}", pr.head_sha, pr.head_ref));
                    now_ready.insert(GateKey {
                        repo: repo.to_string(),
                        stage,
                        kind: ItemKind::Pr,
                        number: pr.number,
                        trigger,
                    });
                }
            }
        }
        let mut fired = Vec::new();
        for key in &now_ready {
            if !self.was_ready.contains(key) {
                let head_sha = match key.kind {
                    ItemKind::Issue => None,
                    ItemKind::Pr => snap.prs.get(&key.number).map(|pr| pr.head_sha.clone()),
                };
                fired.push(ReadyWork {
                    repo: key.repo.clone(),
                    stage: key.stage,
                    kind: key.kind,
                    number: key.number,
                    head_sha,
                });
            }
        }
        self.was_ready.retain(|key| key.repo != repo);
        self.was_ready.extend(now_ready);
        fired
    }

    /// Drop the memory of one item, as when the daemon learns it is gone.
    pub fn forget(&mut self, repo: &str, kind: ItemKind, number: u64) {
        self.was_ready
            .retain(|key| key.repo != repo || key.kind != kind || key.number != number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: format!("issue {number}"),
            body: String::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: String::new(),
            assignees: Vec::new(),
            updated_at: String::new(),
            github_url: String::new(),
            open: true,
        }
    }

    fn issue_with_body(number: u64, labels: &[&str], body: &str) -> Issue {
        let mut item = issue(number, labels);
        item.body = body.to_string();
        item
    }

    fn pr(number: u64, draft: bool, head_sha: &str) -> Pr {
        Pr {
            number,
            node_id: format!("prnode-{number}"),
            title: format!("pr {number}"),
            body: String::new(),
            labels: Vec::new(),
            open: true,
            draft,
            head_sha: head_sha.to_string(),
            head_ref: format!("aif/demo/issue-{number}"),
        }
    }

    fn repo(issues: Vec<Issue>, prs: Vec<Pr>) -> RepoSnapshot {
        RepoSnapshot {
            issues: issues.into_iter().map(|i| (i.number, i)).collect(),
            prs: prs.into_iter().map(|p| (p.number, p)).collect(),
        }
    }

    #[test]
    fn refine_takes_open_issues_labelled_to_refine() {
        assert!(refine_ready(&issue(1, &["to-refine"])));
        assert!(!refine_ready(&issue(2, &["refined"])));
        let mut closed = issue(3, &["to-refine"]);
        closed.open = false;
        assert!(!refine_ready(&closed));
    }

    #[test]
    fn implement_takes_refined_issues_without_to_refine() {
        let snap = repo(vec![issue(2, &[])], vec![]);
        assert!(implement_ready(&snap, &issue(1, &["refined"])));
        assert!(!implement_ready(
            &snap,
            &issue(1, &["refined", "to-refine"])
        ));
        assert!(!implement_ready(&snap, &issue(1, &[])));
    }

    #[test]
    fn implement_waits_for_open_dependencies() {
        let blocked = issue_with_body(1, &["refined"], "blocked by #2");
        let held = repo(vec![blocked, issue(2, &[])], vec![]);
        assert!(!implement_ready(&held, &held.issues[&1]));

        let free = repo(
            vec![issue_with_body(1, &["refined"], "blocked by #2")],
            vec![],
        );
        assert!(implement_ready(&free, &free.issues[&1]));
    }

    #[test]
    fn review_takes_open_drafts_and_release_takes_ready_ones() {
        assert!(review_ready(&pr(1, true, "aaa")));
        assert!(!review_ready(&pr(2, false, "bbb")));
        assert!(release_ready(&pr(3, false, "ccc")));
        assert!(!release_ready(&pr(4, true, "ddd")));
        let mut closed = pr(5, false, "eee");
        closed.open = false;
        assert!(!review_ready(&closed));
        assert!(!release_ready(&closed));
    }

    #[test]
    fn blocked_by_parses_all_three_phrasings_in_any_case() {
        let body = "Blocked by #12\nBLOCKED-BY #5\ndepends on #7";
        assert_eq!(parse_blocked_by(body), vec![5, 7, 12]);
    }

    #[test]
    fn blocked_by_collects_numbers_across_a_body() {
        let body = "Blocked by #3.\nAlso depends on #9 and is blocked-by #3 again.";
        assert_eq!(parse_blocked_by(body), vec![3, 9]);
    }

    #[test]
    fn blocked_by_ignores_bare_numbers_and_loose_text() {
        assert!(parse_blocked_by("#12").is_empty());
        assert!(parse_blocked_by("see #12 and fix it").is_empty());
        assert!(parse_blocked_by("unblocked by #12").is_empty());
        assert!(parse_blocked_by("blocked by the weather").is_empty());
        assert!(parse_blocked_by("").is_empty());
    }

    #[test]
    fn a_phrase_takes_a_list_of_numbers() {
        assert_eq!(parse_blocked_by("blocked by #1, #2 and #3"), vec![1, 2, 3]);
        assert_eq!(parse_blocked_by("blocked by #1, #2, and #3"), vec![1, 2, 3]);
        assert_eq!(parse_blocked_by("blocked by #1 #2"), vec![1, 2]);
    }

    #[test]
    fn a_list_stops_at_the_first_foreign_token() {
        assert_eq!(parse_blocked_by("blocked by #1 then ship #9"), vec![1]);
        assert_eq!(parse_blocked_by("blocked by #1 and then #2"), vec![1]);
        // The tail of a longer word is not the word "and".
        assert_eq!(parse_blocked_by("blocked by #1 android #2"), vec![1]);
    }

    #[test]
    fn a_list_requires_a_separator_between_numbers() {
        assert_eq!(parse_blocked_by("blocked by #1#2"), vec![1]);
    }

    #[test]
    fn a_tab_does_not_separate_numbers() {
        assert_eq!(parse_blocked_by("blocked by #1\t#2"), vec![1]);
    }

    #[test]
    fn two_phrases_each_take_their_own_list() {
        assert_eq!(parse_blocked_by("depends on #4\nblocked-by #7"), vec![4, 7]);
        assert_eq!(
            parse_blocked_by("blocked by #1 and #2, depends on #3"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_steady_label_fires_once() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        let first = tracker.observe("borsuk", &ready);
        assert_eq!(
            first,
            vec![ReadyWork {
                repo: "borsuk".to_string(),
                stage: Stage::Refine,
                kind: ItemKind::Issue,
                number: 1,
                head_sha: None,
            }]
        );
        assert!(tracker.observe("borsuk", &ready).is_empty());
    }

    #[test]
    fn removing_and_readding_a_label_fires_again() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);
        // No gate label, so neither the refine nor the implement gate is open.
        let idle = repo(vec![issue(1, &["question"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
        assert!(tracker.observe("borsuk", &idle).is_empty());
        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
    }

    #[test]
    fn a_new_push_retriggers_review_but_an_unchanged_draft_does_not() {
        let mut tracker = GateTracker::new();
        let draft = |sha: &str| repo(vec![], vec![pr(5, true, sha)]);

        assert_eq!(tracker.observe("borsuk", &draft("aaa")).len(), 1);
        assert!(tracker.observe("borsuk", &draft("aaa")).is_empty());

        let again = tracker.observe("borsuk", &draft("bbb"));
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].head_sha.as_deref(), Some("bbb"));
    }

    #[test]
    fn a_new_head_branch_retriggers_review_with_the_same_commit() {
        let mut tracker = GateTracker::new();
        let mut first = pr(5, true, "aaa");
        first.head_ref = "aif/borsuk/issue-5".to_string();
        let mut renamed = first.clone();
        renamed.head_ref = "aif/borsuk/issue-142".to_string();

        assert_eq!(
            tracker.observe("borsuk", &repo(vec![], vec![first])).len(),
            1
        );
        assert_eq!(
            tracker
                .observe("borsuk", &repo(vec![], vec![renamed]))
                .len(),
            1
        );
    }

    #[test]
    fn an_implement_gate_stays_shut_while_a_dependency_is_open() {
        let mut tracker = GateTracker::new();
        let held = repo(
            vec![
                issue_with_body(7, &["refined"], "blocked by #2"),
                issue(2, &[]),
            ],
            vec![],
        );
        let free = repo(
            vec![issue_with_body(7, &["refined"], "blocked by #2")],
            vec![],
        );

        assert!(tracker.observe("borsuk", &held).is_empty());
        assert!(tracker.observe("borsuk", &held).is_empty());

        let fired = tracker.observe("borsuk", &free);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].stage, Stage::Implement);
        assert_eq!(fired[0].number, 7);
    }

    #[test]
    fn a_vanished_item_is_forgotten_and_can_fire_again_on_return() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);
        let gone = repo(vec![], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
        assert!(tracker.observe("borsuk", &gone).is_empty());
        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
    }

    #[test]
    fn forget_drops_memory_so_the_next_poll_fires_again() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
        tracker.forget("borsuk", ItemKind::Issue, 1);
        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);

        // Forgetting another item does not revive this one.
        tracker.forget("borsuk", ItemKind::Issue, 2);
        assert!(tracker.observe("borsuk", &ready).is_empty());
    }

    #[test]
    fn release_ready_pull_requests_are_reported_once() {
        let mut tracker = GateTracker::new();

        // The first poll sees a draft, so the review gate fires, not release.
        assert_eq!(
            tracker
                .observe("borsuk", &repo(vec![], vec![pr(3, true, "aaa")]))
                .len(),
            1
        );

        let ready = tracker.observe("borsuk", &repo(vec![], vec![pr(3, false, "aaa")]));
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].stage, Stage::Release);
        assert_eq!(ready[0].head_sha.as_deref(), Some("aaa"));

        assert!(tracker
            .observe("borsuk", &repo(vec![], vec![pr(3, false, "aaa")]))
            .is_empty());
    }

    #[test]
    fn repositories_are_tracked_independently() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready).len(), 1);
        assert!(tracker
            .observe("qubitsok", &repo(vec![], vec![]))
            .is_empty());
        // The empty qubitsok poll must not clear borsuk's memory.
        assert!(tracker.observe("borsuk", &ready).is_empty());
    }

    #[test]
    fn a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate() {
        let mut tracker = GateTracker::new();
        let to_refine = repo(vec![issue(1, &["to-refine"])], vec![]);
        let refined = repo(
            vec![
                issue_with_body(1, &["refined"], "depends on #9"),
                issue(9, &[]),
            ],
            vec![],
        );
        let unblocked = repo(
            vec![issue_with_body(1, &["refined"], "depends on #9")],
            vec![],
        );

        assert_eq!(tracker.observe("borsuk", &to_refine).len(), 1);
        // Issue 9 is still open, so the implement gate stays shut.
        assert!(tracker.observe("borsuk", &refined).is_empty());

        let fired = tracker.observe("borsuk", &unblocked);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].stage, Stage::Implement);
    }
}
