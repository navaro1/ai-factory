//! The ticket-PR link table of one repository.
//!
//! GitHub holds the graph. Closing keywords in a PR body tie one PR to
//! several tickets, and several PRs can serve one ticket. The branch rule
//! adds one more pair: a factory PR on `aif/<repo>/issue-<n>` serves ticket
//! `<n>`. Each poll rebuilds the table from the snapshot, so no file
//! persists it and GitHub stays the only source.
//!
//! The keyword list and the same-repository syntax follow
//! `research/2026-09-02-github-closing-keywords.md`.

use std::collections::BTreeSet;

use crate::model::RepoSnapshot;

/// The closing keywords GitHub reads in a pull request body.
const KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// The ticket-PR pairs of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Links {
    /// Every `(ticket, pr)` pair, deduped and ascending.
    pairs: BTreeSet<(u64, u64)>,
}

impl Links {
    /// Derive the links of one repository from its snapshot.
    ///
    /// The branch rule and the body rule union into one table. A body that
    /// parses poorly yields the links that were found; it never fails.
    pub fn derive(repo: &str, snap: &RepoSnapshot) -> Self {
        let mut pairs = BTreeSet::new();
        for pr in snap.prs.values() {
            if let Some(ticket) = issue_number_from_head(repo, &pr.head_ref) {
                pairs.insert((ticket, pr.number));
            }
            for ticket in closing_tickets(&pr.body) {
                pairs.insert((ticket, pr.number));
            }
        }
        Links { pairs }
    }

    /// The pull requests linked to one ticket, ascending.
    pub fn prs_of(&self, ticket: u64) -> Vec<u64> {
        self.pairs
            .iter()
            .filter(|(linked, _)| *linked == ticket)
            .map(|(_, pr)| *pr)
            .collect()
    }

    /// The tickets linked to one pull request, ascending.
    pub fn tickets_of(&self, pr: u64) -> Vec<u64> {
        self.pairs
            .iter()
            .filter(|(_, linked)| *linked == pr)
            .map(|(ticket, _)| *ticket)
            .collect()
    }

    /// Every `(ticket, pr)` pair, ascending.
    pub fn pairs(&self) -> &BTreeSet<(u64, u64)> {
        &self.pairs
    }
}

/// Extract the ticket number from one factory pull request branch.
///
/// A branch `aif/<repo>/issue-<n>` serves ticket `<n>`. Any other branch
/// yields `None`.
pub fn issue_number_from_head(repo: &str, head_ref: &str) -> Option<u64> {
    let prefix = format!("aif/{repo}/issue-");
    head_ref
        .strip_prefix(&prefix)?
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
}

