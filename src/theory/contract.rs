//! The ticket contract grammar and its deterministic checks.
//!
//! A refined ticket is a contract. The section `## Acceptance criteria`
//! holds one line per criterion, and every line names the check that
//! proves it, in the form `- AC-<n> · <statement> · check: <target>`.
//! This module parses that grammar. The checks that run before implement
//! dispatch build on it.

use std::fmt::{Display, Formatter};

use globset::Glob;

use super::records::SECTION_WHY;
use super::skills::Feature;
use super::verify::{Measurer, Tier};

/// The heading that opens the acceptance criteria of a ticket body.
pub const ACCEPTANCE_HEADING: &str = "## Acceptance criteria";

/// The separator of one contract line. A middle dot with one space on
/// each side.
pub const SEPARATOR: &str = " \u{b7} ";

/// The headings a refined ticket must carry. A bug ticket adds
/// [`SECTION_REPRO`].
pub const SECTIONS_REQUIRED: [&str; 4] = [
    "## Problem",
    "## Grounding",
    "## Decisions",
    ACCEPTANCE_HEADING,
];

/// The heading that only a bug ticket must carry.
pub const SECTION_REPRO: &str = "## Repro";

/// The heading that opens the Before / After lines of a pull request.
pub const BEFORE_AFTER_HEADING: &str = "## Before / After";

/// The headings a pull request body must not carry. The body is a
/// briefing, not a lab notebook.
pub const SECTIONS_FORBIDDEN: [&str; 2] = ["## Summary", "## Test plan"];

/// The heading that closes a pull request body with its blast radius.
pub const SECTION_BLAST_RADIUS: &str = "## Blast radius";

/// The only H2 headings a pull request body may carry.
pub const SECTIONS_ALLOWED: [&str; 3] = [SECTION_WHY, BEFORE_AFTER_HEADING, SECTION_BLAST_RADIUS];

/// The dependency manifests the scope rule guards.
pub const MANIFESTS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];

/// The most prose lines one pull request body may carry.
pub const MAX_PROSE_LINES: usize = 40;

/// The most lines one fenced transcript of a pull request body may carry.
/// The full transcript stays in the author's worktree.
pub const MAX_TRANSCRIPT_LINES: usize = 30;

/// The directory prefix every run skill lives under.
const SKILL_PREFIX: &str = ".claude/skills/run-";

/// The arrow of one measure line, as `12 \u{2192} 11 ms` writes it.
const MEASURE_ARROW: char = '\u{2192}';

/// The header cell that names the owned-path column of the plan table.
const PLAN_OWNED_COLUMN: &str = "Owned files or paths";
const FAST_COLUMN: &str = "Fast";

/// One acceptance criterion of a refined ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criterion {
    /// The number after `AC-`.
    pub id: u32,
    /// The falsifiable statement of the criterion.
    pub statement: String,
    /// The check that proves the criterion.
    pub target: CheckTarget,
}

/// The check that proves one criterion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckTarget {
    /// A full drive of the named feature on its surface.
    Drive(String),
    /// The fast command of the named feature file.
    Fast(String),
    /// The measurer with this id in `theory/verify.toml`.
    Measure(String),
}

/// One defect of a contract line, in the operator's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What is wrong.
    pub reason: String,
    /// The floor this finding reports, when the tier rule broke. The
    /// daemon opens a theory event from it.
    pub floor: Option<Floor>,
}

impl Finding {
    /// One finding that reports no floor.
    pub fn plain(reason: impl Into<String>) -> Self {
        Finding {
            reason: reason.into(),
            floor: None,
        }
    }
}

/// One broken floor: the area, the tier it demands, and the tier the
/// line reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Floor {
    /// The area whose floor the line missed.
    pub area: String,
    /// The lowest tier the area accepts.
    pub floor: Tier,
    /// The tier the line reached.
    pub reached: Tier,
}

impl Display for Finding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.reason)
    }
}

/// Parse the acceptance criteria of a ticket body.
///
/// The function reads the non-empty lines under [`ACCEPTANCE_HEADING`] up
/// to the next heading. One line yields one [`Criterion`], or one
/// [`Finding`] when the line carries no `check:`, carries no usable
/// `AC-<n>` id, or names an unknown or empty target. A body without the
/// heading yields nothing.
pub fn parse_criteria(body: &str) -> (Vec<Criterion>, Vec<Finding>) {
    let mut criteria = Vec::new();
    let mut findings = Vec::new();
    let Some((_before, section)) = body.split_once(ACCEPTANCE_HEADING) else {
        return (criteria, findings);
    };
    for line in section.lines().skip(1) {
        if line.starts_with('#') {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_criterion(line) {
            Ok(criterion) => criteria.push(criterion),
            Err(reason) => {
                let reason: String = match named_no_check(line, &reason) {
                    Some(text) => text,
                    None => reason.to_string(),
                };
                findings.push(Finding::plain(format!("{reason}: {line}")));
            }
        }
    }
    (criteria, findings)
}

/// Parse one criterion line, or name the rule it breaks.
fn parse_criterion(line: &str) -> Result<Criterion, String> {
    let Some((head, check)) = line.rsplit_once(SEPARATOR) else {
        return Err("criterion without a check".to_string());
    };
    let Some(check) = check.strip_prefix("check: ") else {
        return Err("criterion without a check".to_string());
    };
    let Some((id, statement)) = head.split_once(SEPARATOR) else {
        return Err("criterion without a check".to_string());
    };
    let Some(id) = id
        .strip_prefix("- AC-")
        .and_then(|id| id.parse::<u32>().ok())
        .filter(|id| *id > 0)
    else {
        return Err("criterion without an AC-<n> id".to_string());
    };
    let target = if let Some(feature) = check.strip_suffix(" drive") {
        named_target(id, feature, CheckTarget::Drive)?
    } else if let Some(feature) = check.strip_suffix(" fast") {
        named_target(id, feature, CheckTarget::Fast)?
    } else if let Some(measurer) = check.strip_prefix("measure ") {
        CheckTarget::Measure(measurer.to_string())
    } else {
        return Err("criterion names an unknown check target".to_string());
    };
    Ok(Criterion {
        id,
        statement: statement.to_string(),
        target,
    })
}

/// Wrap one feature name into a target, or reject an empty name.
fn named_target(
    id: u32,
    feature: &str,
    build: fn(String) -> CheckTarget,
) -> Result<CheckTarget, String> {
    if feature.is_empty() {
        return Err(format!("AC-{id} check names no feature"));
    }
    Ok(build(feature.to_string()))
}

/// The `AC-<n> has no check` reason of a broken line that names its
/// criterion id.
fn named_no_check(line: &str, reason: &str) -> Option<String> {
    if reason != "criterion without a check" {
        return None;
    }
    let rest = line.trim().strip_prefix("- AC-")?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(format!("AC-{} has no check", &rest[..end]))
}

/// Check the contract of one refined ticket.
///
/// The ticket must carry every section of the grammar, its acceptance
/// section must hold at least one criterion line, every criterion line
/// must parse, every check target must name a resolved feature, a
/// measurer of the map, or a feature the plan table declares as
/// `new: <feature>` in its Fast column, and the plan table must give
/// every chunk a non-empty owned path. `bug` adds the `## Repro`
/// section. The first broken rule wins.
pub fn check_ticket(
    body: &str,
    features: &[&Feature],
    measurers: &[Measurer],
    bug: bool,
) -> Result<(), Finding> {
    let mut sections: Vec<&str> = SECTIONS_REQUIRED.to_vec();
    if bug {
        sections.push(SECTION_REPRO);
    }
    for section in &sections {
        if !body.lines().any(|line| line.trim() == *section) {
            return Err(Finding::plain(format!("section {section} missing")));
        }
    }
    let (criteria, findings) = parse_criteria(body);
    if let Some(finding) = findings.into_iter().next() {
        return Err(finding);
    }
    if criteria.is_empty() {
        return Err(Finding::plain("no acceptance criteria"));
    }
    let new_features = new_features(body);
    for criterion in &criteria {
        if let Some(name) = unresolved_target(&criterion.target, features, &new_features, measurers)
        {
            return Err(Finding::plain(format!(
                "AC-{} check {name} is not a feature or a measurer",
                criterion.id
            )));
        }
    }
    check_plan(body)
}

/// The name of a target that names no resolved feature, no feature the
/// plan declares as new, and no measurer.
fn unresolved_target(
    target: &CheckTarget,
    features: &[&Feature],
    new_features: &[String],
    measurers: &[Measurer],
) -> Option<String> {
    let (name, found) = match target {
        CheckTarget::Drive(name) | CheckTarget::Fast(name) => (
            name,
            features.iter().any(|feature| &feature.id == name)
                || new_features.iter().any(|new| new == name),
        ),
        CheckTarget::Measure(id) => (id, measurers.iter().any(|measurer| &measurer.id == id)),
    };
    if found {
        None
    } else {
        Some(name.clone())
    }
}

/// The features the plan table declares as `new: <feature>` in its Fast
/// column. The table is the one whose header row names
/// [`PLAN_OWNED_COLUMN`]; a declared feature needs no run skill yet.
fn new_features(body: &str) -> Vec<String> {
    plan_column(body, FAST_COLUMN)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cell| cell.strip_prefix("new: ").map(str::to_string))
        .filter(|name| !name.is_empty())
        .collect()
}

