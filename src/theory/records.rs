//! The theory records: the labels, the block tags, and the event blocks.
//!
//! A theory record is the GitHub issue that holds the theory state of one
//! item. [`RecordKey`] names the item; the daemon resolves it to an
//! `(owner_repo, number)` pair. Agents and the daemon ship facts as tagged
//! blocks, and [`parse_event_blocks`] reads the event blocks back.
//! [`TheoryRecords`] is the read model of one poll: the labels of every
//! record, the open marker count, and the predictions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::contract::{self, ContractContext, Finding};
use crate::config::Config;
use crate::model::{RepoSnapshot, Snapshot};

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

/// The theory files only a model branch may change.
pub const MODEL_FILES: [&str; 3] = ["theory/model.toml", "theory/verify.toml", "theory/rules.md"];

/// Check the body and the diff of one governed pull request.
///
/// A branch that is not the model branch may not change a theory file.
/// Every other rule is the Before / After contract of
/// [`contract::check_body_lines`], and the changed paths of `ctx` are the
/// same list both rules read. The first broken rule wins.
pub fn check_pr(body: &str, branch: &str, ctx: &ContractContext<'_>) -> Result<(), Finding> {
    if !is_model_branch(branch) {
        for path in &ctx.changed_paths {
            if let Some(file) = MODEL_FILES.iter().find(|file| path == *file) {
                return Err(Finding::plain(format!("{file} changed off a model branch")));
            }
        }
    }
    contract::check_body_lines(body, ctx)
}

/// True when one branch is a model branch, in the form
/// `aif/<alias>/model-<n>`.
fn is_model_branch(branch: &str) -> bool {
    let mut parts = branch.split('/');
    parts.next() == Some("aif")
        && parts.next().is_some_and(|alias| !alias.is_empty())
        && parts.next().is_some_and(|last| last.starts_with("model-"))
        && parts.next().is_none()
}

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
    /// The run skill surface the event names, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
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
    scan_block_bodies(text, EVENT_BLOCK)
        .into_iter()
        .filter_map(|body| serde_json::from_str::<Event>(body).ok())
        .collect()
}

/// The closing tag of one opening tag: `<aif-event-v1>` closes as
/// `</aif-event-v1>`.
pub fn close_tag(open: &str) -> String {
    format!("</{}>", open.trim_start_matches('<').trim_end_matches('>'))
}

/// The value of the `kind` field of a short prediction.
pub const PREDICTION_SHORT: &str = "short";
/// The value of the `kind` field of a full prediction.
pub const PREDICTION_FULL: &str = "full";

/// The five slot names of a full prediction, in design order: the
/// behaviours that change, the states or transitions that change, the
/// invariants at risk, the failure modes added or changed, and the other
/// areas the change can touch.
pub const PREDICTION_SLOT_NAMES: [&str; 5] = [
    "behaviours",
    "states",
    "invariants",
    "failure-modes",
    "other-areas",
];

/// The confidence tag of one prediction slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PredictionTag {
    /// The operator is sure the slot lists every entry in play.
    Sure,
    /// The operator is not sure.
    Unsure,
}

/// One slot of a full prediction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionSlot {
    /// The slot name; one of [`PREDICTION_SLOT_NAMES`].
    pub name: String,
    /// The entry ids the operator named, in order.
    pub entries: Vec<String>,
    /// The confidence tag of the slot.
    pub tag: PredictionTag,
}

/// The short prediction of one ticket: `{ kind, text, areas }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortPrediction {
    /// The block kind; always [`PREDICTION_SHORT`].
    pub kind: String,
    /// The operator's statement of the change, in one line.
    pub text: String,
    /// The model areas the change touches.
    pub areas: Vec<String>,
}

/// The full prediction of one ticket: `{ kind, slots }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullPrediction {
    /// The block kind; always [`PREDICTION_FULL`].
    pub kind: String,
    /// The five slots of the prediction.
    pub slots: Vec<PredictionSlot>,
}

/// One prediction, as one `<aif-prediction-v1>` block holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prediction {
    /// The short prediction the operator writes before refine.
    Short(ShortPrediction),
    /// The full prediction the operator writes after `refined`.
    Full(FullPrediction),
}

/// Render one prediction as a complete `<aif-prediction-v1>` block.
pub fn prediction_block(prediction: &Prediction) -> String {
    let body = match prediction {
        Prediction::Short(value) => serde_json::to_string(value),
        Prediction::Full(value) => serde_json::to_string(value),
    }
    .unwrap_or_default();
    let close = close_tag(PREDICTION_BLOCK);
    format!("{PREDICTION_BLOCK}\n{body}\n{close}")
}

