use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotItem {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub open: bool,
    pub draft: bool,
    pub labels: Vec<String>,
    pub blocked_by: Vec<u64>,
    pub blockers_open: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Issue,
    PullRequest,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::Issue => "issue",
            ItemKind::PullRequest => "pr",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub items: Vec<SnapshotItem>,
}

impl Snapshot {
    pub fn open_items(&self, kind: ItemKind) -> impl Iterator<Item = &SnapshotItem> {
        self.items
            .iter()
            .filter(move |item| item.kind == kind && item.open)
    }

    pub fn find(&self, number: u64) -> Option<&SnapshotItem> {
        self.items.iter().find(|item| item.number == number)
    }
}

pub fn probe(root: &std::path::Path) -> Result<Snapshot> {
    let issues = gh_items::<IssueJson>(
        root,
        &[
            "issue",
            "list",
            "--json",
            "number,title,url,state,labels,body",
            "--limit",
            "200",
            "--state",
            "all",
        ],
    )
    .context("gh issue list failed")?;
    let prs = gh_items::<PrJson>(
        root,
        &[
            "pr",
            "list",
            "--json",
            "number,title,url,state,isDraft,labels,body",
            "--limit",
            "200",
            "--state",
            "all",
        ],
    )
    .context("gh pr list failed")?;

    let mut items: Vec<SnapshotItem> = Vec::new();
    for issue in issues {
        items.push(SnapshotItem {
            kind: ItemKind::Issue,
            number: issue.number,
            title: issue.title,
            url: issue.url,
            open: issue.state == "OPEN",
            draft: false,
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            blocked_by: parse_blocked_by(&issue.body),
            blockers_open: Vec::new(),
        });
    }
    for pr in prs {
        items.push(SnapshotItem {
            kind: ItemKind::PullRequest,
            number: pr.number,
            title: pr.title,
            url: pr.url,
            open: pr.state == "OPEN",
            draft: pr.is_draft,
            labels: pr.labels.into_iter().map(|l| l.name).collect(),
            blocked_by: parse_blocked_by(&pr.body),
            blockers_open: Vec::new(),
        });
    }

    let mut snapshot = Snapshot { items };
    resolve_blockers(&mut snapshot);
    Ok(snapshot)
}

fn resolve_blockers(snapshot: &mut Snapshot) {
    let open_by_number: BTreeMap<u64, bool> = snapshot
        .items
        .iter()
        .map(|item| (item.number, item.open))
        .collect();
    for item in &mut snapshot.items {
        item.blockers_open = item
            .blocked_by
            .iter()
            .filter(|n| open_by_number.get(n).copied().unwrap_or(false))
            .copied()
            .collect();
    }
}

fn parse_blocked_by(body: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for word in body.split_whitespace() {
        let candidate = word.trim_end_matches(|c: char| !c.is_ascii_digit() && c != '#');
        let digits = candidate
            .strip_prefix('#')
            .or_else(|| candidate.strip_prefix("fw/"));
        if let Some(digits) = digits {
            if let Ok(number) = digits.parse::<u64>() {
                if number > 0 && !out.contains(&number) {
                    out.push(number);
                }
            }
        }
    }
    out
}

fn gh_items<T: serde::de::DeserializeOwned>(
    root: &std::path::Path,
    args: &[&str],
) -> Result<Vec<T>> {
    let out = Command::new("gh")
        .current_dir(root)
        .args(args)
        .output()
        .context("failed to run gh")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        anyhow::bail!("{err}");
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[derive(Debug, Deserialize)]
struct IssueJson {
    number: u64,
    title: String,
    url: String,
    state: String,
    labels: Vec<LabelJson>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct PrJson {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    labels: Vec<LabelJson>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct LabelJson {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_by_extraction() {
        let body = "Implement X.\n\nBlocked by #12 and blocked-by #30. See also fw/7 and #12 again. Not a ref: #abc";
        let blocked = parse_blocked_by(body);
        assert_eq!(blocked, vec![12, 30, 7]);
    }

    #[test]
    fn blocker_resolution_marks_open_ones() {
        let mut snapshot = Snapshot {
            items: vec![
                SnapshotItem {
                    kind: ItemKind::Issue,
                    number: 1,
                    title: "one".into(),
                    url: String::new(),
                    open: true,
                    draft: false,
                    labels: vec![],
                    blocked_by: vec![2, 3],
                    blockers_open: vec![],
                },
                SnapshotItem {
                    kind: ItemKind::Issue,
                    number: 2,
                    title: "two".into(),
                    url: String::new(),
                    open: false,
                    draft: false,
                    labels: vec![],
                    blocked_by: vec![],
                    blockers_open: vec![],
                },
                SnapshotItem {
                    kind: ItemKind::Issue,
                    number: 3,
                    title: "three".into(),
                    url: String::new(),
                    open: true,
                    draft: false,
                    labels: vec![],
                    blocked_by: vec![],
                    blockers_open: vec![],
                },
            ],
        };
        resolve_blockers(&mut snapshot);
        assert_eq!(snapshot.items[0].blockers_open, vec![3]);
    }
}
