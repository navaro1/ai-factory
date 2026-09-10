//! The theory records: the labels, the block tags, and the event blocks.
//!
//! A theory record is the GitHub issue that holds the theory state of one
//! item. [`RecordKey`] names the item; the daemon resolves it to an
//! `(owner_repo, number)` pair. Agents and the daemon ship facts as tagged
//! blocks, and [`parse_event_blocks`] reads the event blocks back.
//! [`TheoryRecords`] is the read model of one poll: the labels of every
//! record, the open marker count, and the predictions.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::contract::{self, ContractContext, Finding};
use super::model::{Entry, Model};
use crate::config::Config;
use crate::model::{RepoSnapshot, Snapshot};

/// The label that marks a ticket with an accepted short prediction.
pub const THEORY_SHORT_LABEL: &str = "theory-short";

/// The color GitHub renders `theory-short` with, as six hex digits.
pub const THEORY_SHORT_COLOR: &str = "c5def5";
/// The label that marks a ticket with an accepted full prediction.
pub const THEORY_FULL_LABEL: &str = "theory-full";

/// The color GitHub renders `theory-full` with, as six hex digits.
pub const THEORY_FULL_COLOR: &str = "1d76db";
/// The label that marks a record with an open delta.
pub const DELTA_OPEN_LABEL: &str = "delta-open";
/// The label that marks a record with an open theory event.
pub const EVENT_OPEN_LABEL: &str = "event-open";
/// The label that marks a pull request that changes the model.
pub const MODEL_PR_LABEL: &str = "model-pr";

/// The color GitHub renders `model-pr` with, as six hex digits.
pub const MODEL_PR_COLOR: &str = "1d76db";

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

/// The model file of one repository, relative to the theory checkout.
pub const MODEL_FILE: &str = "theory/model.toml";

/// The theory files only a model branch may change.
pub const MODEL_FILES: [&str; 3] = [MODEL_FILE, "theory/verify.toml", "theory/rules.md"];

/// The heading one pull request body must carry.
pub const SECTION_WHY: &str = "## Why";

/// The heading one pull request body must not carry.
pub const SECTION_HOW: &str = "## How";

/// The heading text no pull request body may open with.
const IMPLEMENTATION_PREFIX: &str = "Implementation";

/// Check the body and the diff of one governed pull request.
///
/// A branch that is not the model branch may not change a theory file.
/// The body then carries [`SECTION_WHY`], carries no [`SECTION_HOW`], and
/// opens no heading with [`IMPLEMENTATION_PREFIX`]. Every other rule is
/// the Before / After contract of [`contract::check_body_lines`], and the
/// changed paths of `ctx` are the same list both rules read. The first
/// broken rule wins.
pub fn check_pr(body: &str, branch: &str, ctx: &ContractContext<'_>) -> Result<(), Finding> {
    if !is_model_branch(branch) {
        for path in &ctx.changed_paths {
            if let Some(file) = MODEL_FILES.iter().find(|file| path == *file) {
                return Err(Finding::plain(format!("{file} changed off a model branch")));
            }
        }
    }
    check_headings(body)?;
    contract::check_body_lines(body, ctx)
}