/// Parse every complete `<aif-prediction-v1>` block of one text.
///
/// A block whose body holds neither a short nor a full prediction, and a
/// block with no closing tag, is skipped. The order of the blocks is kept.
pub fn parse_prediction_blocks(text: &str) -> Vec<Prediction> {
    scan_block_bodies(text, PREDICTION_BLOCK)
        .into_iter()
        .filter_map(parse_prediction)
        .collect()
}

/// Parse one block body into a prediction; `None` for any other body.
///
/// The kind field must name the variant the fields carry, so a body with
/// a wrong or unknown kind is skipped.
fn parse_prediction(body: &str) -> Option<Prediction> {
    if let Ok(value) = serde_json::from_str::<ShortPrediction>(body) {
        if value.kind == PREDICTION_SHORT {
            return Some(Prediction::Short(value));
        }
    }
    if let Ok(value) = serde_json::from_str::<FullPrediction>(body) {
        if value.kind == PREDICTION_FULL {
            return Some(Prediction::Full(value));
        }
    }
    None
}

/// The bodies of every complete block of one tag, in text order.
fn scan_block_bodies<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let close = close_tag(tag);
    let mut bodies = Vec::new();
    let mut rest = text;
    'scan: while let Some(start) = rest.find(tag) {
        let after_open = &rest[start + tag.len()..];
        let Some(end) = after_open.find(&close) else {
            break;
        };
        let span = &after_open[..end];
        if let Some(next_open) = span.find(tag) {
            // The opening tag is truncated: the later tag owns the close,
            // so the scan restarts there.
            rest = &after_open[next_open..];
            continue 'scan;
        }
        bodies.push(span.trim());
        rest = &after_open[end + close.len()..];
    }
    bodies
}

/// The theory read model of one poll: the labels of every record.
///
/// [`TheoryRecords::derive`] reads the labels from the code snapshot, or
/// from the theory snapshot of a shadowed alias, and every gate and row
/// derivation reads this model instead of `issue.labels`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TheoryRecords {
    /// The labels of each item record, keyed by alias and item number.
    items: BTreeMap<(String, u64), Vec<String>>,
    /// The labels of each repository record, keyed by alias.
    repos: BTreeMap<String, Vec<String>>,
    /// The open marker count of each governed alias.
    open: BTreeMap<String, usize>,
}

impl TheoryRecords {
    /// Derive the record labels of every governed repository.
    ///
    /// `config` names every repository and its theory settings;
    /// `snapshots` holds the code snapshots and `theory_snapshots` the
    /// theory snapshots, keyed by alias. A shadowed alias is one whose
    /// `theory.repo` is set; it reads its labels from its theory
    /// snapshot. Every other governed alias reads the code snapshot. An
    /// alias with the governor off derives nothing.
    pub fn derive(
        config: &Config,
        snapshots: &Snapshot,
        theory_snapshots: &BTreeMap<String, RepoSnapshot>,
    ) -> TheoryRecords {
        let mut records = TheoryRecords::default();
        for (alias, repo) in &config.repos {
            if !repo.theory.governor.is_on() {
                continue;
            }
            let shadowed = repo
                .theory
                .theory
                .as_ref()
                .is_some_and(|theory| theory.repo.is_some());
            let source = if shadowed {
                theory_snapshots.get(alias)
            } else {
                snapshots.repos.get(alias)
            };
            let Some(source) = source else {
                continue;
            };
            let mut open = 0usize;
            for (number, issue) in &source.issues {
                let mut record = true;
                if shadowed {
                    if issue.title == repo_record_title(alias) {
                        records.repos.insert(alias.clone(), issue.labels.clone());
                    } else if let Some(item) = shadow_item(alias, &issue.title) {
                        records
                            .items
                            .insert((alias.clone(), item), issue.labels.clone());
                    } else {
                        // One theory repository may hold the shadow issues
                        // of several aliases; this issue is not a record
                        // of this alias.
                        record = false;
                    }
                } else {
                    records
                        .items
                        .insert((alias.clone(), *number), issue.labels.clone());
                    if issue.title == repo_record_title(alias) {
                        records.repos.insert(alias.clone(), issue.labels.clone());
                    }
                }
                if record {
                    open += open_markers(&issue.labels);
                }
            }
            if !shadowed {
                for (number, pr) in &source.prs {
                    records
                        .items
                        .insert((alias.clone(), *number), pr.labels.clone());
                    open += open_markers(&pr.labels);
                }
            }
            records.open.insert(alias.clone(), open);
        }
        records
    }

