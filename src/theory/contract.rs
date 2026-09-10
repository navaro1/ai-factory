//! The ticket contract grammar and its deterministic checks.
//!
//! A refined ticket is a contract. The section `## Acceptance criteria`
//! holds one line per criterion, and every line names the check that
//! proves it, in the form `- AC-<n> · <statement> · check: <target>`.
//! This module parses that grammar. The checks that run before implement
//! dispatch build on it.

use std::fmt::{Display, Formatter};

/// The heading that opens the acceptance criteria of a ticket body.
pub const ACCEPTANCE_HEADING: &str = "## Acceptance criteria";

/// The separator of one contract line. A middle dot with one space on
/// each side.
pub const SEPARATOR: &str = " \u{b7} ";

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
/// [`Finding`] when the line carries no `check:` or names an unknown
/// target form. A body without the heading yields nothing.
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
            Err(reason) => findings.push(Finding {
                reason: format!("{reason}: {line}"),
            }),
        }
    }
    (criteria, findings)
}

/// Parse one criterion line, or name the rule it breaks.
fn parse_criterion(line: &str) -> Result<Criterion, &'static str> {
    let Some((head, check)) = line.rsplit_once(SEPARATOR) else {
        return Err("criterion without a check");
    };
    let Some(check) = check.strip_prefix("check: ") else {
        return Err("criterion without a check");
    };
    let Some((id, statement)) = head.split_once(SEPARATOR) else {
        return Err("criterion without a check");
    };
    let Some(id) = id.strip_prefix("- AC-").and_then(|id| id.parse().ok()) else {
        return Err("criterion without a check");
    };
    let Some(target) = parse_target(check) else {
        return Err("criterion names an unknown check target");
    };
    Ok(Criterion {
        id,
        statement: statement.to_string(),
        target,
    })
}

/// Parse the three target forms of a criterion check.
fn parse_target(check: &str) -> Option<CheckTarget> {
    if let Some(feature) = check.strip_suffix(" drive") {
        return Some(CheckTarget::Drive(feature.to_string()));
    }
    if let Some(feature) = check.strip_suffix(" fast") {
        return Some(CheckTarget::Fast(feature.to_string()));
    }
    check
        .strip_prefix("measure ")
        .map(|id| CheckTarget::Measure(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            findings[0].reason.contains("without a check"),
            "{}",
            findings[0]
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
}
