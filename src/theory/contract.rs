//! The ticket contract grammar and its deterministic checks.
//!
//! A refined ticket is a contract. The section `## Acceptance criteria`
//! holds one line per criterion, and every line names the check that
//! proves it, in the form `- AC-<n> · <statement> · check: <target>`.
//! This module parses that grammar. The checks that run before implement
//! dispatch build on it.

use std::fmt::{Display, Formatter};

use super::skills::Feature;
use super::verify::Measurer;

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
                findings.push(Finding {
                    reason: format!("{reason}: {line}"),
                });
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
            return Err(Finding {
                reason: format!("section {section} missing"),
            });
        }
    }
    let (criteria, findings) = parse_criteria(body);
    if let Some(finding) = findings.into_iter().next() {
        return Err(finding);
    }
    if criteria.is_empty() {
        return Err(Finding {
            reason: "no acceptance criteria".to_string(),
        });
    }
    let new_features = new_features(body);
    for criterion in &criteria {
        if let Some(name) = unresolved_target(&criterion.target, features, &new_features, measurers)
        {
            return Err(Finding {
                reason: format!(
                    "AC-{} check {name} is not a feature or a measurer",
                    criterion.id
                ),
            });
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
    let mut fast_column: Option<usize> = None;
    let mut new_names = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        match fast_column {
            None => {
                if line.starts_with('|') {
                    let cells = table_cells(line);
                    if let Some(column) = cells.iter().position(|cell| *cell == FAST_COLUMN) {
                        fast_column = Some(column);
                    }
                }
            }
            Some(column) => {
                if !line.starts_with('|') {
                    break;
                }
                let cells = table_cells(line);
                if cells.iter().all(|cell| is_separator_cell(cell)) {
                    continue;
                }
                if let Some(name) = cells
                    .get(column)
                    .and_then(|cell| cell.strip_prefix("new: "))
                    .filter(|name| !name.is_empty())
                {
                    new_names.push(name.to_string());
                }
            }
        }
    }
    new_names
}

/// Check the plan table of a ticket body.
///
/// The table is the one whose header row names [`PLAN_OWNED_COLUMN`].
/// Every data row must give its chunk a non-empty owned path. A body
/// without the table misses the plan.
fn check_plan(body: &str) -> Result<(), Finding> {
    let mut owned_column: Option<usize> = None;
    let mut row_number = 0;
    for line in body.lines() {
        let line = line.trim();
        match owned_column {
            None => {
                if line.starts_with('|') {
                    let cells = table_cells(line);
                    if let Some(column) = cells.iter().position(|cell| *cell == PLAN_OWNED_COLUMN) {
                        owned_column = Some(column);
                    }
                }
            }
            Some(column) => {
                if !line.starts_with('|') {
                    break;
                }
                let cells = table_cells(line);
                if cells.iter().all(|cell| is_separator_cell(cell)) {
                    continue;
                }
                row_number += 1;
                if cells.get(column).is_none_or(|cell| cell.is_empty()) {
                    return Err(Finding {
                        reason: format!("plan row {row_number} has an empty owned path"),
                    });
                }
            }
        }
    }
    if owned_column.is_none() {
        return Err(Finding {
            reason: "plan table missing".to_string(),
        });
    }
    Ok(())
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
}