/// The paths the plan table of one ticket owns.
///
/// One cell may name several paths, separated by commas. A path keeps the
/// glob the plan wrote, and loses the backticks that render it as code.
/// A body without the table owns nothing.
pub fn owned_paths(body: &str) -> Vec<String> {
    plan_column(body, PLAN_OWNED_COLUMN)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|cell| {
            cell.split(',')
                .map(|path| path.trim().trim_matches('`').trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|path| !path.is_empty() && path != "-")
        .collect()
}

/// True when one ticket body names a dependency change.
///
/// The scope rule lets a manifest change only for such a ticket. The word
/// or the manifest name is enough, because the refine agent writes both.
pub fn names_dependency(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("dependency")
        || lower.contains("dependencies")
        || MANIFESTS.iter().any(|name| body.contains(name))
}

/// Check the plan table of a ticket body.
///
/// The table is the one whose header row names [`PLAN_OWNED_COLUMN`].
/// Every data row must give its chunk a non-empty owned path. A body
/// without the table misses the plan.
fn check_plan(body: &str) -> Result<(), Finding> {
    let Some(rows) = plan_column(body, PLAN_OWNED_COLUMN) else {
        return Err(Finding::plain("plan table missing"));
    };
    for (index, cell) in rows.iter().enumerate() {
        if cell.is_empty() {
            return Err(Finding::plain(format!(
                "plan row {} has an empty owned path",
                index + 1
            )));
        }
    }
    Ok(())
}

/// The cell of one plan-table column, one entry per data row.
///
/// The table is the first one whose header row names `header`, and it ends
/// at the first line that is not a row. A body without such a header
/// yields `None`.
fn plan_column(body: &str, header: &str) -> Option<Vec<String>> {
    let mut column: Option<usize> = None;
    let mut cells = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        match column {
            None => {
                if line.starts_with('|') {
                    column = table_cells(line).iter().position(|cell| *cell == header);
                }
            }
            Some(at) => {
                if !line.starts_with('|') {
                    break;
                }
                let row = table_cells(line);
                if row.iter().all(|cell| is_separator_cell(cell)) {
                    continue;
                }
                cells.push(row.get(at).copied().unwrap_or_default().to_string());
            }
        }
    }
    column.map(|_| cells)
}

/// The cells of one table row, without the edge pipes.
fn table_cells(line: &str) -> Vec<&str> {
    let mut cells: Vec<&str> = line.split('|').skip(1).map(str::trim).collect();
    if cells.last().is_some_and(|cell| cell.is_empty()) {
        cells.pop();
    }
    cells
}

/// True when one table cell is part of the dash separator row.
fn is_separator_cell(cell: &str) -> bool {
    cell.is_empty() || cell.trim_matches(['-', ':']).is_empty()
}

/// The tier one Before / After line reached, or the measure mark.
///
/// A measure line carries the daemon's own number, so no driver tier
/// applies to it and the floor rule leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierOrMeasure {
    /// The line drove the surface at this tier.
    Tier(Tier),
    /// The line reports a measurer.
    Measure,
}

impl TierOrMeasure {
    /// The lowercase name a line writes.
    pub fn name(self) -> &'static str {
        match self {
            TierOrMeasure::Tier(tier) => tier.name(),
            TierOrMeasure::Measure => "measure",
        }
    }

    /// Parse one tier name or the word `measure`.
    pub fn parse(text: &str) -> Option<Self> {
        if text == "measure" {
            return Some(TierOrMeasure::Measure);
        }
        Tier::parse(text).map(TierOrMeasure::Tier)
    }
}

impl Display for TierOrMeasure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One Before / After line of a pull request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeforeAfterLine {
    /// The number after `AC-`, the criterion this line proves.
    pub id: u32,
    /// The feature or the measurer the line drove.
    pub target: String,
    /// How far the drive reached.
    pub tier: TierOrMeasure,
    /// The command the reviewer re-runs, without its backticks.
    pub command: String,
    /// What the agent observed before the change.
    pub before: String,
    /// What the agent observed after it.
    pub after: String,
}

/// Everything the body check reads besides the body itself.
///
/// The daemon builds it per pull request: the criteria of the linked
/// ticket, the features its areas resolve to, the touched areas with their
/// floor, the measurers of the map, the owned paths of the plan table, and
/// the changed paths of the head diff. A pull request that links no ticket
/// carries an empty criteria list, an empty owned-path list, and
/// `has_ticket` false, and the three rules that read a ticket stand down.
#[derive(Debug, Clone)]
pub struct ContractContext<'a> {
    /// The acceptance criteria of the linked ticket.
    pub criteria: Vec<Criterion>,
    /// The features the resolved run skills carry.
    pub features: Vec<Feature>,
    /// The touched areas, each with the floor it demands.
    pub areas: Vec<(String, Tier)>,
    /// The measurers of the verification map.
    pub measurers: Vec<Measurer>,
    /// The paths the plan table of the ticket owns.
    pub owned_paths: Vec<String>,
    /// The paths the head diff changes.
    pub changed_paths: Vec<String>,
    /// The dependency manifests the scope rule guards.
    pub manifests: &'a [&'a str],
    /// Whether the ticket names a dependency.
    pub ticket_names_dependency: bool,
    /// Whether one ticket backs this pull request. A pull request that
    /// links none carries no criteria and no plan, so the trace rule, the
    /// area half of the coverage rule, and the scope rule do not run.
    pub has_ticket: bool,
}

