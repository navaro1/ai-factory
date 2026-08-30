//! The domain vocabulary every later chunk builds on: stages, issues, pull
//! requests, snapshots, and the changes a poll produces.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One of the four fixed pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Shape the ticket.
    Refine,
    /// Turn a refined issue into a draft pull request.
    Implement,
    /// Review a draft pull request until it is ready.
    Review,
    /// Merge ready pull requests in release trains.
    Release,
}

impl Stage {
    /// Every stage in pipeline order.
    pub const ALL: [Stage; 4] = [
        Stage::Refine,
        Stage::Implement,
        Stage::Review,
        Stage::Release,
    ];

    /// The lowercase config and label name of the stage.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Refine => "refine",
            Stage::Implement => "implement",
            Stage::Review => "review",
            Stage::Release => "release",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Stage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Stage::ALL
            .iter()
            .copied()
            .find(|stage| stage.as_str() == s)
            .ok_or_else(|| format!("unknown stage \"{s}\""))
    }
}

/// Whether a piece of work is an issue or a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// A GitHub issue.
    Issue,
    /// A GitHub pull request.
    Pr,
}

impl ItemKind {
    /// The one-letter form used in task ids: `i` or `p`.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Issue => "i",
            ItemKind::Pr => "p",
        }
    }
}

/// One open GitHub issue, as the poller saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// The issue number.
    pub number: u64,
    /// The GraphQL node id, for stable references.
    pub node_id: String,
    /// The issue title.
    pub title: String,
    /// The issue body, as GitHub returned it.
    pub body: String,
    /// The label names on the issue.
    pub labels: Vec<String>,
    /// Whether the issue is open.
    pub open: bool,
}

/// One open GitHub pull request, as the poller saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pr {
    /// The pull request number.
    pub number: u64,
    /// The GraphQL node id, for stable references.
    pub node_id: String,
    /// The pull request title.
    pub title: String,
    /// The pull request body, as GitHub returned it.
    pub body: String,
    /// The label names on the pull request.
    pub labels: Vec<String>,
    /// Whether the pull request is open.
    pub open: bool,
    /// Whether the pull request is a draft.
    pub draft: bool,
    /// The head commit sha at poll time.
    pub head_sha: String,
}

/// The issues and pull requests of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// The open issues of the repository, keyed by number.
    pub issues: BTreeMap<u64, Issue>,
    /// The open pull requests of the repository, keyed by number.
    pub prs: BTreeMap<u64, Pr>,
}

/// Everything the pollers currently hold, keyed by repository alias.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// One entry per repository that has been polled at least once.
    pub repos: BTreeMap<String, RepoSnapshot>,
}

impl Snapshot {
    /// Replace one repository's entry with `fresh` and report what changed.
    ///
    /// Only the named repository's entry changes. On the first poll for a
    /// repository every item is `Added`. A title or body edit is stored but
    /// produces no change. The method compares label vectors directly.
    pub fn apply(&mut self, repo: &str, fresh: RepoSnapshot) -> Vec<Change> {
        let mut changes = Vec::new();
        match self.repos.get(repo) {
            None => {
                for number in fresh.issues.keys() {
                    changes.push(Change {
                        repo: repo.to_string(),
                        kind: ItemKind::Issue,
                        number: *number,
                        what: ChangeWhat::Added,
                    });
                }
                for number in fresh.prs.keys() {
                    changes.push(Change {
                        repo: repo.to_string(),
                        kind: ItemKind::Pr,
                        number: *number,
                        what: ChangeWhat::Added,
                    });
                }
            }
            Some(old) => {
                for (number, old_item) in &old.issues {
                    match fresh.issues.get(number) {
                        None => changes.push(Change {
                            repo: repo.to_string(),
                            kind: ItemKind::Issue,
                            number: *number,
                            what: ChangeWhat::Removed,
                        }),
                        Some(new_item)
                            if new_item.labels != old_item.labels
                                || new_item.open != old_item.open =>
                        {
                            changes.push(Change {
                                repo: repo.to_string(),
                                kind: ItemKind::Issue,
                                number: *number,
                                what: ChangeWhat::Modified,
                            });
                        }
                        Some(_) => {}
                    }
                }
                for number in fresh.issues.keys() {
                    if !old.issues.contains_key(number) {
                        changes.push(Change {
                            repo: repo.to_string(),
                            kind: ItemKind::Issue,
                            number: *number,
                            what: ChangeWhat::Added,
                        });
                    }
                }
                for (number, old_item) in &old.prs {
                    match fresh.prs.get(number) {
                        None => changes.push(Change {
                            repo: repo.to_string(),
                            kind: ItemKind::Pr,
                            number: *number,
                            what: ChangeWhat::Removed,
                        }),
                        Some(new_item)
                            if new_item.labels != old_item.labels
                                || new_item.open != old_item.open
                                || new_item.draft != old_item.draft
                                || new_item.head_sha != old_item.head_sha =>
                        {
                            changes.push(Change {
                                repo: repo.to_string(),
                                kind: ItemKind::Pr,
                                number: *number,
                                what: ChangeWhat::Modified,
                            });
                        }
                        Some(_) => {}
                    }
                }
                for number in fresh.prs.keys() {
                    if !old.prs.contains_key(number) {
                        changes.push(Change {
                            repo: repo.to_string(),
                            kind: ItemKind::Pr,
                            number: *number,
                            what: ChangeWhat::Added,
                        });
                    }
                }
            }
        }
        self.repos.insert(repo.to_string(), fresh);
        changes
    }
}

