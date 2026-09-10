//! Holds the four stage predicates and the edge-triggered gate tracker.
//!
//! A predicate says which items are ready for a stage right now. The
//! tracker compares the answer with the previous poll and reports
//! `ReadyWork` only on a false to true edge. A gate that stays open
//! across polls reports once. The tracker only reports. It never creates
//! tasks and never touches a release queue; the daemon decides what to do
//! with the report, including the report of the release stage.
//!
//! A gate answers one question: did this item enter the stage? It does not
//! answer whether the work can start now. The blockers of a ticket are the
//! second question, and [`unmet_blockers`] answers it. The daemon asks the
//! gate once, on the edge, and asks the blockers on every pass. So a ticket
//! with an open blocker gets its queued task and its board row, and the
//! dispatch holds that task until the blocker closes.

use std::collections::BTreeSet;

use crate::model::{Issue, ItemKind, Pr, RepoSnapshot, Stage};
use crate::theory::records::{
    skips_prediction_gates, RecordKey, TheoryRecords, THEORY_SHORT_LABEL,
};

/// The label that asks the factory to shape a raw issue.
pub const TO_REFINE: &str = "to-refine";

/// The label that marks a shaped issue as ready to implement.
pub const REFINED: &str = "refined";

/// The label that marks a parent whose chunks became sub-tickets.
///
/// A refine run has two outcomes. It shapes one ticket and adds
/// [`REFINED`], or it splits the work and adds this label to the parent.
/// A parent never carries [`REFINED`], so [`implement_ready`] stays false
/// on it and only the sub-tickets reach the implement stage. The parent
/// closes when the PR of the final chunk merges.
pub const EPIC: &str = "epic";

/// The label that asks a human to decide something on GitHub.
pub const NEEDS_HUMAN_LABEL: &str = "needs-human";

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

/// True when the issue is open, carries `to-refine`, and, on a governed
/// repository, holds an accepted short prediction.
///
/// The short prediction is the operator's claim about the change, and the
/// governor takes it before the refine agent reads the ticket. The claim
/// lands as `theory-short` on the theory record, so the gate reads
/// `records`, never `issue.labels`, and shadow mode needs no second rule.
///
/// Two items pass without a prediction. A `model-pr` or a `verify-skill`
/// item changes no application behaviour, and
/// [`skips_prediction_gates`] names both. The skip wins over every later
/// rule, so a `model-pr` still refines while the model is broken; the
/// model pull request is how the operator repairs it. Every other
/// governed item holds while the model does not parse, because a broken
/// model can validate no area.
pub fn refine_ready(issue: &Issue, alias: &str, records: &TheoryRecords) -> bool {
    if !issue.open || !has_label(&issue.labels, TO_REFINE) {
        return false;
    }
    if !records.is_governed(alias) || skips_prediction_gates(&issue.labels) {
        return true;
    }
    if records.model_error(alias) {
        return false;
    }
    has_label(
        records.labels_of(alias, &RecordKey::Issue(issue.number)),
        THEORY_SHORT_LABEL,
    )
}

/// The pipeline hint of a ticket that waits for its short prediction.
pub const AWAITS_SHORT_HINT: &str = "awaits short prediction";

/// The pipeline hint of a governed ticket whose model did not parse.
pub const MODEL_ERROR_HINT: &str = "model error";

/// Why the refine gate holds one ticket, or `None` when it holds none.
///
/// The hint is the reason the pipeline draws where the refine task would
/// stand, so a held ticket never leaves the lane silent. A ticket that is
/// not refine work at all, and a ticket the gate admits, hold nothing.
pub fn refine_hold(issue: &Issue, alias: &str, records: &TheoryRecords) -> Option<&'static str> {
    if !issue.open || !has_label(&issue.labels, TO_REFINE) || refine_ready(issue, alias, records) {
        return None;
    }
    if records.model_error(alias) {
        return Some(MODEL_ERROR_HINT);
    }
    Some(AWAITS_SHORT_HINT)
}