    /// The labels of the record of one item.
    ///
    /// An item with no record yet, and an alias the derive skipped,
    /// answers no labels.
    pub fn labels_of(&self, alias: &str, key: &RecordKey) -> &[String] {
        match key {
            RecordKey::Issue(number) | RecordKey::Pr(number) => self
                .items
                .get(&(alias.to_string(), *number))
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            RecordKey::Repo => self.repos.get(alias).map(Vec::as_slice).unwrap_or(&[]),
        }
    }

    /// The count of records labelled `delta-open` or `event-open` of one
    /// alias.
    ///
    /// A record with both labels counts once. An alias with the governor
    /// off answers zero.
    pub fn open_count(&self, alias: &str) -> usize {
        self.open.get(alias).copied().unwrap_or(0)
    }
}

/// The title of the repository record of one alias.
fn repo_record_title(alias: &str) -> String {
    format!("{alias}/theory")
}

/// The item number of one shadow issue title, `<alias>#<n>`.
fn shadow_item(alias: &str, title: &str) -> Option<u64> {
    title.strip_prefix(alias)?.strip_prefix('#')?.parse().ok()
}

/// One if the record carries `delta-open` or `event-open`, else zero.
/// A record with both labels counts once.
fn open_markers(labels: &[String]) -> usize {
    usize::from(
        labels
            .iter()
            .any(|label| label.as_str() == DELTA_OPEN_LABEL || label.as_str() == EVENT_OPEN_LABEL),
    )
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
            surface: None,
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
            surface: None,
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
                surface: None,
            }]
        );
    }

    /// The context of one pull request that changes the model file.
    fn model_context(paths: &[&str]) -> ContractContext<'static> {
        ContractContext {
            criteria: Vec::new(),
            features: Vec::new(),
            areas: Vec::new(),
            owned_paths: vec!["theory/**".to_string()],
            changed_paths: paths.iter().map(|path| path.to_string()).collect(),
            manifests: &contract::MANIFESTS,
            ticket_names_dependency: false,
        }
    }

    /// The smallest body the Before / After contract accepts.
    const EMPTY_CONTRACT: &str = "## Why\n\n## Before / After\n";

    #[test]
    fn check_pr_refuses_a_model_file_off_a_model_branch() {
        let ctx = model_context(&["theory/model.toml"]);
        let finding = check_pr(EMPTY_CONTRACT, "aif/borsuk/issue-142", &ctx)
            .expect_err("the model file needs the model branch");
        assert_eq!(
            finding.reason,
            "theory/model.toml changed off a model branch"
        );
    }

    #[test]
    fn check_pr_accepts_a_model_file_on_the_model_branch() {
        let ctx = model_context(&["theory/model.toml"]);
        check_pr(EMPTY_CONTRACT, "aif/borsuk/model-a1b2c3d4", &ctx)
            .expect("the model branch owns the model file");
    }

    #[test]
    fn check_pr_runs_the_body_rules_after_the_model_rule() {
        let ctx = model_context(&["theory/rules.md"]);
        let finding = check_pr("## Why\n", "aif/borsuk/model-1", &ctx)
            .expect_err("the body carries no Before / After section");
        assert_eq!(finding.reason, "section Before / After missing");
    }

    use std::path::PathBuf;

    use crate::config::{Config, Governor, RepoConfig, TheoryConfig, TheoryRepo};
    use crate::model::{Issue, Pr};

    /// The label names of one record, as plain string slices.
    fn names(labels: &[String]) -> Vec<&str> {
        labels.iter().map(String::as_str).collect()
    }

    /// One open issue with the given labels.
    fn issue(number: u64, title: &str, labels: &[&str]) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: title.to_string(),
            body: String::new(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            author: String::new(),
            assignees: Vec::new(),
            updated_at: String::new(),
            github_url: String::new(),
            open: true,
        }
    }

    /// One open pull request with the given labels.
    fn pr(number: u64, labels: &[&str]) -> Pr {
        Pr {
            number,
            node_id: format!("node-{number}"),
            title: format!("pr {number}"),
            body: String::new(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            open: true,
            draft: false,
            head_sha: String::new(),
            head_ref: String::new(),
        }
    }

    /// One snapshot with the given issues and pull requests.
    fn snapshot(issues: Vec<Issue>, pulls: Vec<Pr>) -> RepoSnapshot {
        let mut snap = RepoSnapshot::default();
        for issue in issues {
            snap.issues.insert(issue.number, issue);
        }
        for pr in pulls {
            snap.prs.insert(pr.number, pr);
        }
        snap
    }

    /// The theory settings of one governed code-mode repository.
    fn governed() -> TheoryConfig {
        TheoryConfig {
            governor: Governor::On,
            ..TheoryConfig::default()
        }
    }

    /// The theory settings of one governed shadow-mode repository.
    fn shadowed() -> TheoryConfig {
        TheoryConfig {
            theory: Some(TheoryRepo {
                repo: Some("acme/theory".to_string()),
                path: PathBuf::from("/tmp/theory"),
            }),
            ..TheoryConfig::default()
        }
    }

    /// The theory settings of one repository with the governor off.
    fn ungoverned() -> TheoryConfig {
        TheoryConfig {
            governor: Governor::Off,
            ..TheoryConfig::default()
        }
    }

    /// One config over the given repositories, keyed by alias.
    fn config_with(repos: Vec<(&str, TheoryConfig)>) -> Config {
        let config_repos = repos
            .into_iter()
            .map(|(alias, theory)| {
                (
                    alias.to_string(),
                    RepoConfig {
                        alias: alias.to_string(),
                        path: PathBuf::from(format!("/tmp/{alias}")),
                        owner_repo: format!("acme/{alias}"),
                        lanes: BTreeMap::new(),
                        release: crate::config::ReleasePolicy::Manual,
                        theory,
                        skills: None,
                        role_overrides: BTreeMap::new(),
                        tag_route_overrides: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Config {
            schema_version: 1,
            roles: BTreeMap::new(),
            stages: BTreeMap::new(),
            repos: config_repos,
            tag_route_overrides: BTreeMap::new(),
            ticket_chat: Default::default(),
            usage: Default::default(),
            measure: Default::default(),
        }
    }

    #[test]
    fn derive_reads_the_code_labels_and_open_markers_of_a_governed_alias() {
        let mut snapshots = Snapshot::default();
        snapshots.repos.insert(
            "borsuk".to_string(),
            snapshot(
                vec![
                    issue(5, "Add the poller", &["to-refine"]),
                    issue(7, "Bug report", &["delta-open"]),
                    issue(3, "borsuk/theory", &["event-open"]),
                    issue(4, "Both markers", &["delta-open", "event-open"]),
                ],
                vec![pr(9, &["event-open"])],
            ),
        );
        let config = config_with(vec![("borsuk", governed())]);

        let records = TheoryRecords::derive(&config, &snapshots, &BTreeMap::new());

        assert_eq!(
            names(records.labels_of("borsuk", &RecordKey::Issue(5))),
            vec!["to-refine"]
        );
        assert_eq!(
            names(records.labels_of("borsuk", &RecordKey::Pr(9))),
            vec!["event-open"]
        );
        assert_eq!(
            names(records.labels_of("borsuk", &RecordKey::Repo)),
            vec!["event-open"]
        );
        assert!(records
            .labels_of("borsuk", &RecordKey::Issue(11))
            .is_empty());
        assert_eq!(records.open_count("borsuk"), 4);
    }

    #[test]
    fn derive_reads_the_shadow_labels_of_a_shadowed_alias() {
        let mut theory_snapshots = BTreeMap::new();
        theory_snapshots.insert(
            "shade".to_string(),
            snapshot(
                vec![
                    issue(11, "shade#5", &["theory-short"]),
                    issue(12, "shade/theory", &["event-open"]),
                    issue(13, "unrelated", &["delta-open"]),
                    issue(14, "other#7", &["event-open"]),
                ],
                vec![],
            ),
        );
        let config = config_with(vec![("shade", shadowed())]);

        let records = TheoryRecords::derive(&config, &Snapshot::default(), &theory_snapshots);

        assert_eq!(
            names(records.labels_of("shade", &RecordKey::Issue(5))),
            vec!["theory-short"]
        );
        assert_eq!(
            names(records.labels_of("shade", &RecordKey::Pr(5))),
            vec!["theory-short"]
        );
        assert_eq!(
            names(records.labels_of("shade", &RecordKey::Repo)),
            vec!["event-open"]
        );
        // One theory repository may hold the shadow issues of several
        // aliases, so issues outside `shade` derive nothing and count
        // nothing.
        assert!(records.labels_of("shade", &RecordKey::Issue(13)).is_empty());
        assert!(records.labels_of("shade", &RecordKey::Issue(7)).is_empty());
        assert_eq!(records.open_count("shade"), 1);
    }

    #[test]
    fn derive_treats_a_path_only_theory_config_as_code_mode() {
        let mut snapshots = Snapshot::default();
        snapshots.repos.insert(
            "solo".to_string(),
            snapshot(vec![issue(5, "Add the poller", &["to-refine"])], vec![]),
        );
        let config = config_with(vec![(
            "solo",
            TheoryConfig {
                governor: Governor::On,
                theory: Some(TheoryRepo {
                    repo: None,
                    path: PathBuf::from("/tmp/theory"),
                }),
                ..TheoryConfig::default()
            },
        )]);

        let records = TheoryRecords::derive(&config, &snapshots, &BTreeMap::new());

        assert_eq!(
            names(records.labels_of("solo", &RecordKey::Issue(5))),
            vec!["to-refine"]
        );
        assert_eq!(records.open_count("solo"), 0);
    }

    #[test]
    fn derive_ignores_an_alias_with_the_governor_off_and_one_without_a_snapshot() {
        let mut snapshots = Snapshot::default();
        snapshots.repos.insert(
            "off".to_string(),
            snapshot(vec![issue(5, "Bug", &["delta-open"])], vec![]),
        );
        let config = config_with(vec![("off", ungoverned()), ("cold", governed())]);

        let records = TheoryRecords::derive(&config, &snapshots, &BTreeMap::new());

        assert!(records.labels_of("off", &RecordKey::Issue(5)).is_empty());
        assert_eq!(records.open_count("off"), 0);
        assert!(records.labels_of("cold", &RecordKey::Issue(5)).is_empty());
        assert_eq!(records.open_count("cold"), 0);
    }

    #[test]
    fn a_short_prediction_round_trips_through_its_block() {
        let prediction = ShortPrediction {
            kind: PREDICTION_SHORT.to_string(),
            text: "The poller parks the worker.".to_string(),
            areas: vec!["poll".to_string()],
        };
        let block = prediction_block(&Prediction::Short(prediction.clone()));

        assert!(block.starts_with(PREDICTION_BLOCK));
        assert!(block.contains("\"kind\":\"short\""), "{block}");
        assert_eq!(
            parse_prediction_blocks(&block),
            vec![Prediction::Short(prediction)]
        );
    }

    #[test]
    fn a_full_prediction_round_trips_through_its_block() {
        let prediction = FullPrediction {
            kind: PREDICTION_FULL.to_string(),
            slots: PREDICTION_SLOT_NAMES
                .iter()
                .enumerate()
                .map(|(index, name)| PredictionSlot {
                    name: (*name).to_string(),
                    entries: vec![format!("INV-{index}")],
                    tag: if index % 2 == 0 {
                        PredictionTag::Sure
                    } else {
                        PredictionTag::Unsure
                    },
                })
                .collect(),
        };
        assert_eq!(prediction.slots.len(), PREDICTION_SLOT_NAMES.len());

        let block = prediction_block(&Prediction::Full(prediction.clone()));

        assert_eq!(
            parse_prediction_blocks(&block),
            vec![Prediction::Full(prediction)]
        );
    }

    #[test]
    fn parse_prediction_blocks_skips_broken_bodies_and_keeps_the_order() {
        let short = ShortPrediction {
            kind: PREDICTION_SHORT.to_string(),
            text: "one line".to_string(),
            areas: vec![],
        };
        let full = FullPrediction {
            kind: PREDICTION_FULL.to_string(),
            slots: vec![PredictionSlot {
                name: "behaviours".to_string(),
                entries: vec!["INV-3".to_string()],
                tag: PredictionTag::Unsure,
            }],
        };
        let transcript = format!(
            "prose\n{}\nmid\n{PREDICTION_BLOCK}\nnot json\n{}\n{PREDICTION_BLOCK}\n\
             {{\"kind\":\"banana\",\"text\":\"x\",\"areas\":[]}}\n{}\n\
             {PREDICTION_BLOCK}\nnever closed",
            prediction_block(&Prediction::Full(full)),
            close_tag(PREDICTION_BLOCK),
            prediction_block(&Prediction::Short(short)),
        );

        assert_eq!(
            parse_prediction_blocks(&transcript),
            vec![
                Prediction::Full(FullPrediction {
                    kind: PREDICTION_FULL.to_string(),
                    slots: vec![PredictionSlot {
                        name: "behaviours".to_string(),
                        entries: vec!["INV-3".to_string()],
                        tag: PredictionTag::Unsure,
                    }],
                }),
                Prediction::Short(ShortPrediction {
                    kind: PREDICTION_SHORT.to_string(),
                    text: "one line".to_string(),
                    areas: vec![],
                }),
            ]
        );
    }
}