/// The ticket numbers one PR body closes.
///
/// A keyword arms the scan; `#<number>` tokens then collect while they
/// last. Case does not matter and a colon may follow the keyword. A
/// reference with a `/` names another repository, so the scan skips it. A
/// bare `#<number>` without a keyword collects nothing.
pub fn closing_tickets(body: &str) -> BTreeSet<u64> {
    let mut closed = BTreeSet::new();
    let mut armed = false;
    // Commas and semicolons separate references, so they read as spaces.
    let spaced = body.replace([',', ';'], " ");
    for raw in spaced.split_whitespace() {
        let token = raw.trim_matches(|edge: char| edge == '.' || edge == ')');
        let bare = token.strip_suffix(':').unwrap_or(token).to_lowercase();
        if KEYWORDS.contains(&bare.as_str()) {
            armed = true;
            continue;
        }
        let Some(reference) = token.strip_prefix('#') else {
            // A cross-repository reference keeps the scan armed; any other
            // word ends it.
            armed = token.contains('/');
            continue;
        };
        if !armed {
            continue;
        }
        if let Ok(number) = reference.parse::<u64>() {
            if number > 0 {
                closed.insert(number);
            }
        }
    }
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Pr, RepoSnapshot};
    use std::collections::BTreeMap;

    /// One PR with the given branch and body.
    fn pr(number: u64, head_ref: &str, body: &str) -> Pr {
        Pr {
            number,
            node_id: format!("prnode-{number}"),
            title: format!("pr {number}"),
            body: body.to_string(),
            labels: Vec::new(),
            open: true,
            draft: true,
            head_sha: format!("sha-{number}"),
            head_ref: head_ref.to_string(),
        }
    }

    /// One snapshot of the given PRs.
    fn snapshot(prs: Vec<Pr>) -> RepoSnapshot {
        RepoSnapshot {
            issues: BTreeMap::new(),
            prs: prs.into_iter().map(|pr| (pr.number, pr)).collect(),
        }
    }

    /// One open issue, so a snapshot can hold tickets too.
    fn issue(number: u64) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: format!("issue {number}"),
            body: String::new(),
            labels: Vec::new(),
            open: true,
            author: String::new(),
            assignees: Vec::new(),
            updated_at: String::new(),
            github_url: String::new(),
        }
    }

    #[test]
    fn keywords_collect_same_repository_references() {
        assert_eq!(
            closing_tickets("Fixes #10, resolves #22"),
            BTreeSet::from([10, 22])
        );
        assert_eq!(closing_tickets("closes: #7"), BTreeSet::from([7]));
        assert_eq!(closing_tickets("CLOSES #3"), BTreeSet::from([3]));
        assert_eq!(
            closing_tickets("Fixed #1, #2, #3"),
            BTreeSet::from([1, 2, 3])
        );
    }

    #[test]
    fn foreign_references_and_plain_mentions_collect_nothing() {
        assert_eq!(closing_tickets("Fixes octo-org/other#9"), BTreeSet::new());
        assert_eq!(closing_tickets("mentioned #5"), BTreeSet::new());
        assert_eq!(closing_tickets("see #5 and #6"), BTreeSet::new());
    }

    #[test]
    fn unreadable_references_yield_the_links_that_parse() {
        assert_eq!(closing_tickets("Fixed ###, closes #abc"), BTreeSet::new());
        assert_eq!(
            closing_tickets("Closes #12, broken ###, resolves #30."),
            BTreeSet::from([12, 30])
        );
        assert_eq!(closing_tickets(""), BTreeSet::new());
    }

    #[test]
    fn the_branch_rule_links_a_factory_branch_to_its_ticket() {
        let links = Links::derive("borsuk", &snapshot(vec![pr(7, "aif/borsuk/issue-5", "")]));
        assert_eq!(links.tickets_of(7), vec![5]);
        assert_eq!(links.prs_of(5), vec![7]);
    }

    #[test]
    fn the_body_rule_unions_with_the_branch_rule() {
        let links = Links::derive(
            "borsuk",
            &snapshot(vec![pr(7, "aif/borsuk/issue-5", "Closes #9")]),
        );
        assert_eq!(links.tickets_of(7), vec![5, 9]);
        assert_eq!(links.prs_of(5), vec![7]);
        assert_eq!(links.prs_of(9), vec![7]);
    }

    #[test]
    fn one_pair_dedupes_when_both_rules_name_it() {
        let links = Links::derive(
            "borsuk",
            &snapshot(vec![pr(7, "aif/borsuk/issue-5", "Closes #5")]),
        );
        assert_eq!(links.tickets_of(7), vec![5]);
        assert_eq!(links.pairs().len(), 1);
    }

    #[test]
    fn several_prs_can_serve_one_ticket() {
        let links = Links::derive(
            "borsuk",
            &snapshot(vec![
                pr(7, "aif/borsuk/issue-5", ""),
                pr(9, "feature/other", "Fixes #5"),
            ]),
        );
        assert_eq!(links.prs_of(5), vec![7, 9]);
    }

    #[test]
    fn foreign_branches_without_keywords_link_nothing() {
        let links = Links::derive("borsuk", &snapshot(vec![pr(7, "feature/depends", "")]));
        assert!(links.pairs().is_empty());
    }

    #[test]
    fn the_snapshot_issues_take_no_part_in_derivation() {
        let mut snap = snapshot(vec![pr(7, "aif/borsuk/issue-5", "")]);
        snap.issues.insert(5, issue(5));
        let links = Links::derive("borsuk", &snap);
        assert_eq!(links.tickets_of(7), vec![5]);
    }
}