/// Parse the Before / After lines of a pull request body.
///
/// The function reads the lines under [`BEFORE_AFTER_HEADING`] up to the
/// next heading and skips the fenced transcripts between them. One line
/// yields one [`BeforeAfterLine`], or one [`Finding`] that names its
/// position. A body without the heading yields nothing.
pub fn parse_lines(body: &str) -> (Vec<BeforeAfterLine>, Vec<Finding>) {
    let mut lines = Vec::new();
    let mut findings = Vec::new();
    let Some((_before, section)) = body.split_once(BEFORE_AFTER_HEADING) else {
        return (lines, findings);
    };
    let mut fenced = false;
    let mut number = 0;
    for line in section.lines().skip(1) {
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if line.starts_with('#') {
            break;
        }
        if line.is_empty() {
            continue;
        }
        number += 1;
        match parse_before_after(line) {
            Ok(parsed) => lines.push(parsed),
            Err(reason) => findings.push(Finding::plain(format!("line {number} {reason}"))),
        }
    }
    (lines, findings)
}

/// Parse one Before / After line, or name the rule it breaks.
///
/// The grammar holds six fields. A measure line holds five, because the
/// daemon writes its own number as `<before> \u{2192} <after>`.
fn parse_before_after(line: &str) -> Result<BeforeAfterLine, String> {
    let fields: Vec<&str> = line.split(SEPARATOR).map(str::trim).collect();
    if fields.len() < 5 || fields.len() > 6 {
        return Err(format!(
            "has {} fields, a Before / After line has six",
            fields.len()
        ));
    }
    let Some(id) = fields[0]
        .strip_prefix("- AC-")
        .and_then(|id| id.parse::<u32>().ok())
        .filter(|id| *id > 0)
    else {
        return Err("has no AC-<n> id".to_string());
    };
    let target = fields[1];
    if target.is_empty() {
        return Err("names no feature".to_string());
    }
    let Some(tier) = TierOrMeasure::parse(fields[2]) else {
        return Err(format!("names the unknown tier {}", fields[2]));
    };
    let command = fields[3].trim_matches('`').trim();
    if command.is_empty() {
        return Err("names no command".to_string());
    }
    let (before, after) = if fields.len() == 6 {
        let Some(before) = fields[4].strip_prefix("before: ") else {
            return Err("has no before text".to_string());
        };
        let Some(after) = fields[5].strip_prefix("after: ") else {
            return Err("has no after text".to_string());
        };
        (before.trim().to_string(), after.trim().to_string())
    } else if tier == TierOrMeasure::Measure {
        match fields[4].split_once(MEASURE_ARROW) {
            Some((before, after)) => (before.trim().to_string(), after.trim().to_string()),
            None => (String::new(), fields[4].to_string()),
        }
    } else {
        return Err("has five fields and names no measure".to_string());
    };
    Ok(BeforeAfterLine {
        id,
        target: target.to_string(),
        tier,
        command: command.to_string(),
        before,
        after,
    })
}

/// Check the Before / After contract of one pull request body.
///
/// The seven rules run in this order: sections, trace, coverage, tier,
/// state, scope, and prose. The first broken rule wins, and a broken
/// floor carries its [`Floor`] so the daemon can open the event.
///
/// Three of the seven read the linked ticket: trace, the area half of
/// coverage, and scope. A body with `has_ticket` false runs without those
/// three. Every other rule reads the body and the verification map alone,
/// so it answers with or without a ticket.
pub fn check_body_lines(body: &str, ctx: &ContractContext<'_>) -> Result<(), Finding> {
    check_sections(body)?;
    let (lines, findings) = parse_lines(body);
    if let Some(finding) = findings.into_iter().next() {
        return Err(finding);
    }
    if ctx.has_ticket {
        check_trace(&lines, ctx)?;
    }
    check_coverage(&lines, ctx)?;
    check_tier(&lines, ctx)?;
    check_state(&lines)?;
    if ctx.has_ticket {
        check_scope(ctx)?;
    }
    check_prose(body)
}

/// The sections rule: the body carries the Before / After section and the
/// Blast radius section, carries no section it forbids, and opens no other
/// H2 heading.
///
/// A transcript may hold a heading of its own, so the rule reads the lines
/// outside the fenced blocks alone.
fn check_sections(body: &str) -> Result<(), Finding> {
    let lines = unfenced_lines(body);
    for heading in SECTIONS_FORBIDDEN {
        if lines.contains(&heading) {
            return Err(Finding::plain(format!(
                "section {} is not allowed",
                heading_name(heading)
            )));
        }
    }
    for heading in [BEFORE_AFTER_HEADING, SECTION_BLAST_RADIUS] {
        if !lines.contains(&heading) {
            return Err(Finding::plain(format!(
                "section {} missing",
                heading_name(heading)
            )));
        }
    }
    for line in &lines {
        if line.starts_with("## ") && !SECTIONS_ALLOWED.contains(line) {
            return Err(Finding::plain(format!(
                "heading {} is not allowed",
                heading_name(line)
            )));
        }
    }
    Ok(())
}

/// The backtick width of one line, when the line is a fence.
///
/// A fence runs three backticks or more. The block one fence opens closes
/// at the first fence at least as wide, so a wider fence holds a narrower
/// one as content.
fn fence_width(line: &str) -> Option<usize> {
    let width = line.trim_start().chars().take_while(|c| *c == '`').count();
    (width >= 3).then_some(width)
}

/// Every line of one body that sits outside a fenced block, trimmed.
fn unfenced_lines(body: &str) -> Vec<&str> {
    let mut open: Option<usize> = None;
    let mut lines = Vec::new();
    for line in body.lines() {
        match (open, fence_width(line)) {
            (None, Some(width)) => open = Some(width),
            (Some(width), Some(closing)) if closing >= width => open = None,
            (None, None) => lines.push(line.trim()),
            _ => {}
        }
    }
    lines
}

/// The name of one heading, without its hashes.
fn heading_name(heading: &str) -> &str {
    heading.trim_start_matches('#').trim()
}