/// True when the issue is open, carries `refined`, and does not carry
/// `to-refine`.
///
/// This is the whole implement gate. The blockers of the ticket are not
/// part of it, and [`unmet_blockers`] carries them instead.
///
/// The gate held the blocker test until v0.6. The test then decided two
/// things at once, and each answer was wrong for the other. A blocked
/// ticket got no task, so the board showed nothing and the operator saw no
/// cause. And `Daemon::reconcile_unready` cancels an active task whose gate
/// closed, so a blocker that reopened turned a healthy ticket into a failed
/// row.
///
/// The gate now answers one question: did the ticket enter the implement
/// stage? The daemon cancels the task of a ticket that lost the label,
/// closed, or is absent, because that ticket left the stage. A ticket that
/// waits for a blocker is still implement work. It keeps its task, and the
/// dispatch defers that task.
pub fn implement_ready(issue: &Issue) -> bool {
    issue.open && has_label(&issue.labels, REFINED) && !has_label(&issue.labels, TO_REFINE)
}

/// True when `number` still blocks work in this repository.
///
/// GitHub gives issues and pull requests one number space, so a blocker
/// names either kind. The snapshot holds the open items alone, so a
/// blocker that appears in it is open, and every other blocker is
/// settled. A settled blocker is a closed ticket, a merged or closed pull
/// request, or an item this repository never had.
pub fn blocker_open(snap: &RepoSnapshot, number: u64) -> bool {
    snap.issues.contains_key(&number) || snap.prs.contains_key(&number)
}

/// The blockers of one issue that are still open, in ascending order.
///
/// The result is empty when the body names no blocker, or when every
/// blocker it names is settled. A caller that only needs a yes or no asks
/// `is_empty`. A caller that must name the cause takes the first entry.
///
/// The issue never blocks itself. A body that names its own number is a
/// mistake, and a ticket that waits for itself waits for ever, so the
/// parser drops that number.
pub fn unmet_blockers(snap: &RepoSnapshot, issue: &Issue) -> Vec<u64> {
    parse_blocked_by(&issue.body)
        .into_iter()
        .filter(|number| *number != issue.number && blocker_open(snap, *number))
        .collect()
}