/// The heading rules of one pull request body.
///
/// The body says why the change happened. It never says how the agent
/// worked, so a `## How` section and a heading that opens with
/// `Implementation` are both refused.
fn check_headings(body: &str) -> Result<(), Finding> {
    if !body.lines().any(|line| line.trim() == SECTION_WHY) {
        return Err(Finding::plain("section Why missing"));
    }
    for line in body.lines() {
        let line = line.trim();
        if line == SECTION_HOW {
            return Err(Finding::plain("section How is not allowed"));
        }
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let name = rest.trim_start_matches('#').trim();
        if name.starts_with(IMPLEMENTATION_PREFIX) {
            return Err(Finding::plain(format!("heading {name} is not allowed")));
        }
    }
    Ok(())
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

impl RecordKey {
    /// The text this key carries in [`TheoryView::records`], for example
    /// `issue-142`, `pr-5`, or `repo`.
    ///
    /// [`TheoryView::records`]: crate::sock::TheoryView::records
    pub fn key_text(&self) -> String {
        match self {
            Self::Issue(number) => format!("issue-{number}"),
            Self::Pr(number) => format!("pr-{number}"),
            Self::Repo => "repo".to_string(),
        }
    }
}

/// The theory labels whose first sight asks for the record comments.
pub const RECORD_LABELS: [&str; 4] = [
    THEORY_SHORT_LABEL,
    THEORY_FULL_LABEL,
    DELTA_OPEN_LABEL,
    EVENT_OPEN_LABEL,
];

/// The theory labels of one label list, in [`RECORD_LABELS`] order.
pub fn record_labels(labels: &[String]) -> Vec<String> {
    RECORD_LABELS
        .into_iter()
        .filter(|wanted| labels.iter().any(|label| label == wanted))
        .map(str::to_string)
        .collect()
}

/// True when the labels of one item skip both prediction gates.
///
/// A `model-pr` changes the model itself, and a `verify-skill` ticket
/// changes no application behaviour, so neither one carries a prediction.
/// Every other check still runs for both: the ticket check, the body
/// check, the fast checks, and the review.
pub fn skips_prediction_gates(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|label| label == MODEL_PR_LABEL || label == VERIFY_SKILL_LABEL)
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

/// The index of the slot that names areas instead of entries.
const OTHER_AREAS_SLOT: usize = 4;

/// The slot whose entries name model areas, not model entries.
pub const PREDICTION_OTHER_AREAS: &str = PREDICTION_SLOT_NAMES[OTHER_AREAS_SLOT];

/// The entry kind each slot draws its candidates from, in slot order.
const SLOT_KINDS: [&str; 5] = ["transition", "state", "invariant", "failure", "boundary"];

/// The confidence tag of one prediction slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PredictionTag {
    /// The operator is sure the slot lists every entry in play.
    Sure,
    /// The operator is not sure.
    Unsure,
}

impl PredictionTag {
    /// The lowercase name of the tag, as a block writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sure => "sure",
            Self::Unsure => "unsure",
        }
    }
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

/// The refusal reason of one area the model does not cover.
pub fn no_entries(area: &str) -> String {
    format!("area {area} has no entries")
}

/// True when one refusal reason names an area with no entries.
///
/// The Theory view offers the bootstrap action on such a hold alone,
/// because bootstrapping fixes nothing else.
pub fn names_empty_area(reason: &str) -> bool {
    reason.starts_with("area ") && reason.ends_with(" has no entries")
}

/// The template of one full prediction, for the areas of the short one.
///
/// The file holds one table per slot of [`PREDICTION_SLOT_NAMES`], each
/// with an empty entry list and the `unsure` tag. A comment line above
/// each table lists the candidate entry ids, so the operator picks from
/// the model instead of recalling ids. The comment is a hint, never a
/// bound: [`parse_full`] takes any entry of the model.
pub fn full_template(areas: &[String], model: &Model) -> String {
    let mut blocks = Vec::with_capacity(PREDICTION_SLOT_NAMES.len());
    for (slot, name) in PREDICTION_SLOT_NAMES.iter().enumerate() {
        let ids = candidates(slot, model, areas);
        let list = if ids.is_empty() {
            "none".to_string()
        } else {
            ids.join(", ")
        };
        blocks.push(format!(
            "# candidates: {list}\n[{name}]\nentries = []\ntag = \"unsure\"\n"
        ));
    }
    blocks.join("\n")
}

/// The candidate entry ids of one slot, in model order.
///
/// The first four slots take the entries of the named areas that carry
/// the slot's kind. The `other-areas` slot takes every boundary the areas
/// leave out, because that slot names areas the change can reach.
fn candidates(slot: usize, model: &Model, areas: &[String]) -> Vec<String> {
    let names = area_names(model, areas);
    model
        .entries
        .iter()
        .filter(|entry| entry.kind_name() == SLOT_KINDS[slot])
        .filter(|entry| {
            if slot == OTHER_AREAS_SLOT {
                return !areas.iter().any(|area| area == entry.id());
            }
            names.contains(entry.id()) || relations(entry).iter().any(|name| names.contains(name))
        })
        .map(|entry| entry.id().to_string())
        .collect()
}