/// The trace rule: every criterion has a line, and every line names a
/// criterion the ticket carries.
fn check_trace(lines: &[BeforeAfterLine], ctx: &ContractContext<'_>) -> Result<(), Finding> {
    for criterion in &ctx.criteria {
        if !lines.iter().any(|line| line.id == criterion.id) {
            return Err(Finding::plain(format!(
                "AC-{} has no Before / After line",
                criterion.id
            )));
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if !ctx.criteria.iter().any(|criterion| criterion.id == line.id) {
            return Err(Finding::plain(format!(
                "line {} names AC-{} which the ticket lacks",
                index + 1,
                line.id
            )));
        }
    }
    Ok(())
}

/// The coverage rule: every touched area that resolves a feature has at
/// least one line, and every line names a resolved feature or a measurer
/// of the map.
///
/// The area half asks the ticket for a line per area, so a body with no
/// ticket behind it skips that half. The target half reads the
/// verification map, so it runs either way.
fn check_coverage(lines: &[BeforeAfterLine], ctx: &ContractContext<'_>) -> Result<(), Finding> {
    if ctx.has_ticket {
        check_areas(lines, ctx)?;
    }
    for (index, line) in lines.iter().enumerate() {
        if let Some(finding) = unknown_target(index, line, ctx) {
            return Err(finding);
        }
    }
    Ok(())
}

/// The area half of the coverage rule: every touched area that resolves a
/// feature has at least one line.
fn check_areas(lines: &[BeforeAfterLine], ctx: &ContractContext<'_>) -> Result<(), Finding> {
    for (area, _floor) in &ctx.areas {
        let bound: Vec<&Feature> = ctx
            .features
            .iter()
            .filter(|feature| &feature.area == area)
            .collect();
        if bound.is_empty() {
            continue;
        }
        if !lines
            .iter()
            .any(|line| bound.iter().any(|feature| feature.id == line.target))
        {
            return Err(Finding::plain(format!("area {area} has no line")));
        }
    }
    Ok(())
}

/// The finding of one line whose target names nothing the factory can
/// drive.
///
/// A tier line names a resolved feature. A `measure` line names a measurer
/// of the verification map. Either name resolves, or the line proves
/// nothing and the body check refuses it.
fn unknown_target(
    index: usize,
    line: &BeforeAfterLine,
    ctx: &ContractContext<'_>,
) -> Option<Finding> {
    let known = match line.tier {
        TierOrMeasure::Tier(_) => ctx.features.iter().any(|feature| feature.id == line.target),
        TierOrMeasure::Measure => ctx
            .measurers
            .iter()
            .any(|measurer| measurer.id == line.target),
    };
    if known {
        return None;
    }
    Some(Finding::plain(format!(
        "line {} names {} which is not a feature or a measurer",
        index + 1,
        line.target
    )))
}

/// The tier rule: every line names a target the factory knows and reaches
/// the floor of the area it drove.
fn check_tier(lines: &[BeforeAfterLine], ctx: &ContractContext<'_>) -> Result<(), Finding> {
    for (index, line) in lines.iter().enumerate() {
        if let Some(finding) = unknown_target(index, line, ctx) {
            return Err(finding);
        }
        let TierOrMeasure::Tier(reached) = line.tier else {
            continue;
        };
        let Some(area) = ctx
            .features
            .iter()
            .find(|feature| feature.id == line.target)
            .map(|feature| feature.area.as_str())
        else {
            continue;
        };
        let Some((_id, floor)) = ctx.areas.iter().find(|(id, _floor)| id == area) else {
            continue;
        };
        if reached < *floor {
            return Err(Finding {
                reason: format!(
                    "line {} tier {reached} is below the floor {floor}",
                    index + 1
                ),
                floor: Some(Floor {
                    area: area.to_string(),
                    floor: *floor,
                    reached,
                }),
            });
        }
    }
    Ok(())
}

/// The state rule: no line ends inconclusive.
fn check_state(lines: &[BeforeAfterLine]) -> Result<(), Finding> {
    for (index, line) in lines.iter().enumerate() {
        if line.after.contains("inconclusive") {
            return Err(Finding::plain(format!(
                "line {} is inconclusive",
                index + 1
            )));
        }
    }
    Ok(())
}

/// The scope rule: every changed path is one the ticket asked for.
///
/// A path holds when the plan owns it, when it belongs to a run skill, or
/// when it is a test file. A dependency manifest holds only when the
/// ticket names a dependency.
fn check_scope(ctx: &ContractContext<'_>) -> Result<(), Finding> {
    for path in &ctx.changed_paths {
        if let Some(manifest) = ctx
            .manifests
            .iter()
            .find(|manifest| is_manifest(path, manifest))
        {
            if !ctx.ticket_names_dependency {
                return Err(Finding::plain(format!(
                    "{manifest} changed and the ticket names no dependency"
                )));
            }
            continue;
        }
        if is_skill_path(path) || is_test_path(path) {
            continue;
        }
        if ctx.owned_paths.iter().any(|owned| owns(owned, path)) {
            continue;
        }
        return Err(Finding::plain(format!("{path} is outside the plan")));
    }
    Ok(())
}

/// True when one changed path belongs to a run skill directory.
///
/// The prefix alone is not enough. `.claude/skills/run-web/SKILL.md`
/// belongs to the `web` surface, and `.claude/skills/run-web.md` belongs
/// to no surface at all.
fn is_skill_path(path: &str) -> bool {
    path.strip_prefix(SKILL_PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(surface, _rest)| !surface.is_empty())
}

/// True when one changed path is the named dependency manifest.
fn is_manifest(path: &str, manifest: &str) -> bool {
    path == manifest || path.ends_with(&format!("/{manifest}"))
}

/// True when one changed path is a test file.
fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with("tests/")
        || path.contains("/tests/")
        || name.contains("_test.")
        || name.contains(".test.")
}

/// True when one owned path of the plan covers one changed path, as a
/// file, as a directory prefix, or as a glob.
fn owns(owned: &str, path: &str) -> bool {
    if owned == path {
        return true;
    }
    let directory = format!("{}/", owned.trim_end_matches('/'));
    if path.starts_with(&directory) {
        return true;
    }
    Glob::new(owned).is_ok_and(|glob| glob.compile_matcher().is_match(path))
}

/// The prose rule: the three lint rules of pstack, the line cap, and the
/// transcript cap.
fn check_prose(body: &str) -> Result<(), Finding> {
    let prose = lint_prose(body)?;
    if prose > MAX_PROSE_LINES {
        return Err(Finding::plain(format!("body has {prose} prose lines")));
    }
    check_transcripts(body)
}

/// One fenced block of a pull request body.
struct Block {
    /// The body line the opening fence sits on, counted from one.
    at: usize,
    /// How many backticks the opening fence runs.
    width: usize,
    /// How many lines sit between the fences.
    lines: usize,
}

/// The transcript cap: no fenced block runs past [`MAX_TRANSCRIPT_LINES`].
///
/// The finding names the line the block opens on and how many lines it
/// holds between its fences. A fence narrower than the one that opened the
/// block is content, not the close. A block the body never closes ends at
/// the last line.
fn check_transcripts(body: &str) -> Result<(), Finding> {
    let mut open: Option<Block> = None;
    for (index, line) in body.lines().enumerate() {
        let fence = fence_width(line);
        match &mut open {
            None => {
                if let Some(width) = fence {
                    open = Some(Block {
                        at: index + 1,
                        width,
                        lines: 0,
                    });
                }
            }
            Some(block) => {
                if fence.is_some_and(|closing| closing >= block.width) {
                    let block = open.take().expect("the walk just read an open block");
                    cap_block(&block)?;
                } else {
                    block.lines += 1;
                }
            }
        }
    }
    match open {
        Some(block) => cap_block(&block),
        None => Ok(()),
    }
}