/// True when the pull request is open, still a draft, and carries no
/// `needs-human` label.
///
/// The review stage has two outcomes: the agent finds nothing and marks the
/// pull request ready, or it repairs every finding and marks it ready. The
/// `needs-human` label names the one explicit rest state between them, and
/// the label closes this gate for two reasons.
///
/// A closed gate keeps the pull request still. The gate trigger carries the
/// head sha, so without the label test a push would start a second review of
/// a pull request that waits for a person.
///
/// A closed gate also restarts the work exactly once. [`GateTracker::observe`]
/// rebuilds its memory on every poll, so the label drops the key. When the
/// operator answers and the daemon removes the label, the gate goes from
/// false to true and fires one fresh review, which reads the answer in the
/// comments of the pull request.
pub fn review_ready(pr: &Pr) -> bool {
    pr.open && pr.draft && !has_label(&pr.labels, NEEDS_HUMAN_LABEL)
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
    ///
    /// `records` is the theory read model of the same poll. The refine
    /// gate of a governed repository reads its record labels from it.
    pub fn observe(
        &mut self,
        repo: &str,
        snap: &RepoSnapshot,
        records: &TheoryRecords,
    ) -> Vec<ReadyWork> {
        let mut now_ready = BTreeSet::new();
        for issue in snap.issues.values() {
            for (stage, open) in [
                (Stage::Refine, refine_ready(issue, repo, records)),
                (Stage::Implement, implement_ready(issue)),
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

    /// Drop the whole memory of one repository, as when its configuration
    /// goes away while the daemon runs.
    pub fn forget_repo(&mut self, repo: &str) {
        self.was_ready.retain(|key| key.repo != repo);
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

    /// The config of the prediction gate tests: one repository whose
    /// governor `governor` names.
    fn gate_config(governor: &str) -> crate::config::Config {
        let text = format!(
            "schema_version = 1\n\
             \n[stage.refine]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[stage.implement]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[stage.review]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[stage.release]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[ticket.create]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[ticket.chat]\nharness = \"claude\"\nmodel = \"m\"\n\
             \n[repo.borsuk]\npath = \"/tmp/borsuk\"\ngovernor = \"{governor}\"\n"
        );
        crate::config::Config::parse(&text).expect("the gate config must parse")
    }

    /// The theory read model of one poll that shows `issue`.
    fn records_of(governor: &str, issue: &Issue, model_error: bool) -> TheoryRecords {
        let mut snapshot = crate::model::Snapshot::default();
        snapshot
            .repos
            .insert("borsuk".to_string(), repo(vec![issue.clone()], Vec::new()));
        let mut records = TheoryRecords::derive(
            &gate_config(governor),
            &snapshot,
            &std::collections::BTreeMap::new(),
        );
        records.set_model_error("borsuk", model_error);
        records
    }

    /// The refine gate of a governed ticket needs the short prediction:
    /// `to-refine` alone holds, both labels open the gate, a repository
    /// with the governor off keeps the v0.6 rule, and a model that did
    /// not parse holds every governed ticket.
    #[test]
    fn the_refine_gate_of_a_governed_ticket_waits_for_theory_short() {
        let waiting = issue(1, &[TO_REFINE]);
        assert!(
            !refine_ready(&waiting, "borsuk", &records_of("on", &waiting, false)),
            "to-refine alone yields no refine work"
        );

        let ready = issue(1, &[TO_REFINE, THEORY_SHORT_LABEL]);
        assert!(
            refine_ready(&ready, "borsuk", &records_of("on", &ready, false)),
            "both labels yield work"
        );

        assert!(
            refine_ready(&waiting, "borsuk", &records_of("off", &waiting, false)),
            "with the governor off to-refine alone yields work"
        );

        assert!(
            !refine_ready(&ready, "borsuk", &records_of("on", &ready, true)),
            "a model in error holds both labels"
        );

        let skipped = issue(1, &[TO_REFINE, "verify-skill"]);
        assert!(
            refine_ready(&skipped, "borsuk", &records_of("on", &skipped, false)),
            "a verify-skill ticket needs no prediction"
        );

        // The skip wins over the model error. The model pull request is
        // how the operator repairs a broken model, so it may not wait for
        // that model to parse.
        let model_pr = issue(1, &[TO_REFINE, "model-pr"]);
        assert!(
            refine_ready(&model_pr, "borsuk", &records_of("on", &model_pr, true)),
            "a model-pr moves while the model is broken"
        );
        assert_eq!(
            refine_hold(&model_pr, "borsuk", &records_of("on", &model_pr, true)),
            None,
            "a skipped ticket is never held"
        );
        assert_eq!(
            refine_hold(&ready, "borsuk", &records_of("on", &ready, true)),
            Some(MODEL_ERROR_HINT)
        );
        assert_eq!(
            refine_hold(&waiting, "borsuk", &records_of("on", &waiting, false)),
            Some(AWAITS_SHORT_HINT)
        );
    }

    /// The gate tracker reads the same rule, so a held ticket reports no
    /// ready work and the label that lands later reports it once.
    #[test]
    fn the_tracker_reports_a_governed_refine_only_after_theory_short() {
        let mut tracker = GateTracker::new();
        let waiting = issue(1, &[TO_REFINE]);
        let held = repo(vec![waiting.clone()], Vec::new());
        assert!(
            tracker
                .observe("borsuk", &held, &records_of("on", &waiting, false))
                .is_empty(),
            "the held ticket reports nothing"
        );

        let ready = issue(1, &[TO_REFINE, THEORY_SHORT_LABEL]);
        let open = repo(vec![ready.clone()], Vec::new());
        let fired = tracker.observe("borsuk", &open, &records_of("on", &ready, false));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].stage, Stage::Refine);
        assert_eq!(fired[0].number, 1);
    }

    /// A theory read model with no governed repository, so every gate
    /// keeps its v0.6 rule.
    fn ungoverned() -> TheoryRecords {
        TheoryRecords::default()
    }

    fn repo(issues: Vec<Issue>, prs: Vec<Pr>) -> RepoSnapshot {
        RepoSnapshot {
            issues: issues.into_iter().map(|i| (i.number, i)).collect(),
            prs: prs.into_iter().map(|p| (p.number, p)).collect(),
        }
    }

    #[test]
    fn refine_takes_open_issues_labelled_to_refine() {
        assert!(refine_ready(
            &issue(1, &["to-refine"]),
            "borsuk",
            &ungoverned()
        ));
        assert!(!refine_ready(
            &issue(2, &["refined"]),
            "borsuk",
            &ungoverned()
        ));
        let mut closed = issue(3, &["to-refine"]);
        closed.open = false;
        assert!(!refine_ready(&closed, "borsuk", &ungoverned()));
    }

    #[test]
    fn implement_takes_refined_issues_without_to_refine() {
        assert!(implement_ready(&issue(1, &["refined"])));
        assert!(!implement_ready(&issue(1, &["refined", "to-refine"])));
        assert!(!implement_ready(&issue(1, &[])));
        let mut closed = issue(1, &["refined"]);
        closed.open = false;
        assert!(!implement_ready(&closed));
    }

    /// The parent of a split carries `epic` and never `refined`, so the
    /// implement gate stays shut on it while its sub-tickets run.
    #[test]
    fn an_epic_parent_never_opens_the_implement_gate() {
        let snap = repo(
            vec![issue(1, &[EPIC]), issue(2, &[REFINED, "chunk"])],
            vec![],
        );
        assert!(!implement_ready(&snap.issues[&1]));
        assert!(
            !refine_ready(&snap.issues[&1], "borsuk", &ungoverned()),
            "the parent is not refined again"
        );
        assert!(
            implement_ready(&snap.issues[&2]),
            "the sub-ticket carries the work"
        );
    }

    /// The gate answers one question: did the ticket enter the stage? An
    /// open blocker is the second question, so it leaves the gate open and
    /// shows up in the unmet list instead. The daemon defers the task.
    #[test]
    fn an_open_dependency_leaves_the_implement_gate_open() {
        let blocked = issue_with_body(1, &["refined"], "blocked by #2");
        let held = repo(vec![blocked, issue(2, &[])], vec![]);
        assert!(implement_ready(&held.issues[&1]));
        assert_eq!(unmet_blockers(&held, &held.issues[&1]), vec![2]);

        let free = repo(
            vec![issue_with_body(1, &["refined"], "blocked by #2")],
            vec![],
        );
        assert!(implement_ready(&free.issues[&1]));
        assert!(unmet_blockers(&free, &free.issues[&1]).is_empty());
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

    /// The `needs-human` label is the one explicit rest state of the review
    /// stage. It must hold the pull request still, even after a push, and it
    /// must release exactly one fresh review when the operator answers.
    #[test]
    fn a_needs_human_draft_rests_until_the_label_goes_away() {
        let mut tracker = GateTracker::new();
        let mut waiting = pr(7, true, "aaa");
        waiting.labels = vec![NEEDS_HUMAN_LABEL.to_string()];
        assert!(!review_ready(&waiting), "the label closes the review gate");

        let fired = tracker.observe(
            "borsuk",
            &repo(Vec::new(), vec![waiting.clone()]),
            &ungoverned(),
        );
        assert!(
            !fired.iter().any(|work| work.stage == Stage::Review),
            "a labelled draft starts no review"
        );

        // A push moves the gate trigger. The closed gate must stay closed,
        // so the agent's own repair commits cannot restart its review.
        let mut pushed = waiting.clone();
        pushed.head_sha = "bbb".to_string();
        let fired = tracker.observe(
            "borsuk",
            &repo(Vec::new(), vec![pushed.clone()]),
            &ungoverned(),
        );
        assert!(
            !fired.iter().any(|work| work.stage == Stage::Review),
            "a push on a labelled draft starts no review"
        );

        // The operator answers, so the daemon removes the label. The gate
        // goes from false to true and reports the work exactly once.
        let mut answered = pushed.clone();
        answered.labels.clear();
        let fired = tracker.observe(
            "borsuk",
            &repo(Vec::new(), vec![answered.clone()]),
            &ungoverned(),
        );
        let review: Vec<&ReadyWork> = fired
            .iter()
            .filter(|work| work.stage == Stage::Review)
            .collect();
        assert_eq!(review.len(), 1, "the answer starts one fresh review");
        assert_eq!(review[0].number, 7);
        assert_eq!(review[0].head_sha.as_deref(), Some("bbb"));

        let again = tracker.observe("borsuk", &repo(Vec::new(), vec![answered]), &ungoverned());
        assert!(
            !again.iter().any(|work| work.stage == Stage::Review),
            "the open gate reports once"
        );
    }

    /// GitHub gives issues and pull requests one number space, so an open
    /// pull request is a blocker of the ticket that names it.
    #[test]
    fn an_open_pull_request_counts_as_a_blocker() {
        let held = repo(
            vec![issue_with_body(1, &["refined"], "blocked by #4")],
            vec![pr(4, true, "aaa")],
        );
        assert_eq!(unmet_blockers(&held, &held.issues[&1]), vec![4]);

        let merged = repo(
            vec![issue_with_body(1, &["refined"], "blocked by #4")],
            vec![],
        );
        assert!(unmet_blockers(&merged, &merged.issues[&1]).is_empty());
    }

    /// The unmet list names every open blocker and drops the settled ones.
    #[test]
    fn unmet_blockers_reports_only_the_open_ones_in_order() {
        let ticket = |body: &str| issue_with_body(1, &["refined"], body);
        let snap = repo(
            vec![issue(2, &[]), issue(9, &[]), ticket("")],
            vec![pr(5, false, "aaa")],
        );

        assert_eq!(
            unmet_blockers(&snap, &ticket("blocked by #9, #2 and #7")),
            vec![2, 9],
            "#7 is settled, so it does not appear"
        );
        assert_eq!(unmet_blockers(&snap, &ticket("depends on #5")), vec![5]);
        assert!(unmet_blockers(&snap, &ticket("depends on #7")).is_empty());
        assert!(unmet_blockers(&snap, &ticket("no phrasing here #2")).is_empty());

        assert!(blocker_open(&snap, 2));
        assert!(blocker_open(&snap, 5));
        assert!(!blocker_open(&snap, 7));
    }

    /// A ticket that names its own number would wait for ever, so the
    /// parser drops that number and keeps the rest of the list.
    #[test]
    fn a_ticket_never_blocks_itself() {
        let itself = issue_with_body(1, &["refined"], "depends on #1 and #2");
        let snap = repo(vec![itself.clone(), issue(2, &[])], vec![]);

        assert_eq!(unmet_blockers(&snap, &itself), vec![2]);
        assert!(blocker_open(&snap, 1), "the ticket itself is still open");

        let alone = issue_with_body(1, &["refined"], "depends on #1");
        let solo = repo(vec![alone.clone()], vec![]);
        assert!(unmet_blockers(&solo, &alone).is_empty());
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

        let first = tracker.observe("borsuk", &ready, &ungoverned());
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
        assert!(tracker.observe("borsuk", &ready, &ungoverned()).is_empty());
    }

    #[test]
    fn removing_and_readding_a_label_fires_again() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);
        // No gate label, so neither the refine nor the implement gate is open.
        let idle = repo(vec![issue(1, &["question"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        assert!(tracker.observe("borsuk", &idle, &ungoverned()).is_empty());
        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
    }

    #[test]
    fn a_new_push_retriggers_review_but_an_unchanged_draft_does_not() {
        let mut tracker = GateTracker::new();
        let draft = |sha: &str| repo(vec![], vec![pr(5, true, sha)]);

        assert_eq!(
            tracker
                .observe("borsuk", &draft("aaa"), &ungoverned())
                .len(),
            1
        );
        assert!(tracker
            .observe("borsuk", &draft("aaa"), &ungoverned())
            .is_empty());

        let again = tracker.observe("borsuk", &draft("bbb"), &ungoverned());
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
            tracker
                .observe("borsuk", &repo(vec![], vec![first]), &ungoverned())
                .len(),
            1
        );
        assert_eq!(
            tracker
                .observe("borsuk", &repo(vec![], vec![renamed]), &ungoverned())
                .len(),
            1
        );
    }

    /// The blocker no longer holds the gate. The ticket gets its task at
    /// once, so the board shows the wait, and the tracker reports it once.
    #[test]
    fn an_implement_gate_opens_once_even_while_a_dependency_is_open() {
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

        let fired = tracker.observe("borsuk", &held, &ungoverned());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].stage, Stage::Implement);
        assert_eq!(fired[0].number, 7);

        // The blocker closing is not a gate edge, so no second task fires.
        assert!(tracker.observe("borsuk", &held, &ungoverned()).is_empty());
        assert!(tracker.observe("borsuk", &free, &ungoverned()).is_empty());
    }

    #[test]
    fn a_vanished_item_is_forgotten_and_can_fire_again_on_return() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);
        let gone = repo(vec![], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        assert!(tracker.observe("borsuk", &gone, &ungoverned()).is_empty());
        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
    }

    #[test]
    fn forget_drops_memory_so_the_next_poll_fires_again() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        tracker.forget("borsuk", ItemKind::Issue, 1);
        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);

        // Forgetting another item does not revive this one.
        tracker.forget("borsuk", ItemKind::Issue, 2);
        assert!(tracker.observe("borsuk", &ready, &ungoverned()).is_empty());
    }

    #[test]
    fn forget_repo_drops_the_whole_memory_of_one_repository() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        assert_eq!(tracker.observe("qubitsok", &ready, &ungoverned()).len(), 1);
        tracker.forget_repo("borsuk");
        // The removed repository fires again on a return; the kept one
        // holds its memory.
        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        assert!(tracker
            .observe("qubitsok", &ready, &ungoverned())
            .is_empty());
    }

    #[test]
    fn release_ready_pull_requests_are_reported_once() {
        let mut tracker = GateTracker::new();

        // The first poll sees a draft, so the review gate fires, not release.
        assert_eq!(
            tracker
                .observe(
                    "borsuk",
                    &repo(vec![], vec![pr(3, true, "aaa")]),
                    &ungoverned()
                )
                .len(),
            1
        );

        let ready = tracker.observe(
            "borsuk",
            &repo(vec![], vec![pr(3, false, "aaa")]),
            &ungoverned(),
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].stage, Stage::Release);
        assert_eq!(ready[0].head_sha.as_deref(), Some("aaa"));

        assert!(tracker
            .observe(
                "borsuk",
                &repo(vec![], vec![pr(3, false, "aaa")]),
                &ungoverned()
            )
            .is_empty());
    }

    #[test]
    fn repositories_are_tracked_independently() {
        let mut tracker = GateTracker::new();
        let ready = repo(vec![issue(1, &["to-refine"])], vec![]);

        assert_eq!(tracker.observe("borsuk", &ready, &ungoverned()).len(), 1);
        assert!(tracker
            .observe("qubitsok", &repo(vec![], vec![]), &ungoverned())
            .is_empty());
        // The empty qubitsok poll must not clear borsuk's memory.
        assert!(tracker.observe("borsuk", &ready, &ungoverned()).is_empty());
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

        assert_eq!(
            tracker.observe("borsuk", &to_refine, &ungoverned()).len(),
            1
        );

        // The label moved, so the implement gate opens. Issue 9 is still
        // open, and the daemon defers the task it gets from this report.
        let fired = tracker.observe("borsuk", &refined, &ungoverned());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].stage, Stage::Implement);
        assert_eq!(unmet_blockers(&refined, &refined.issues[&1]), vec![9]);
    }
}