/// The names one set of areas covers: each area and the sides of its
/// boundary entry.
///
/// A state names no boundary and a transition names no boundary either,
/// so the sides carry the area down to the states it separates and to the
/// transitions between them.
fn area_names<'a>(model: &'a Model, areas: &'a [String]) -> BTreeSet<&'a str> {
    let mut names: BTreeSet<&str> = areas.iter().map(String::as_str).collect();
    for entry in &model.entries {
        if let Entry::Boundary { id, sides, .. } = entry {
            if areas.iter().any(|area| area == id) {
                names.extend(sides.iter().map(String::as_str));
            }
        }
    }
    names
}

/// What the relation fields of one entry point at.
fn relations(entry: &Entry) -> Vec<&str> {
    match entry {
        Entry::Invariant { constrains, .. } => constrains.iter().map(String::as_str).collect(),
        Entry::State { .. } => Vec::new(),
        Entry::Transition { from, to, .. } => vec![from.as_str(), to.as_str()],
        Entry::Boundary { sides, .. } => sides.iter().map(String::as_str).collect(),
        Entry::Failure { crosses, .. } => vec![crosses.as_str()],
    }
}

/// One slot table of the template file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    /// The entry ids the operator listed.
    #[serde(default)]
    entries: Vec<String>,
    /// The confidence tag the operator left or changed.
    #[serde(default)]
    tag: String,
}

