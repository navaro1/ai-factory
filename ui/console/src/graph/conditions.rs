use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, Copy)]
pub struct Item<'a> {
    pub kind: ItemKind,
    pub number: u64,
    pub labels: &'a [String],
    pub open: bool,
    pub draft: bool,
    pub blocked_by: &'a [u64],
    pub blockers_open: &'a [u64],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    All(Vec<Condition>),
    Not(Box<Condition>),
    IssueLabel(String),
    IssueOpen,
    IssueDependenciesMet,
    PrDraft,
    PrOpen,
}

impl Condition {
    pub fn parse(raw: &str) -> Result<Self> {
        let tokens: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
        let mut parser = CondParser {
            tokens: &tokens,
            pos: 0,
            subject: None,
        };
        let cond = parser.expr()?;
        if parser.pos != tokens.len() {
            bail!("unexpected token {:?}", tokens[parser.pos]);
        }
        Ok(cond)
    }

    pub fn evaluate(&self, item: &Item) -> bool {
        match self {
            Condition::All(parts) => parts.iter().all(|p| p.evaluate(item)),
            Condition::Not(inner) => !inner.evaluate(item),
            Condition::IssueLabel(label) => {
                item.kind == ItemKind::Issue && item.labels.iter().any(|l| l == label)
            }
            Condition::IssueOpen => item.kind == ItemKind::Issue && item.open,
            Condition::IssueDependenciesMet => item.blockers_open.is_empty(),
            Condition::PrDraft => item.kind == ItemKind::PullRequest && item.draft,
            Condition::PrOpen => item.kind == ItemKind::PullRequest && item.open,
        }
    }

    pub fn render(&self) -> String {
        match self {
            Condition::All(parts) => parts
                .iter()
                .map(|p| p.render())
                .collect::<Vec<_>>()
                .join(" and "),
            Condition::Not(inner) => {
                let mut rendered = inner.render();
                for prefix in ["pr is ", "issue is ", "pr ", "issue ", "is "] {
                    if let Some(rest) = rendered.strip_prefix(prefix) {
                        rendered = rest.to_owned();
                        break;
                    }
                }
                format!("not {rendered}")
            }
            Condition::IssueLabel(label) => format!("issue has label '{label}'"),
            Condition::IssueOpen => "issue is open".into(),
            Condition::IssueDependenciesMet => "issue dependencies met".into(),
            Condition::PrDraft => "pr is draft".into(),
            Condition::PrOpen => "pr is open".into(),
        }
    }
}

struct CondParser<'a> {
    tokens: &'a [String],
    pos: usize,
    subject: Option<ItemKind>,
}

impl<'a> CondParser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn next(&mut self) -> Option<&'a str> {
        let token = self.peek();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn expr(&mut self) -> Result<Condition> {
        let mut parts = vec![self.primary()?];
        while self.peek() == Some("and") {
            self.next();
            parts.push(self.primary()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Condition::All(parts)
        })
    }

    fn primary(&mut self) -> Result<Condition> {
        if self.peek() == Some("not") {
            self.next();
            return Ok(Condition::Not(Box::new(self.primary()?)));
        }
        match self.next() {
            Some("issue") => {
                self.subject = Some(ItemKind::Issue);
                self.issue_pred()
            }
            Some("pr") => {
                self.subject = Some(ItemKind::PullRequest);
                self.pr_pred()
            }
            Some(token) if self.subject.is_some() => {
                self.pos -= 1;
                match self.subject {
                    Some(ItemKind::Issue) => self.issue_pred(),
                    _ => self.pr_pred(),
                }
            }
            other => bail!("expected `issue` or `pr`, found {other:?}"),
        }
    }

    fn issue_pred(&mut self) -> Result<Condition> {
        match self.next() {
            Some("has") => match self.next() {
                Some("label") => match self.next() {
                    Some(label) => Ok(Condition::IssueLabel(unquote(label))),
                    other => bail!("expected a label after `has label`, found {other:?}"),
                },
                other => bail!("expected `label` after `has`, found {other:?}"),
            },
            Some("dependencies") => match self.next() {
                Some("met") => Ok(Condition::IssueDependenciesMet),
                other => bail!("expected `met` after `dependencies`, found {other:?}"),
            },
            Some("is") => match self.next() {
                Some("open") => Ok(Condition::IssueOpen),
                other => bail!("unknown issue state {other:?}"),
            },
            Some("open") => Ok(Condition::IssueOpen),
            other => bail!("unknown issue predicate {other:?}"),
        }
    }

    fn pr_pred(&mut self) -> Result<Condition> {
        if self.peek() == Some("is") {
            self.next();
        }
        match self.next() {
            Some("draft") => Ok(Condition::PrDraft),
            Some("open") => Ok(Condition::PrOpen),
            other => bail!("unknown pr state {other:?}"),
        }
    }
}

fn unquote(token: &str) -> String {
    token.trim_matches('\'').trim_matches('"').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_and_combinations_with_elided_subject() {
        let cond = Condition::parse("pr is open and not draft").unwrap();
        assert_eq!(
            cond,
            Condition::All(vec![
                Condition::PrOpen,
                Condition::Not(Box::new(Condition::PrDraft))
            ])
        );
    }

    #[test]
    fn parses_issue_predicates() {
        let cond = Condition::parse("issue has label 'to-refine' and dependencies met").unwrap();
        let owned = labels(&["to-refine"]);
        let none: Vec<u64> = vec![];
        let item = Item {
            kind: ItemKind::Issue,
            number: 1,
            labels: &owned,
            open: true,
            draft: false,
            blocked_by: &none,
            blockers_open: &none,
        };
        assert!(cond.evaluate(&item));

        let open_blockers = vec![3u64];
        let blocked_item = Item {
            blockers_open: &open_blockers,
            ..item
        };
        assert!(!cond.evaluate(&blocked_item));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Condition::parse("ticket is hot").is_err());
        assert!(Condition::parse("issue has").is_err());
        assert!(Condition::parse("pr is open and").is_err());
        assert!(Condition::parse("").is_err());
    }

    #[test]
    fn render_round_trips() {
        let raw = "pr is open and not draft";
        let cond = Condition::parse(raw).unwrap();
        assert_eq!(cond.render(), raw);
    }
}