/// Refuse one fenced block that runs past the transcript cap.
fn cap_block(block: &Block) -> Result<(), Finding> {
    if block.lines > MAX_TRANSCRIPT_LINES {
        return Err(Finding::plain(format!(
            "fenced block at body line {} has {} lines",
            block.at, block.lines
        )));
    }
    Ok(())
}

/// Run the three prose lint rules of pstack over one text, and count its
/// prose lines.
///
/// A prose line is a non-empty line outside a fenced block that is neither
/// a heading nor a Before / After line. The lint reads it with its code
/// spans removed, and names the line of the text it broke: a long dash, a
/// curly quote, or a colon that opens a clause. The stage prompts run the
/// same rules, so the factory writes what it asks an agent to write.
pub fn lint_prose(text: &str) -> Result<usize, Finding> {
    let mut fenced = false;
    let mut prose = 0;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.is_empty() || line.starts_with('#') || is_before_after_line(line) {
            continue;
        }
        prose += 1;
        let clean = strip_code(line);
        if clean.contains('\u{2013}') || clean.contains('\u{2014}') {
            return Err(Finding::plain(format!("long dash at line {number}")));
        }
        if clean.contains(['\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}']) {
            return Err(Finding::plain(format!("curly quote at line {number}")));
        }
        if has_colon(&clean) {
            return Err(Finding::plain(format!("colon at line {number}")));
        }
    }
    Ok(prose)
}

/// True when one line carries the shape of a Before / After line.
fn is_before_after_line(line: &str) -> bool {
    line.starts_with("- AC-") && line.contains(SEPARATOR)
}

/// One line with its code spans replaced by a single backtick.
fn strip_code(line: &str) -> String {
    line.split('`').step_by(2).collect::<Vec<_>>().join("`")
}