/// Parse one edited template into a full prediction.
///
/// Every error names the slot and what broke in it, because the operator
/// fixes one slot at a time. The first four slots take entries of their
/// own kind, and an id may appear in one slot only. The `other-areas`
/// slot names areas, so the daemon validates it against the areas and
/// this parse lets its names through.
pub fn parse_full(text: &str, model: &Model) -> Result<FullPrediction, String> {
    let raw: BTreeMap<String, RawSlot> =
        toml::from_str(text).map_err(|error| format!("invalid TOML: {error}"))?;
    for name in raw.keys() {
        if !PREDICTION_SLOT_NAMES.contains(&name.as_str()) {
            return Err(format!("unknown slot {name}"));
        }
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut slots = Vec::with_capacity(PREDICTION_SLOT_NAMES.len());
    for (slot, name) in PREDICTION_SLOT_NAMES.iter().enumerate() {
        let Some(table) = raw.get(*name) else {
            return Err(format!("slot {name} is missing"));
        };
        let tag = match table.tag.as_str() {
            "sure" => PredictionTag::Sure,
            "unsure" => PredictionTag::Unsure,
            other => return Err(format!("slot {name}: unknown tag {other}")),
        };
        for id in &table.entries {
            if slot != OTHER_AREAS_SLOT {
                let Some(entry) = model.entries.iter().find(|entry| entry.id() == id) else {
                    return Err(format!("slot {name}: unknown entry {id}"));
                };
                let wanted = SLOT_KINDS[slot];
                if entry.kind_name() != wanted {
                    return Err(format!(
                        "slot {name}: the kind of {id} is {}, not {wanted}",
                        entry.kind_name()
                    ));
                }
            }
            if !seen.insert(id) {
                return Err(format!("slot {name}: duplicate entry {id}"));
            }
        }
        slots.push(PredictionSlot {
            name: (*name).to_string(),
            entries: table.entries.clone(),
            tag,
        });
    }
    Ok(FullPrediction {
        kind: PREDICTION_FULL.to_string(),
        slots,
    })
}

/// Check the shape of one full prediction that arrived over the wire.
///
/// [`parse_full`] builds the shape, so this catches a message that no
/// template wrote: a wrong kind, a missing slot, a slot the prediction
/// does not own, and a slot it names twice. The daemon runs it before it
/// writes anything.
pub fn check_full_shape(prediction: &FullPrediction) -> Result<(), String> {
    if prediction.kind != PREDICTION_FULL {
        return Err(format!(
            "the prediction kind is {}, not {PREDICTION_FULL}",
            prediction.kind
        ));
    }
    for name in PREDICTION_SLOT_NAMES {
        if !prediction.slots.iter().any(|slot| slot.name == name) {
            return Err(format!("slot {name} is missing"));
        }
    }
    for slot in &prediction.slots {
        if !PREDICTION_SLOT_NAMES.contains(&slot.name.as_str()) {
            return Err(format!("unknown slot {}", slot.name));
        }
    }
    if prediction.slots.len() != PREDICTION_SLOT_NAMES.len() {
        return Err("the prediction names one slot twice".to_string());
    }
    Ok(())
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
    /// The aliases whose governor is on.
    governed: BTreeSet<String>,
    /// The governed aliases whose model or verification map did not parse.
    model_errors: BTreeSet<String>,
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
            records.governed.insert(alias.clone());
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

    /// True when the governor of one alias is on.
    pub fn is_governed(&self, alias: &str) -> bool {
        self.governed.contains(alias)
    }

    /// Mark the model of one alias broken, or clear the mark.
    ///
    /// The model lives in the daemon cache, not in the snapshot, so the
    /// caller folds its state in after the derive. A governed item holds
    /// while the mark stands, because a broken model can validate
    /// nothing. An item that [`skips_prediction_gates`] names passes
    /// first and never reads the mark, so a `model-pr` still moves while
    /// the model is broken.
    pub fn set_model_error(&mut self, alias: &str, broken: bool) {
        if broken {
            self.model_errors.insert(alias.to_string());
        } else {
            self.model_errors.remove(alias);
        }
    }

    /// True when the model of one alias did not parse.
    pub fn model_error(&self, alias: &str) -> bool {
        self.model_errors.contains(alias)
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

    /// The model of the prediction tests: two areas, one of them
    /// `daemon`, each with its states, its transition, its invariant, and
    /// its failure mode.
    const PREDICTION_MODEL: &str = concat!(
        "[[entry]]\nkind = \"boundary\"\nid = \"daemon\"\ntitle = \"the daemon\"\n",
        "statement = \"the daemon owns the task table\"\nsides = [\"idle\", \"polling\"]\n",
        "paths = [\"src/daemon.rs\"]\n",
        "[[entry]]\nkind = \"state\"\nid = \"idle\"\ntitle = \"idle\"\n",
        "statement = \"the daemon waits\"\n",
        "[[entry]]\nkind = \"state\"\nid = \"polling\"\ntitle = \"polling\"\n",
        "statement = \"the daemon reads GitHub\"\n",
        "[[entry]]\nkind = \"state\"\nid = \"drawing\"\ntitle = \"drawing\"\n",
        "statement = \"the interface draws\"\n",
        "[[entry]]\nkind = \"transition\"\nid = \"poll\"\ntitle = \"poll\"\n",
        "statement = \"the daemon starts one poll\"\nfrom = \"idle\"\nto = \"polling\"\n",
        "[[entry]]\nkind = \"transition\"\nid = \"draw\"\ntitle = \"draw\"\n",
        "statement = \"the interface draws one frame\"\nfrom = \"drawing\"\nto = \"drawing\"\n",
        "[[entry]]\nkind = \"invariant\"\nid = \"one-poll\"\ntitle = \"one poll\"\n",
        "statement = \"one poll runs at a time\"\nconstrains = [\"daemon\"]\n",
        "[[entry]]\nkind = \"invariant\"\nid = \"one-frame\"\ntitle = \"one frame\"\n",
        "statement = \"one frame draws at a time\"\nconstrains = [\"drawing\"]\n",
        "[[entry]]\nkind = \"failure\"\nid = \"poll-storm\"\ntitle = \"poll storm\"\n",
        "statement = \"the daemon polls without a pause\"\ncrosses = \"daemon\"\n",
        "[[entry]]\nkind = \"failure\"\nid = \"torn-frame\"\ntitle = \"torn frame\"\n",
        "statement = \"the interface draws half a frame\"\ncrosses = \"tui\"\n",
        "[[entry]]\nkind = \"boundary\"\nid = \"tui\"\ntitle = \"the interface\"\n",
        "statement = \"the interface owns the screen\"\nsides = [\"drawing\", \"outside\"]\n",
        "paths = [\"src/tui/*.rs\"]\n",
    );

    /// The parsed model of the prediction tests.
    fn prediction_model() -> Model {
        super::super::model::parse(PREDICTION_MODEL).expect("the test model parses")
    }

    /// The areas of the short prediction of the template tests.
    fn daemon_area() -> Vec<String> {
        vec!["daemon".to_string()]
    }

    /// The template writes one table per slot, each empty and unsure, and
    /// names the candidates of the area the short prediction claimed.
    #[test]
    fn the_template_writes_five_slots_with_the_candidates_of_one_area() {
        let text = full_template(&daemon_area(), &prediction_model());
        assert_eq!(
            text,
            concat!(
                "# candidates: poll\n[behaviours]\nentries = []\ntag = \"unsure\"\n\n",
                "# candidates: idle, polling\n[states]\nentries = []\ntag = \"unsure\"\n\n",
                "# candidates: one-poll\n[invariants]\nentries = []\ntag = \"unsure\"\n\n",
                "# candidates: poll-storm\n[failure-modes]\nentries = []\ntag = \"unsure\"\n\n",
                "# candidates: tui\n[other-areas]\nentries = []\ntag = \"unsure\"\n",
            )
        );

        let parsed = parse_full(&text, &prediction_model()).expect("the template parses");
        assert_eq!(parsed.kind, PREDICTION_FULL);
        assert_eq!(
            parsed
                .slots
                .iter()
                .map(|slot| slot.name.as_str())
                .collect::<Vec<_>>(),
            PREDICTION_SLOT_NAMES.to_vec()
        );
        assert!(parsed
            .slots
            .iter()
            .all(|slot| slot.entries.is_empty() && slot.tag == PredictionTag::Unsure));
    }

    /// An area the model does not cover names no candidate at all, so the
    /// template says so instead of listing every entry.
    #[test]
    fn the_template_of_an_unknown_area_names_no_candidate() {
        let text = full_template(&["gh".to_string()], &prediction_model());
        assert!(
            text.starts_with("# candidates: none\n[behaviours]\n"),
            "{text}"
        );
        assert!(
            text.contains("# candidates: daemon, tui\n[other-areas]\n"),
            "every boundary is another area: {text}"
        );
    }

    /// Every parse error names the slot and what broke in it.
    #[test]
    fn parse_full_names_the_slot_and_what_broke_in_it() {
        let model = prediction_model();
        let template = full_template(&daemon_area(), &model);
        let rows: [(&str, &str, &str); 6] = [
            (
                "[invariants]\nentries = []",
                "[invariants]\nentries = [\"nope\"]",
                "slot invariants: unknown entry nope",
            ),
            (
                "[invariants]\nentries = []",
                "[invariants]\nentries = [\"idle\"]",
                "slot invariants: the kind of idle is state, not invariant",
            ),
            (
                "[behaviours]\nentries = []",
                "[behaviours]\nentries = [\"poll-storm\"]",
                "slot behaviours: the kind of poll-storm is failure, not transition",
            ),
            (
                "[states]\nentries = []",
                "[states]\nentries = [\"idle\", \"idle\"]",
                "slot states: duplicate entry idle",
            ),
            (
                "[states]\nentries = []\ntag = \"unsure\"",
                "[states]\nentries = []\ntag = \"maybe\"",
                "slot states: unknown tag maybe",
            ),
            (
                "[other-areas]\n",
                "[notes]\nentries = []\ntag = \"sure\"\n[other-areas]\n",
                "unknown slot notes",
            ),
        ];
        for (from, to, reason) in rows {
            let broken = template.replace(from, to);
            assert_ne!(broken, template, "the edit changed nothing: {to}");
            assert_eq!(parse_full(&broken, &model), Err(reason.to_string()));
        }

        let missing = template.replace("[failure-modes]\nentries = []\ntag = \"unsure\"\n", "");
        assert_eq!(
            parse_full(&missing, &model),
            Err("slot failure-modes is missing".to_string())
        );
    }

    /// An id may sit in one slot only, even across two slots.
    #[test]
    fn parse_full_refuses_the_same_id_in_two_slots() {
        let model = prediction_model();
        let text = full_template(&daemon_area(), &model)
            .replace("[states]\nentries = []", "[states]\nentries = [\"idle\"]")
            .replace(
                "[other-areas]\nentries = []",
                "[other-areas]\nentries = [\"idle\"]",
            );
        assert_eq!(
            parse_full(&text, &model),
            Err("slot other-areas: duplicate entry idle".to_string())
        );
    }

    /// The shape check guards the wire against a message no template
    /// wrote.
    #[test]
    fn check_full_shape_names_every_broken_shape() {
        let model = prediction_model();
        let good = parse_full(&full_template(&daemon_area(), &model), &model).unwrap();
        assert_eq!(check_full_shape(&good), Ok(()));

        let mut short = good.clone();
        short.kind = PREDICTION_SHORT.to_string();
        assert_eq!(
            check_full_shape(&short),
            Err("the prediction kind is short, not full".to_string())
        );

        let mut missing = good.clone();
        missing.slots.remove(2);
        assert_eq!(
            check_full_shape(&missing),
            Err("slot invariants is missing".to_string())
        );

        let mut unknown = good.clone();
        unknown.slots[1].name = "notes".to_string();
        assert_eq!(
            check_full_shape(&unknown),
            Err("slot states is missing".to_string()),
            "a renamed slot reads as the missing one"
        );

        let mut extra = good.clone();
        extra.slots.push(PredictionSlot {
            name: "notes".to_string(),
            entries: Vec::new(),
            tag: PredictionTag::Sure,
        });
        assert_eq!(
            check_full_shape(&extra),
            Err("unknown slot notes".to_string())
        );

        let mut twice = good;
        twice.slots.push(PredictionSlot {
            name: "states".to_string(),
            entries: Vec::new(),
            tag: PredictionTag::Sure,
        });
        assert_eq!(
            check_full_shape(&twice),
            Err("the prediction names one slot twice".to_string())
        );
    }

    /// The `other-areas` slot names areas, not model entries, so the
    /// parse lets an unknown name through and the daemon refuses it.
    #[test]
    fn parse_full_leaves_the_other_areas_slot_to_the_daemon() {
        let model = prediction_model();
        let text = full_template(&daemon_area(), &model).replace(
            "[other-areas]\nentries = []",
            "[other-areas]\nentries = [\"gh\"]",
        );
        let parsed = parse_full(&text, &model).expect("the parse takes any area name");
        assert_eq!(parsed.slots[4].entries, vec!["gh".to_string()]);
        assert!(names_empty_area(&no_entries("gh")));
        assert!(!names_empty_area("model error"));
    }

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
    fn check_pr_reads_the_headings_of_one_body_by_the_table() {
        let ctx = model_context(&[]);
        check_pr("## Why\n\n## Evidence\n\n## Before / After\n", "main", &ctx)
            .expect("Why and Evidence are the accepted pair");

        let cases = [
            ("## Evidence\n\n## Before / After\n", "section Why missing"),
            (
                "## Why\n\n## How\n\n## Before / After\n",
                "section How is not allowed",
            ),
            (
                "## Why\n\n## Implementation notes\n\n## Before / After\n",
                "heading Implementation notes is not allowed",
            ),
            (
                "## Why\n\n# Implementation\n\n## Before / After\n",
                "heading Implementation is not allowed",
            ),
        ];
        for (body, expected) in cases {
            let finding = check_pr(body, "main", &ctx).expect_err(expected);
            assert_eq!(finding.reason, expected, "body:\n{body}");
        }
    }

    #[test]
    fn check_pr_runs_the_heading_rules_after_the_model_rule() {
        let ctx = model_context(&["theory/verify.toml"]);
        let body = "## Why\n\n## How\n";

        let finding =
            check_pr(body, "aif/borsuk/issue-142", &ctx).expect_err("the model file rule wins");
        assert_eq!(
            finding.reason,
            "theory/verify.toml changed off a model branch"
        );

        // The model branch clears the first rule, so the heading rule
        // answers the same body.
        let finding =
            check_pr(body, "aif/borsuk/model-1", &ctx).expect_err("the heading rule answers");
        assert_eq!(finding.reason, "section How is not allowed");
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