/// One observable change to one item of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The repository alias the item belongs to.
    pub repo: String,
    /// Whether the item is an issue or a pull request.
    pub kind: ItemKind,
    /// The item number.
    pub number: u64,
    /// What happened to the item.
    pub what: ChangeWhat,
}

/// The kind of an observable change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeWhat {
    /// The item appeared since the last poll.
    Added,
    /// The item disappeared since the last poll.
    Removed,
    /// The item changed in a way the factory tracks: labels, open, draft, or
    /// head sha.
    Modified,
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
            open: true,
        }
    }

    fn pr(number: u64, draft: bool, head_sha: &str, labels: &[&str]) -> Pr {
        Pr {
            number,
            node_id: format!("prnode-{number}"),
            title: format!("pr {number}"),
            body: String::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            open: true,
            draft,
            head_sha: head_sha.to_string(),
        }
    }

    fn repo(issues: Vec<Issue>, prs: Vec<Pr>) -> RepoSnapshot {
        RepoSnapshot {
            issues: issues.into_iter().map(|i| (i.number, i)).collect(),
            prs: prs.into_iter().map(|p| (p.number, p)).collect(),
        }
    }

    #[test]
    fn stage_names_round_trip_through_str() {
        for stage in Stage::ALL {
            assert_eq!(stage.as_str().parse::<Stage>().unwrap(), stage);
            assert_eq!(stage.to_string(), stage.as_str());
        }
        assert_eq!(
            "refin".parse::<Stage>().unwrap_err(),
            "unknown stage \"refin\""
        );
        assert_eq!(
            serde_json::to_value(Stage::Implement).unwrap(),
            serde_json::json!("implement")
        );
        assert_eq!(
            serde_json::from_str::<Stage>("\"review\"").unwrap(),
            Stage::Review
        );
    }

    #[test]
    fn item_kind_gives_task_id_names() {
        assert_eq!(ItemKind::Issue.as_str(), "i");
        assert_eq!(ItemKind::Pr.as_str(), "p");
    }

    #[test]
    fn first_poll_reports_every_item_as_added() {
        let mut snap = Snapshot::default();
        let changes = snap.apply(
            "borsuk",
            repo(vec![issue(1, &[])], vec![pr(2, true, "aaa", &[])]),
        );
        let mut seen: Vec<(ItemKind, u64)> = changes.iter().map(|c| (c.kind, c.number)).collect();
        seen.sort();
        assert_eq!(seen, vec![(ItemKind::Issue, 1), (ItemKind::Pr, 2)]);
        assert!(changes.iter().all(|c| c.what == ChangeWhat::Added));
    }

    #[test]
    fn apply_never_touches_another_repository() {
        let mut snap = Snapshot::default();
        snap.apply("borsuk", repo(vec![issue(1, &["to-refine"])], vec![]));
        let other = repo(
            vec![issue(7, &["refined"])],
            vec![pr(8, true, "sha-8", &["needs-human"])],
        );
        snap.apply("qubitsok", other.clone());

        // A fresh empty borsuk poll must remove only borsuk entries.
        let changes = snap.apply("borsuk", repo(vec![], vec![]));
        assert_eq!(
            changes,
            vec![Change {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 1,
                what: ChangeWhat::Removed,
            }]
        );
        assert_eq!(snap.repos["qubitsok"], other);
        assert!(snap.repos["borsuk"].issues.is_empty());
    }

    #[test]
    fn later_additions_and_removals_report_the_item_identity() {
        let mut snap = Snapshot::default();
        snap.apply(
            "borsuk",
            repo(vec![issue(1, &[])], vec![pr(2, true, "sha-2", &[])]),
        );

        let changes = snap.apply(
            "borsuk",
            repo(vec![issue(3, &[])], vec![pr(4, false, "sha-4", &[])]),
        );

        assert_eq!(
            changes,
            vec![
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Issue,
                    number: 1,
                    what: ChangeWhat::Removed,
                },
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Issue,
                    number: 3,
                    what: ChangeWhat::Added,
                },
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Pr,
                    number: 2,
                    what: ChangeWhat::Removed,
                },
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Pr,
                    number: 4,
                    what: ChangeWhat::Added,
                },
            ]
        );
    }

    #[test]
    fn text_and_node_edits_are_stored_without_a_change() {
        let mut snap = Snapshot::default();
        snap.apply(
            "borsuk",
            repo(vec![issue(1, &[])], vec![pr(2, true, "sha-2", &[])]),
        );
        let mut fresh = repo(vec![issue(1, &[])], vec![pr(2, true, "sha-2", &[])]);
        let issue = fresh.issues.get_mut(&1).unwrap();
        issue.node_id = "new-issue-node".to_string();
        issue.title = "new issue title".to_string();
        issue.body = "new issue body".to_string();
        let pr = fresh.prs.get_mut(&2).unwrap();
        pr.node_id = "new-pr-node".to_string();
        pr.title = "new pull request title".to_string();
        pr.body = "new pull request body".to_string();

        let changes = snap.apply("borsuk", fresh.clone());

        assert!(changes.is_empty());
        assert_eq!(snap.repos["borsuk"], fresh);
    }

    #[test]
    fn tracked_field_changes_are_reported_and_title_edits_are_not() {
        let mut snap = Snapshot::default();
        snap.apply(
            "borsuk",
            repo(
                vec![issue(1, &["to-refine"]), issue(2, &[])],
                vec![
                    pr(3, true, "aaa", &[]),
                    pr(4, true, "bbb", &[]),
                    pr(5, true, "ccc", &[]),
                ],
            ),
        );

        let mut fresh = repo(
            vec![issue(1, &["to-refine", "refined"]), issue(2, &[])],
            vec![
                pr(3, false, "aaa", &[]),
                pr(4, true, "ddd", &[]),
                pr(5, true, "ccc", &[]),
            ],
        );
        fresh.issues.get_mut(&1).unwrap().labels =
            vec!["refined".to_string(), "to-refine".to_string()];
        fresh.issues.get_mut(&2).unwrap().title = "a new title".to_string();
        fresh.prs.get_mut(&5).unwrap().body = "body edit only".to_string();

        let mut changes = snap.apply("borsuk", fresh);
        changes.sort_by_key(|c| (c.kind, c.number));
        assert_eq!(
            changes,
            vec![
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Issue,
                    number: 1,
                    what: ChangeWhat::Modified,
                },
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Pr,
                    number: 3,
                    what: ChangeWhat::Modified,
                },
                Change {
                    repo: "borsuk".to_string(),
                    kind: ItemKind::Pr,
                    number: 4,
                    what: ChangeWhat::Modified,
                },
            ]
        );
    }

    #[test]
    fn an_open_flip_is_a_change() {
        let mut snap = Snapshot::default();
        snap.apply("borsuk", repo(vec![issue(1, &["a", "b"])], vec![]));

        let mut fresh = repo(vec![issue(1, &["b", "a"])], vec![]);
        fresh.issues.get_mut(&1).unwrap().open = false;
        let changes = snap.apply("borsuk", fresh);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].what, ChangeWhat::Modified);
        assert_eq!(changes[0].number, 1);
    }

    #[test]
    fn a_label_vector_order_change_is_modified() {
        let mut snap = Snapshot::default();
        snap.apply("borsuk", repo(vec![issue(1, &["a", "b"])], vec![]));

        let changes = snap.apply("borsuk", repo(vec![issue(1, &["b", "a"])], vec![]));

        assert_eq!(
            changes,
            vec![Change {
                repo: "borsuk".to_string(),
                kind: ItemKind::Issue,
                number: 1,
                what: ChangeWhat::Modified,
            }]
        );
    }

    #[test]
    fn change_round_trips_through_json() {
        let change = Change {
            repo: "borsuk".to_string(),
            kind: ItemKind::Pr,
            number: 9,
            what: ChangeWhat::Added,
        };
        let text = serde_json::to_string(&change).unwrap();
        assert_eq!(serde_json::from_str::<Change>(&text).unwrap(), change);
        assert!(text.contains("\"what\":\"added\""));
    }
}