/// True when one line carries a colon that opens a clause.
fn has_colon(text: &str) -> bool {
    text.as_bytes()
        .windows(3)
        .any(|window| window[0] == b':' && window[1] == b' ' && window[2] != b' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theory::verify::Mode;

    #[test]
    fn parse_criteria_reads_three_lines_with_their_targets() {
        let body = concat!(
            "## Acceptance criteria\n",
            "- AC-1 · An empty card field blocks submit · check: checkout-submit drive\n",
            "- AC-2 · Orders rejects an empty card · check: api-orders fast\n",
            "- AC-3 · poll_p95 does not worsen · check: measure poll_p95\n",
            "\n",
            "## Implementation plan\n",
            "| Chunk |\n",
        );
        let (criteria, findings) = parse_criteria(body);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(criteria.len(), 3);
        assert_eq!(criteria[0].id, 1);
        assert_eq!(
            criteria[0].target,
            CheckTarget::Drive("checkout-submit".to_string())
        );
        assert_eq!(criteria[1].id, 2);
        assert_eq!(
            criteria[1].target,
            CheckTarget::Fast("api-orders".to_string())
        );
        assert_eq!(criteria[2].id, 3);
        assert_eq!(
            criteria[2].target,
            CheckTarget::Measure("poll_p95".to_string())
        );
    }

    #[test]
    fn parse_criteria_reports_a_line_without_a_check() {
        let body = concat!(
            "## Acceptance criteria\n",
            "- AC-1 · Submit is blocked · check: checkout drive\n",
            "- AC-2 · The card is validated\n",
        );
        let (criteria, findings) = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].reason,
            "AC-2 has no check: - AC-2 \u{b7} The card is validated"
        );
    }

    #[test]
    fn parse_criteria_reports_an_unknown_target_form() {
        let body =
            "## Acceptance criteria\n- AC-1 · The card is validated · check: checkout submits\n";
        let (criteria, findings) = parse_criteria(body);
        assert!(criteria.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].reason.contains("unknown check target"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn parse_criteria_reports_an_id_that_is_not_a_number() {
        let body =
            "## Acceptance criteria\n- AC-x · The card is validated · check: checkout drive\n";
        let (criteria, findings) = parse_criteria(body);
        assert!(criteria.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].reason.contains("without an AC-<n> id"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn parse_criteria_reports_a_negative_id() {
        let body =
            "## Acceptance criteria\n- AC--1 · The card is validated · check: checkout drive\n";
        let (criteria, findings) = parse_criteria(body);
        assert!(criteria.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].reason.contains("without an AC-<n> id"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn parse_criteria_reports_the_zero_id() {
        let body =
            "## Acceptance criteria\n- AC-0 · The card is validated · check: checkout drive\n";
        let (criteria, findings) = parse_criteria(body);
        assert!(criteria.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].reason.contains("without an AC-<n> id"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn parse_criteria_reports_an_empty_target_name() {
        let body = "## Acceptance criteria\n- AC-1 · The card is validated · check:  drive\n";
        let (criteria, findings) = parse_criteria(body);
        assert!(criteria.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].reason.contains("check names no feature"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn parse_criteria_yields_nothing_without_the_heading() {
        let (criteria, findings) = parse_criteria("no contract here");
        assert!(criteria.is_empty());
        assert!(findings.is_empty());
    }

    #[test]
    fn parse_criteria_stops_at_the_next_heading() {
        let body = concat!(
            "## Acceptance criteria\n",
            "- AC-1 · Submit is blocked · check: checkout drive\n",
            "## Implementation plan\n",
            "- AC-2 · not a criterion · check: checkout drive\n",
        );
        let (criteria, findings) = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(findings.is_empty());
    }

    /// A body that passes every rule of the ticket check.
    fn good_body() -> String {
        concat!(
            "## Problem\n",
            "The checkout rejects no card.\n",
            "\n",
            "## Grounding\n",
            "web-checkout covers the checkout form.\n",
            "\n",
            "## Decisions\n",
            "The submit blocks on an empty card field.\n",
            "\n",
            "## Acceptance criteria\n",
            "- AC-1 \u{b7} An empty card field blocks submit \u{b7} check: checkout drive\n",
            "- AC-2 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
            "\n",
            "# The implementation plan\n",
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Fast | Wave |\n",
            "|---|---|---|---|---|---|---|\n",
            "| 1 | Block the submit | src/checkout.rs | - | cargo test | npx playwright | 1 |\n",
        )
        .to_string()
    }

    /// The one resolved feature of the check tests.
    fn good_features() -> Vec<Feature> {
        vec![Feature {
            surface: "web".to_string(),
            id: "checkout".to_string(),
            area: "web-checkout".to_string(),
            fast: Some("npx playwright test checkout".to_string()),
            body: String::new(),
        }]
    }

    /// The one measurer of the check tests.
    fn good_measurers() -> Vec<Measurer> {
        vec![Measurer {
            id: "poll_p95".to_string(),
            area: "web-checkout".to_string(),
            command: "true".to_string(),
            mode: Mode::Fast,
            timeout_s: 30,
        }]
    }

    #[test]
    fn check_ticket_reports_the_first_broken_rule() {
        let features = good_features();
        let measurers = good_measurers();
        let feature_refs: Vec<&Feature> = features.iter().collect();
        let planless = good_body()
            .lines()
            .take_while(|line| !line.starts_with("| Chunk"))
            .chain(std::iter::once(""))
            .collect::<Vec<_>>()
            .join("\n");
        let cases: Vec<(String, bool, &str)> = vec![
            (
                good_body().replace("## Problem\n", ""),
                false,
                "section ## Problem missing",
            ),
            (good_body(), true, "section ## Repro missing"),
            (
                good_body().replace(
                    "- AC-2 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
                    "- AC-2 \u{b7} The poll budget holds\n",
                ),
                false,
                "AC-2 has no check",
            ),
            (
                good_body().replace("measure poll_p95", "api-orders fast"),
                false,
                "AC-2 check api-orders is not a feature or a measurer",
            ),
            (
                good_body().replace(
                    "- AC-2 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
                    "- AC-0 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
                ),
                false,
                "criterion without an AC-<n> id",
            ),
            (
                good_body().replace("check: measure poll_p95", "check:  drive"),
                false,
                "AC-2 check names no feature",
            ),
            (planless, false, "plan table missing"),
            (
                good_body().replace(
                    "| 1 | Block the submit | src/checkout.rs |",
                    "| 1 | Block the submit |  |",
                ),
                false,
                "plan row 1 has an empty owned path",
            ),
            (
                good_body().replace("measure poll_p95", "cli fast"),
                false,
                "AC-2 check cli is not a feature or a measurer",
            ),
            (
                good_body().replace(
                    concat!(
                        "- AC-1 \u{b7} An empty card field blocks submit \u{b7} check: checkout drive\n",
                        "- AC-2 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
                    ),
                    "",
                ),
                false,
                "no acceptance criteria",
            ),
        ];
        for (body, bug, expected) in cases {
            let finding = check_ticket(&body, &feature_refs, &measurers, bug).expect_err(expected);
            assert!(
                finding.reason.starts_with(expected),
                "expected {expected}, got {}",
                finding.reason
            );
        }
    }

    /// A feature that only the plan table declares as `new: <feature>`
    /// passes for an empty feature set, and fails without the
    /// declaration.
    #[test]
    fn check_ticket_accepts_a_feature_the_plan_declares_new() {
        let measurers = good_measurers();
        let empty: Vec<&Feature> = Vec::new();
        let only_cli = good_body().replace(
            concat!(
                "- AC-1 \u{b7} An empty card field blocks submit \u{b7} check: checkout drive\n",
                "- AC-2 \u{b7} The poll budget holds \u{b7} check: measure poll_p95\n",
            ),
            "- AC-2 \u{b7} The poll budget holds \u{b7} check: cli fast\n",
        );
        let declared = only_cli.replace(
            "| cargo test | npx playwright | 1 |",
            "| cargo test | new: cli | 1 |",
        );
        check_ticket(&declared, &empty, &measurers, false)
            .expect("the plan declares the cli feature");
        let undeclared = only_cli;
        let finding = check_ticket(&undeclared, &empty, &measurers, false)
            .expect_err("cli is no feature and no plan row declares it");
        assert_eq!(
            finding.reason,
            "AC-2 check cli is not a feature or a measurer"
        );
    }

    #[test]
    fn check_ticket_passes_a_contract_that_names_its_checks() {
        let features = good_features();
        let measurers = good_measurers();
        let feature_refs: Vec<&Feature> = features.iter().collect();
        check_ticket(&good_body(), &feature_refs, &measurers, false)
            .expect("the body passes every rule");
        let bug_body = format!(
            "{}\n## Repro\nrun npm test with an empty card\n",
            good_body()
        );
        check_ticket(&bug_body, &feature_refs, &measurers, true)
            .expect("the bug body carries its repro");
    }

    /// The three example lines of the design record, as one body.
    fn example_body() -> String {
        concat!(
            "## Why\n",
            "The checkout accepts an empty card field.\n",
            "\n",
            "## Before / After\n",
            "- AC-1 \u{b7} checkout-submit \u{b7} browser \u{b7} `npx playwright test checkout` \u{b7} before: an empty card is accepted and the API returns 500 \u{b7} after: the field shows a required message\n",
            "- AC-2 \u{b7} api-orders \u{b7} http \u{b7} `curl -s -X POST :4000/orders -d @empty.json` \u{b7} before: 500 \u{b7} after: 422\n",
            "- AC-3 \u{b7} poll_p95 \u{b7} measure \u{b7} `aif measure` \u{b7} 12 \u{2192} 11 ms\n",
            "\n",
            "## Blast radius\n",
            "The change touches the checkout form only.\n",
        )
        .to_string()
    }

    #[test]
    fn parse_lines_reads_the_three_example_lines_of_the_design() {
        let (lines, findings) = parse_lines(&example_body());
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines.iter().map(|line| line.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            lines.iter().map(|line| line.tier).collect::<Vec<_>>(),
            vec![
                TierOrMeasure::Tier(Tier::Browser),
                TierOrMeasure::Tier(Tier::Http),
                TierOrMeasure::Measure,
            ]
        );
        assert_eq!(lines[0].target, "checkout-submit");
        assert_eq!(lines[0].command, "npx playwright test checkout");
        assert_eq!(
            lines[0].before,
            "an empty card is accepted and the API returns 500"
        );
        assert_eq!(lines[2].before, "12");
        assert_eq!(lines[2].after, "11 ms");
    }

    #[test]
    fn parse_lines_rejects_a_line_with_four_fields() {
        let body = concat!(
            "## Before / After\n",
            "- AC-1 \u{b7} checkout \u{b7} browser \u{b7} `npx playwright test checkout`\n",
        );
        let (lines, findings) = parse_lines(body);
        assert!(lines.is_empty());
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].reason,
            "line 1 has 4 fields, a Before / After line has six"
        );
    }

    #[test]
    fn parse_lines_skips_a_fenced_transcript_between_the_lines() {
        let body = concat!(
            "## Before / After\n",
            "- AC-1 \u{b7} checkout \u{b7} browser \u{b7} `npx test` \u{b7} before: 500 \u{b7} after: 422\n",
            "```\n",
            "not a line at all\n",
            "```\n",
            "- AC-2 \u{b7} orders \u{b7} http \u{b7} `curl` \u{b7} before: 500 \u{b7} after: 422\n",
        );
        let (lines, findings) = parse_lines(body);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].id, 2);
    }

    /// The two criteria every body-check case traces to.
    fn body_criteria() -> Vec<Criterion> {
        vec![
            Criterion {
                id: 1,
                statement: "An empty card field blocks submit".to_string(),
                target: CheckTarget::Drive("checkout".to_string()),
            },
            Criterion {
                id: 2,
                statement: "Orders rejects an empty card".to_string(),
                target: CheckTarget::Fast("orders".to_string()),
            },
        ]
    }

    /// The two features the two criteria drive.
    fn body_features() -> Vec<Feature> {
        vec![
            Feature {
                surface: "web".to_string(),
                id: "checkout".to_string(),
                area: "web-checkout".to_string(),
                fast: Some("npx playwright test checkout".to_string()),
                body: String::new(),
            },
            Feature {
                surface: "api".to_string(),
                id: "orders".to_string(),
                area: "api-orders".to_string(),
                fast: None,
                body: String::new(),
            },
        ]
    }

    /// The one measurer of the map the body-check cases read.
    fn body_measurer() -> Measurer {
        Measurer {
            id: "poll_p95".to_string(),
            area: "api-orders".to_string(),
            command: "aif measure".to_string(),
            mode: Mode::Fast,
            timeout_s: 60,
        }
    }

    /// A pull request body that passes every rule of the body check.
    ///
    /// Line 3 and line 4 are its two opening prose lines, so a lint case
    /// can name the line it breaks.
    fn passing_body() -> String {
        concat!(
            "## Why\n",
            "\n",
            "The checkout accepts an empty card field.\n",
            "The submit must block on it.\n",
            "\n",
            "## Before / After\n",
            "- AC-1 \u{b7} checkout \u{b7} browser \u{b7} `npx playwright test checkout` \u{b7} before: 500 \u{b7} after: the field shows a required message\n",
            "- AC-2 \u{b7} orders \u{b7} http \u{b7} `curl -s -X POST :4000/orders` \u{b7} before: 500 \u{b7} after: 422\n",
            "\n",
            "## Blast radius\n",
            "The change touches the checkout form only.\n",
        )
        .to_string()
    }

    /// The context every body-check case starts from.
    fn body_context() -> ContractContext<'static> {
        ContractContext {
            criteria: body_criteria(),
            features: body_features(),
            areas: vec![
                ("web-checkout".to_string(), Tier::Browser),
                ("api-orders".to_string(), Tier::Http),
            ],
            measurers: vec![body_measurer()],
            owned_paths: vec!["src/checkout.rs".to_string(), "web/**".to_string()],
            changed_paths: vec!["src/checkout.rs".to_string()],
            manifests: &MANIFESTS,
            ticket_names_dependency: false,
            has_ticket: true,
        }
    }

    /// `passing_body()` with one more Before / After line under its two.
    fn third_line(line: &str) -> String {
        passing_body().replace("\n## Blast radius", &format!("{line}\n\n## Blast radius"))
    }

    /// The context of a body whose third line answers a third criterion.
    fn third_criterion_context(target: CheckTarget) -> ContractContext<'static> {
        let mut ctx = body_context();
        ctx.criteria.push(Criterion {
            id: 3,
            statement: "The number holds".to_string(),
            target,
        });
        ctx
    }

    /// One fenced block that holds `lines` lines between its fences.
    fn fenced_block(lines: usize) -> String {
        let inner: String = (0..lines).map(|n| format!("out {n}\n")).collect();
        format!("```\n{inner}```\n")
    }

    #[test]
    fn check_body_lines_passes_a_body_that_meets_every_rule() {
        check_body_lines(&passing_body(), &body_context()).expect("the body passes every rule");
    }

    #[test]
    fn check_body_lines_reports_one_finding_per_rule() {
        let long = format!(
            "{}{}",
            passing_body(),
            (0..38)
                .map(|n| format!("Prose line {n} says nothing.\n"))
                .collect::<String>()
        );
        let cases: Vec<(String, ContractContext<'static>, &str)> = vec![
            (
                passing_body().replace("## Before / After\n", "## Summary\n"),
                body_context(),
                "section Summary is not allowed",
            ),
            (
                passing_body().replace("## Before / After", "## What changed"),
                body_context(),
                "section Before / After missing",
            ),
            (
                passing_body().replace(
                    "- AC-2 \u{b7} orders \u{b7} http \u{b7} `curl -s -X POST :4000/orders` \u{b7} before: 500 \u{b7} after: 422\n",
                    "",
                ),
                body_context(),
                "AC-2 has no Before / After line",
            ),
            (
                passing_body().replace(
                    "\n## Blast radius",
                    "- AC-9 \u{b7} checkout \u{b7} browser \u{b7} `npx test` \u{b7} before: 500 \u{b7} after: 422\n\n## Blast radius",
                ),
                body_context(),
                "line 3 names AC-9 which the ticket lacks",
            ),
            (
                passing_body().replace(
                    "- AC-1 \u{b7} checkout \u{b7} browser",
                    "- AC-1 \u{b7} orders \u{b7} http",
                ),
                body_context(),
                "area web-checkout has no line",
            ),
            (
                passing_body().replace(
                    "- AC-1 \u{b7} checkout \u{b7} browser",
                    "- AC-1 \u{b7} checkout \u{b7} http",
                ),
                body_context(),
                "line 1 tier http is below the floor browser",
            ),
            (
                passing_body().replace("after: 422", "after: inconclusive"),
                body_context(),
                "line 2 is inconclusive",
            ),
            (
                passing_body(),
                ContractContext {
                    changed_paths: vec!["src/other.rs".to_string()],
                    ..body_context()
                },
                "src/other.rs is outside the plan",
            ),
            (
                passing_body(),
                ContractContext {
                    changed_paths: vec!["Cargo.toml".to_string()],
                    ..body_context()
                },
                "Cargo.toml changed and the ticket names no dependency",
            ),
            (
                passing_body().replace(
                    "The submit must block on it.",
                    "The submit must block on it \u{2014} and it does not.",
                ),
                body_context(),
                "long dash at line 4",
            ),
            (
                passing_body().replace(
                    "The checkout accepts an empty card field.",
                    "The checkout accepts an \u{201c}empty\u{201d} card field.",
                ),
                body_context(),
                "curly quote at line 3",
            ),
            (
                passing_body().replace(
                    "The checkout accepts an empty card field.",
                    "One defect: the checkout accepts an empty card field.",
                ),
                body_context(),
                "colon at line 3",
            ),
            (long, body_context(), "body has 41 prose lines"),
            (
                third_line("- AC-3 \u{b7} nope \u{b7} browser \u{b7} `npx test` \u{b7} before: 500 \u{b7} after: 422"),
                third_criterion_context(CheckTarget::Drive("nope".to_string())),
                "line 3 names nope which is not a feature or a measurer",
            ),
            (
                third_line("- AC-3 \u{b7} p99 \u{b7} measure \u{b7} `aif measure` \u{b7} 12 \u{2192} 11 ms"),
                third_criterion_context(CheckTarget::Measure("p99".to_string())),
                "line 3 names p99 which is not a feature or a measurer",
            ),
            (
                passing_body().replace("## Blast radius\n", ""),
                body_context(),
                "section Blast radius missing",
            ),
            (
                format!("{}\n## Evidence\nThe run log sits here.\n", passing_body()),
                body_context(),
                "heading Evidence is not allowed",
            ),
            (
                format!("{}{}", passing_body(), fenced_block(31)),
                body_context(),
                "fenced block at body line 12 has 31 lines",
            ),
        ];
        for (body, ctx, expected) in cases {
            let finding = check_body_lines(&body, &ctx).expect_err(expected);
            assert_eq!(finding.reason, expected, "body:\n{body}");
        }
    }

    #[test]
    fn a_measure_line_that_names_a_measurer_of_the_map_passes() {
        let body = third_line(
            "- AC-3 \u{b7} poll_p95 \u{b7} measure \u{b7} `aif measure` \u{b7} 12 \u{2192} 11 ms",
        );
        let ctx = third_criterion_context(CheckTarget::Measure("poll_p95".to_string()));
        check_body_lines(&body, &ctx).expect("the map carries poll_p95");
    }

    /// The coverage rule answers an unknown target first, so the tier rule
    /// never sees one through [`check_body_lines`]. It must refuse one on
    /// its own, or a later caller inherits the hole.
    #[test]
    fn check_tier_refuses_a_line_that_names_no_feature_of_its_own() {
        let lines = vec![BeforeAfterLine {
            id: 1,
            target: "nope".to_string(),
            tier: TierOrMeasure::Tier(Tier::Browser),
            command: "npx test".to_string(),
            before: "500".to_string(),
            after: "422".to_string(),
        }];
        let finding = check_tier(&lines, &body_context()).expect_err("nope is no feature");
        assert_eq!(
            finding.reason,
            "line 1 names nope which is not a feature or a measurer"
        );
    }

    #[test]
    fn a_fenced_block_at_the_transcript_cap_passes() {
        let body = format!("{}{}", passing_body(), fenced_block(MAX_TRANSCRIPT_LINES));
        check_body_lines(&body, &body_context()).expect("thirty lines are the cap, not past it");
    }

    /// A pull request that closes no ticket carries no criteria and no
    /// plan. Trace, the area half of coverage, and scope stand down.
    #[test]
    fn a_body_without_a_ticket_skips_trace_the_area_half_and_scope() {
        let ctx = ContractContext {
            criteria: body_criteria(),
            changed_paths: vec!["src/other.rs".to_string()],
            has_ticket: false,
            ..body_context()
        };
        // Dropping line 2 leaves AC-2 without a line and the api-orders
        // area without one. The changed path sits outside the plan.
        let body = passing_body().replace(
            "- AC-2 \u{b7} orders \u{b7} http \u{b7} `curl -s -X POST :4000/orders` \u{b7} before: 500 \u{b7} after: 422\n",
            "",
        );
        check_body_lines(&body, &ctx).expect("trace, the area half, and scope stand down");

        let ctx = ContractContext {
            has_ticket: false,
            ..body_context()
        };
        let finding = check_body_lines(&passing_body().replace("## Blast radius\n", ""), &ctx)
            .expect_err("the sections rule still answers");
        assert_eq!(finding.reason, "section Blast radius missing");
    }

    /// The target half of coverage and the tier rule read the verification
    /// map, not the ticket. A pull request with no ticket behind it still
    /// answers to both.
    #[test]
    fn a_body_without_a_ticket_still_refuses_an_unknown_target_and_a_low_tier() {
        let ctx = ContractContext {
            criteria: Vec::new(),
            has_ticket: false,
            ..body_context()
        };
        let unknown = passing_body().replace(
            "- AC-1 \u{b7} checkout \u{b7} browser",
            "- AC-1 \u{b7} nope \u{b7} browser",
        );
        let finding = check_body_lines(&unknown, &ctx).expect_err("nope is no feature");
        assert_eq!(
            finding.reason,
            "line 1 names nope which is not a feature or a measurer"
        );

        let low = passing_body().replace(
            "- AC-1 \u{b7} checkout \u{b7} browser",
            "- AC-1 \u{b7} checkout \u{b7} http",
        );
        let finding = check_body_lines(&low, &ctx).expect_err("http is below the browser floor");
        assert_eq!(
            finding.reason,
            "line 1 tier http is below the floor browser"
        );
    }

    /// A transcript may hold a heading and a diff line of its own. Neither
    /// is a section of the body.
    #[test]
    fn a_fenced_transcript_hides_its_headings_from_the_sections_rule() {
        let body = format!(
            "{}{}",
            passing_body(),
            concat!("```\n", "## Foo\n", " ## bar\n", "## Summary\n", "```\n",)
        );
        check_body_lines(&body, &body_context()).expect("a fenced line is not a heading");
    }

    /// A wider fence holds a narrower one as content, so the block the
    /// body opens with four backticks ends at the four-backtick fence.
    #[test]
    fn a_narrow_fence_inside_a_wider_block_does_not_close_it() {
        let inner: String = (0..31).map(|n| format!("out {n}\n")).collect();
        let body = format!("{}````\n```\n{inner}```\n````\n", passing_body());
        let finding =
            check_body_lines(&body, &body_context()).expect_err("the wide block runs past the cap");
        assert_eq!(finding.reason, "fenced block at body line 12 has 33 lines");
    }

    #[test]
    fn a_broken_floor_carries_its_area_and_both_tiers() {
        let body = passing_body().replace(
            "- AC-1 \u{b7} checkout \u{b7} browser",
            "- AC-1 \u{b7} checkout \u{b7} http",
        );
        let finding = check_body_lines(&body, &body_context()).expect_err("the floor breaks");
        assert_eq!(
            finding.floor,
            Some(Floor {
                area: "web-checkout".to_string(),
                floor: Tier::Browser,
                reached: Tier::Http,
            })
        );
    }

    #[test]
    fn a_run_skill_path_outside_a_surface_directory_is_outside_the_plan() {
        let ctx = ContractContext {
            changed_paths: vec![".claude/skills/run-web.md".to_string()],
            ..body_context()
        };
        let finding =
            check_body_lines(&passing_body(), &ctx).expect_err("the file names no surface");
        assert_eq!(
            finding.reason,
            ".claude/skills/run-web.md is outside the plan"
        );
    }

    #[test]
    fn check_body_lines_accepts_a_test_file_a_skill_and_a_named_dependency() {
        let ctx = ContractContext {
            changed_paths: vec![
                "tests/cli.rs".to_string(),
                "src/checkout_test.rs".to_string(),
                ".claude/skills/run-web/SKILL.md".to_string(),
                "web/pay.ts".to_string(),
                "Cargo.toml".to_string(),
            ],
            ticket_names_dependency: true,
            ..body_context()
        };
        check_body_lines(&passing_body(), &ctx).expect("every path is in scope");
    }

    #[test]
    fn owned_paths_reads_the_plan_table() {
        let body = concat!(
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Fast | Wave |\n",
            "|---|---|---|---|---|---|---|\n",
            "| 1 | Block the submit | `src/checkout.rs`, web/** | - | cargo test | npx | 1 |\n",
        );
        assert_eq!(
            owned_paths(body),
            vec!["src/checkout.rs".to_string(), "web/**".to_string()]
        );
        assert!(owned_paths("no plan here").is_empty());
    }

    #[test]
    fn names_dependency_reads_the_word_and_the_manifest() {
        assert!(names_dependency("Add the globset dependency."));
        assert!(names_dependency("Bump the dependencies."));
        assert!(names_dependency("Edit Cargo.toml by hand."));
        assert!(!names_dependency("Block the submit on an empty card."));
    }

    #[test]
    fn lint_prose_counts_the_prose_lines_it_accepts() {
        let body = concat!(
            "# A heading\n",
            "\n",
            "One sentence.\n",
            "```\n",
            "code \u{2014} with a long dash\n",
            "```\n",
            "Another sentence.\n",
        );
        assert_eq!(lint_prose(body).expect("the prose passes"), 2);
    }
}
