//! Ticket review actions and their GitHub effects.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;

use crate::config::Config;
use crate::exec::Exec;
use crate::gh::GhClient;
use crate::model::{Issue, Snapshot};
use crate::sock::{
    Push, RepoLabel, TicketAction, TicketConflict, TicketContent, TicketContentSource,
    TicketDetails, TicketLabels, TicketResult, TicketResultKind,
};

/// All daemon effects of one ticket action.
#[derive(Debug, Default)]
pub struct TicketEffects {
    /// Detail, label, and result messages for the UI.
    pub pushes: Vec<Push>,
    /// A GitHub mutation that must replace the issue in the daemon snapshot.
    pub confirmed: Option<(String, Issue, u64)>,
}

/// The one controller for every ticket action.
pub struct TicketController {
    /// The command runner for GitHub API calls.
    exec: Arc<dyn Exec>,
    /// The last confirmed mutation time per repository.
    last_mutation_ms: BTreeMap<String, u64>,
    /// The last valid label catalog per repository.
    label_catalogs: BTreeMap<String, Vec<RepoLabel>>,
}

impl TicketController {
    /// Build the controller over one command runner.
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self {
            exec,
            last_mutation_ms: BTreeMap::new(),
            label_catalogs: BTreeMap::new(),
        }
    }

    /// The last confirmed mutation time of one repository.
    pub fn last_mutation_ms(&self, repo: &str) -> Option<u64> {
        self.last_mutation_ms.get(repo).copied()
    }

    /// Record the wall-clock time after GitHub confirms one mutation.
    pub fn record_confirmed_mutation(&mut self, repo: &str, confirmed_ms: u64) {
        self.last_mutation_ms.insert(repo.to_string(), confirmed_ms);
    }

    /// Apply one action against the last confirmed repository snapshot.
    pub fn handle(
        &mut self,
        action: TicketAction,
        snapshot: &Snapshot,
        config: &Config,
        now_ms: u64,
    ) -> TicketEffects {
        match action {
            TicketAction::Details {
                request,
                repo,
                number,
            } => match snapshot
                .repos
                .get(&repo)
                .and_then(|items| items.issues.get(&number))
                .filter(|issue| issue.open)
                .cloned()
            {
                Some(issue) => TicketEffects {
                    pushes: vec![Push::TicketDetails(TicketDetails {
                        request,
                        repo,
                        issue,
                        proposal: None,
                        chat_error: config.ticket_chat_model().err(),
                    })],
                    confirmed: None,
                },
                None => TicketEffects {
                    pushes: vec![Push::TicketResult(result(
                        request,
                        repo,
                        number,
                        TicketResultKind::Failure,
                        "The ticket is not open in the current GitHub state.",
                    ))],
                    confirmed: None,
                },
            },
            TicketAction::UpdateContent {
                request,
                repo,
                number,
                expected,
                desired,
                source,
            } => self.update_content(
                request, repo, number, expected, desired, source, config, now_ms,
            ),
            TicketAction::Labels { request, repo } => self.load_labels(request, repo, config),
            TicketAction::ToggleLabel {
                request,
                repo,
                number,
                label,
                on,
            } => self.toggle_label(request, repo, number, label, on, snapshot, config, now_ms),
            TicketAction::CreateLabel {
                request,
                repo,
                number,
                name,
                color,
            } => self.create_label(request, repo, number, name, color, snapshot, config, now_ms),
            TicketAction::Chat {
                request,
                repo,
                number,
            } => {
                if let Err(error) = config.ticket_chat_model() {
                    return TicketEffects {
                        pushes: vec![Push::TicketResult(result(
                            request,
                            repo,
                            number,
                            TicketResultKind::Failure,
                            &error,
                        ))],
                        confirmed: None,
                    };
                }
                let issue = snapshot
                    .repos
                    .get(&repo)
                    .and_then(|items| items.issues.get(&number))
                    .filter(|issue| issue.open);
                if issue.is_none() {
                    return TicketEffects {
                        pushes: vec![Push::TicketResult(result(
                            request,
                            repo,
                            number,
                            TicketResultKind::Failure,
                            "The ticket is not open in the current GitHub state.",
                        ))],
                        confirmed: None,
                    };
                }
                if issue.is_some_and(|issue| issue.labels.iter().any(|label| label == "refined")) {
                    return TicketEffects {
                        pushes: vec![Push::TicketResult(result(
                            request,
                            repo,
                            number,
                            TicketResultKind::Failure,
                            "The refined ticket no longer has an active conversation.",
                        ))],
                        confirmed: None,
                    };
                }
                TicketEffects::default()
            }
        }
    }

    /// Update issue content after a fresh conflict check.
    #[allow(clippy::too_many_arguments)]
    fn update_content(
        &mut self,
        request: String,
        repo: String,
        number: u64,
        expected: TicketContent,
        desired: TicketContent,
        source: TicketContentSource,
        config: &Config,
        now_ms: u64,
    ) -> TicketEffects {
        let mut pushes = vec![Push::TicketResult(result(
            request.clone(),
            repo.clone(),
            number,
            TicketResultKind::Pending,
            "GitHub update pending.",
        ))];
        let Some(repo_config) = config.repos.get(&repo) else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The repository is not configured.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let gh = GhClient::new(&*self.exec);
        let current = match gh.fetch_issue(&repo_config.owner_repo, number) {
            Ok(issue) => issue,
            Err(error) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    number,
                    TicketResultKind::Failure,
                    &format!("GitHub could not fetch the current ticket: {error:#}"),
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        if current.title != expected.title || current.body != expected.body {
            pushes.push(Push::TicketResult(TicketResult {
                request,
                repo,
                number,
                kind: TicketResultKind::Conflict,
                message: "GitHub content changed. Compare both versions.".to_string(),
                issue: None,
                conflict: Some(TicketConflict {
                    remote: current,
                    pending: desired,
                    source,
                }),
            }));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        }
        let confirmed = match gh.update_issue(
            &repo_config.owner_repo,
            number,
            &desired.title,
            &desired.body,
        ) {
            Ok(issue) => issue,
            Err(error) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    number,
                    TicketResultKind::Failure,
                    &format!("GitHub rejected the content update: {error:#}"),
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        self.last_mutation_ms.insert(repo.clone(), now_ms);
        pushes.push(Push::TicketResult(TicketResult {
            request,
            repo: repo.clone(),
            number,
            kind: TicketResultKind::Success,
            message: "GitHub confirmed the content update.".to_string(),
            issue: Some(confirmed.clone()),
            conflict: None,
        }));
        TicketEffects {
            pushes,
            confirmed: Some((repo, confirmed, now_ms)),
        }
    }

    /// Load one repository label catalog and retain the prior valid result.
    fn load_labels(&mut self, request: String, repo: String, config: &Config) -> TicketEffects {
        let Some(repo_config) = config.repos.get(&repo) else {
            return TicketEffects {
                pushes: vec![Push::TicketLabels(TicketLabels {
                    request,
                    repo,
                    labels: Vec::new(),
                    error: Some("The repository is not configured.".to_string()),
                })],
                confirmed: None,
            };
        };
        let gh = GhClient::new(&*self.exec);
        match gh.fetch_labels(&repo_config.owner_repo) {
            Ok(labels) => {
                self.label_catalogs.insert(repo.clone(), labels.clone());
                TicketEffects {
                    pushes: vec![Push::TicketLabels(TicketLabels {
                        request,
                        repo,
                        labels,
                        error: None,
                    })],
                    confirmed: None,
                }
            }
            Err(error) => TicketEffects {
                pushes: vec![Push::TicketLabels(TicketLabels {
                    request,
                    labels: self.label_catalogs.get(&repo).cloned().unwrap_or_default(),
                    repo,
                    error: Some(format!(
                        "GitHub could not load repository labels: {error:#}"
                    )),
                })],
                confirmed: None,
            },
        }
    }

    /// Add or remove one existing label as one immediate user action.
    #[allow(clippy::too_many_arguments)]
    fn toggle_label(
        &mut self,
        request: String,
        repo: String,
        number: u64,
        label: String,
        on: bool,
        snapshot: &Snapshot,
        config: &Config,
        now_ms: u64,
    ) -> TicketEffects {
        let mut pushes = vec![Push::TicketResult(result(
            request.clone(),
            repo.clone(),
            number,
            TicketResultKind::Pending,
            "GitHub label update pending.",
        ))];
        let Some(repo_config) = config.repos.get(&repo) else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The repository is not configured.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let Some(mut issue) = snapshot
            .repos
            .get(&repo)
            .and_then(|items| items.issues.get(&number))
            .cloned()
        else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The ticket is not open in the current GitHub state.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let gh = GhClient::new(&*self.exec);
        let labels = if on {
            gh.add_label_names(&repo_config.owner_repo, number, &label)
                .map(Some)
        } else {
            gh.remove_label_names(&repo_config.owner_repo, number, &label)
        };
        let labels = match labels {
            Ok(Some(labels)) => labels,
            Ok(None) => {
                issue.labels.retain(|current| current != &label);
                issue.labels.clone()
            }
            Err(error) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    number,
                    TicketResultKind::Failure,
                    &format!("GitHub rejected the label update: {error:#}"),
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        issue.labels = labels;
        self.last_mutation_ms.insert(repo.clone(), now_ms);
        pushes.push(Push::TicketResult(TicketResult {
            request,
            repo: repo.clone(),
            number,
            kind: TicketResultKind::Success,
            message: "GitHub confirmed the label update.".to_string(),
            issue: Some(issue.clone()),
            conflict: None,
        }));
        TicketEffects {
            pushes,
            confirmed: Some((repo, issue, now_ms)),
        }
    }

    /// Create one repository label and attach it to the issue.
    #[allow(clippy::too_many_arguments)]
    fn create_label(
        &mut self,
        request: String,
        repo: String,
        number: u64,
        name: String,
        color: String,
        snapshot: &Snapshot,
        config: &Config,
        now_ms: u64,
    ) -> TicketEffects {
        let mut pushes = vec![Push::TicketResult(result(
            request.clone(),
            repo.clone(),
            number,
            TicketResultKind::Pending,
            "GitHub label creation pending.",
        ))];
        let name = name.trim().to_string();
        if name.is_empty() {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The label name must not be empty.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        }
        let color = match normalize_label_color(&color) {
            Ok(color) => color,
            Err(message) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    number,
                    TicketResultKind::Failure,
                    &message,
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        let Some(repo_config) = config.repos.get(&repo) else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The repository is not configured.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let Some(mut issue) = snapshot
            .repos
            .get(&repo)
            .and_then(|items| items.issues.get(&number))
            .cloned()
        else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                number,
                TicketResultKind::Failure,
                "The ticket is not open in the current GitHub state.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let gh = GhClient::new(&*self.exec);
        let mut created_new = false;
        let label = match gh.create_label(&repo_config.owner_repo, &name, &color) {
            Ok(label) => {
                created_new = true;
                label
            }
            Err(error) if error.to_string().contains("HTTP 422") => {
                let labels = match gh.fetch_labels(&repo_config.owner_repo) {
                    Ok(labels) => labels,
                    Err(refresh_error) => {
                        pushes.push(Push::TicketResult(result(
                            request,
                            repo,
                            number,
                            TicketResultKind::Failure,
                            &format!(
                                "GitHub reported an existing label, but the catalog refresh failed: {refresh_error:#}"
                            ),
                        )));
                        return TicketEffects {
                            pushes,
                            confirmed: None,
                        };
                    }
                };
                self.label_catalogs.insert(repo.clone(), labels.clone());
                let Some(label) = labels
                    .into_iter()
                    .find(|label| label.name.eq_ignore_ascii_case(&name))
                else {
                    pushes.push(Push::TicketResult(result(
                        request,
                        repo,
                        number,
                        TicketResultKind::Failure,
                        "GitHub rejected label creation, and the refreshed catalog has no matching label.",
                    )));
                    return TicketEffects {
                        pushes,
                        confirmed: None,
                    };
                };
                label
            }
            Err(error) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    number,
                    TicketResultKind::Failure,
                    &format!("GitHub rejected label creation: {error:#}"),
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        let catalog = self.label_catalogs.entry(repo.clone()).or_default();
        if !catalog
            .iter()
            .any(|current| current.name.eq_ignore_ascii_case(&label.name))
        {
            catalog.push(label.clone());
            catalog.sort_by(|left, right| left.name.cmp(&right.name));
        }
        pushes.push(Push::TicketLabels(TicketLabels {
            request: request.clone(),
            repo: repo.clone(),
            labels: catalog.clone(),
            error: None,
        }));
        let labels = match gh.add_label_names(&repo_config.owner_repo, number, &label.name) {
            Ok(labels) => labels,
            Err(error) => {
                let kind = if created_new {
                    TicketResultKind::PartialFailure
                } else {
                    TicketResultKind::Failure
                };
                let message = if created_new {
                    format!("GitHub created the label, but the label is not attached: {error:#}")
                } else {
                    format!("GitHub did not attach the existing label: {error:#}")
                };
                pushes.push(Push::TicketResult(result(
                    request, repo, number, kind, &message,
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        issue.labels = labels;
        self.last_mutation_ms.insert(repo.clone(), now_ms);
        pushes.push(Push::TicketResult(TicketResult {
            request,
            repo: repo.clone(),
            number,
            kind: TicketResultKind::Success,
            message: "GitHub created and attached the label.".to_string(),
            issue: Some(issue.clone()),
            conflict: None,
        }));
        TicketEffects {
            pushes,
            confirmed: Some((repo, issue, now_ms)),
        }
    }
}

/// Normalize one optional-hash six-digit label color.
pub fn normalize_label_color(color: &str) -> Result<String, String> {
    let color = color.trim().strip_prefix('#').unwrap_or(color.trim());
    if color.len() != 6 || !color.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err("The label color must contain exactly six hexadecimal digits.".to_string());
    }
    Ok(color.to_ascii_lowercase())
}

/// Parse the final strict proposal block from one assistant text event.
pub fn parse_ticket_proposal(text: &str) -> Option<TicketContent> {
    const OPEN: &str = "<aif-ticket-proposal-v1>";
    const CLOSE: &str = "</aif-ticket-proposal-v1>";

    let text = text.trim_end();
    if text.contains("```")
        || text.match_indices(OPEN).count() != 1
        || text.match_indices(CLOSE).count() != 1
        || !text.ends_with(CLOSE)
    {
        return None;
    }
    let open = text.find(OPEN)?;
    if open > 0 && text.as_bytes().get(open - 1) != Some(&b'\n') {
        return None;
    }
    let block = &text[open..];
    let json = block
        .strip_prefix(&format!("{OPEN}\n"))?
        .strip_suffix(&format!("\n{CLOSE}"))?;
    if json.contains('\n') {
        return None;
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProposalWire {
        title: String,
        body: String,
    }

    let proposal: ProposalWire = serde_json::from_str(json).ok()?;
    if proposal.title.trim().is_empty() {
        return None;
    }
    Some(TicketContent {
        title: proposal.title,
        body: proposal.body,
    })
}

/// Build one result without issue or conflict data.
fn result(
    request: String,
    repo: String,
    number: u64,
    kind: TicketResultKind,
    message: &str,
) -> TicketResult {
    TicketResult {
        request,
        repo,
        number,
        kind,
        message: message.to_string(),
        issue: None,
        conflict: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::config::Config;
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, RepoSnapshot, Snapshot};
    use crate::sock::{TicketContent, TicketContentSource, TicketResultKind};

    fn config() -> Config {
        let mut text = String::new();
        for stage in crate::model::Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nrunner = \"claude\"\n"
            ));
        }
        text.push_str("[repo.borsuk]\npath = \"/tmp/borsuk\"\n");
        let mut config = Config::parse(&text).unwrap();
        config.repos.get_mut("borsuk").unwrap().owner_repo = "acme/borsuk".to_string();
        config
    }

    fn issue(title: &str, body: &str) -> Issue {
        Issue {
            number: 7,
            node_id: "node-7".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            labels: vec!["ui".to_string()],
            author: "piotr".to_string(),
            assignees: vec!["owner".to_string()],
            updated_at: "2026-08-30T12:00:00Z".to_string(),
            github_url: "https://github.com/acme/borsuk/issues/7".to_string(),
            open: true,
        }
    }

    fn issue_json(title: &str, body: &str) -> String {
        serde_json::json!({
            "number": 7,
            "node_id": "node-7",
            "title": title,
            "body": body,
            "state": "open",
            "labels": [{"name": "ui"}],
            "user": {"login": "piotr"},
            "assignees": [{"login": "owner"}],
            "updated_at": "2026-08-30T12:00:00Z",
            "html_url": "https://github.com/acme/borsuk/issues/7"
        })
        .to_string()
    }

    fn ok(body: &str) -> CmdOut {
        CmdOut::ok(format!("HTTP/2 200\r\n\r\n{body}"))
    }

    fn gh(args: &[&str]) -> impl Fn(&Call) -> bool + Send + Sync {
        let args: Vec<String> = args.iter().map(|value| (*value).to_string()).collect();
        move |call| call.program == "gh" && call.args == args
    }

    fn snapshot(issue: Issue) -> Snapshot {
        Snapshot {
            repos: [(
                "borsuk".to_string(),
                RepoSnapshot {
                    issues: [(7, issue)].into_iter().collect(),
                    prs: BTreeMap::new(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn update_action(expected: TicketContent, desired: TicketContent) -> TicketAction {
        TicketAction::UpdateContent {
            request: "save-7".to_string(),
            repo: "borsuk".to_string(),
            number: 7,
            expected,
            desired,
            source: TicketContentSource::Direct,
        }
    }

    #[test]
    fn a_content_update_fetches_first_and_uses_the_returned_issue() {
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"]),
                    ok(&issue_json("Old title", "Old body")),
                )
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "PATCH",
                        "repos/acme/borsuk/issues/7",
                        "-f",
                        "title=New title",
                        "-f",
                        "body=New body",
                    ]),
                    ok(&issue_json("New title", "New body")),
                ),
        );
        let mut controller = TicketController::new(exec.clone());
        let effects = controller.handle(
            update_action(
                TicketContent {
                    title: "Old title".to_string(),
                    body: "Old body".to_string(),
                },
                TicketContent {
                    title: "New title".to_string(),
                    body: "New body".to_string(),
                },
            ),
            &snapshot(issue("Old title", "Old body")),
            &config(),
            5_000,
        );

        assert_eq!(effects.pushes.len(), 2);
        let Push::TicketResult(pending) = &effects.pushes[0] else {
            panic!("the first push must report pending");
        };
        assert_eq!(pending.kind, TicketResultKind::Pending);
        let Push::TicketResult(success) = &effects.pushes[1] else {
            panic!("the second push must report success");
        };
        assert_eq!(success.kind, TicketResultKind::Success);
        assert_eq!(success.issue.as_ref().unwrap().title, "New title");
        assert_eq!(effects.confirmed.as_ref().unwrap().1.title, "New title");
        assert_eq!(controller.last_mutation_ms("borsuk"), Some(5_000));
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_remote_content_change_returns_the_pending_and_remote_versions() {
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"]),
            ok(&issue_json("Remote title", "Remote body")),
        ));
        let mut controller = TicketController::new(exec.clone());
        let desired = TicketContent {
            title: "Local title".to_string(),
            body: "Local body".to_string(),
        };
        let effects = controller.handle(
            update_action(
                TicketContent {
                    title: "Old title".to_string(),
                    body: "Old body".to_string(),
                },
                desired.clone(),
            ),
            &snapshot(issue("Old title", "Old body")),
            &config(),
            5_000,
        );

        let Push::TicketResult(conflict) = &effects.pushes[1] else {
            panic!("the final push must report a conflict");
        };
        assert_eq!(conflict.kind, TicketResultKind::Conflict);
        let comparison = conflict.conflict.as_ref().unwrap();
        assert_eq!(comparison.remote.title, "Remote title");
        assert_eq!(comparison.pending, desired);
        assert!(effects.confirmed.is_none());
        assert_eq!(exec.calls().len(), 1, "a conflict must not patch GitHub");
    }

    #[test]
    fn github_rejection_and_connection_loss_return_clear_failures() {
        let rejected = Arc::new(ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/7"]),
            CmdOut {
                status: 1,
                stdout: "HTTP/2 422\r\n\r\n{}".to_string(),
                stderr: "validation failed".to_string(),
            },
        ));
        let disconnected = Arc::new(ScriptExec::new());
        for exec in [rejected, disconnected] {
            let mut controller = TicketController::new(exec);
            let effects = controller.handle(
                update_action(
                    TicketContent {
                        title: "Old title".to_string(),
                        body: "Old body".to_string(),
                    },
                    TicketContent {
                        title: "New title".to_string(),
                        body: "New body".to_string(),
                    },
                ),
                &snapshot(issue("Old title", "Old body")),
                &config(),
                5_000,
            );
            let Push::TicketResult(failure) = &effects.pushes[1] else {
                panic!("the final push must report a failure");
            };
            assert_eq!(failure.kind, TicketResultKind::Failure);
            assert!(failure.message.contains("GitHub"), "{}", failure.message);
        }
    }

    #[test]
    fn a_label_catalog_failure_keeps_the_last_valid_catalog() {
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/labels?per_page=100&page=1",
            ]),
            ok(r#"[{"name":"ui","color":"55e6ff"},{"name":"urgent","color":"ff6b7a"}]"#),
        ));
        let mut controller = TicketController::new(exec);
        let first = controller.handle(
            TicketAction::Labels {
                request: "labels-1".to_string(),
                repo: "borsuk".to_string(),
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            1_000,
        );
        let second = controller.handle(
            TicketAction::Labels {
                request: "labels-2".to_string(),
                repo: "borsuk".to_string(),
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            2_000,
        );

        let Push::TicketLabels(first) = &first.pushes[0] else {
            panic!("the request must return a label catalog");
        };
        let Push::TicketLabels(second) = &second.pushes[0] else {
            panic!("the failed request must return the old label catalog");
        };
        assert_eq!(first.labels, second.labels);
        assert_eq!(first.labels[0].name, "ui");
        assert!(first.error.is_none());
        assert!(second.error.as_deref().unwrap().contains("GitHub"));
    }

    #[test]
    fn each_existing_label_toggle_reports_pending_and_confirmed_state() {
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=urgent",
            ]),
            ok(r#"[{"name":"ui"},{"name":"urgent"}]"#),
        ));
        let mut controller = TicketController::new(exec);
        let effects = controller.handle(
            TicketAction::ToggleLabel {
                request: "label-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
                label: "urgent".to_string(),
                on: true,
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            3_000,
        );

        let Push::TicketResult(pending) = &effects.pushes[0] else {
            panic!("the first result must report pending");
        };
        let Push::TicketResult(success) = &effects.pushes[1] else {
            panic!("the final result must report success");
        };
        assert_eq!(pending.kind, TicketResultKind::Pending);
        assert_eq!(success.kind, TicketResultKind::Success);
        assert_eq!(
            success.issue.as_ref().unwrap().labels,
            vec!["ui".to_string(), "urgent".to_string()]
        );
        assert_eq!(effects.confirmed.as_ref().unwrap().2, 3_000);
    }

    #[test]
    fn a_failed_label_toggle_keeps_the_confirmed_issue() {
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=urgent",
            ]),
            CmdOut {
                status: 1,
                stdout: "HTTP/2 500\r\n\r\n{}".to_string(),
                stderr: "server error".to_string(),
            },
        ));
        let mut controller = TicketController::new(exec);
        let effects = controller.handle(
            TicketAction::ToggleLabel {
                request: "label-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
                label: "urgent".to_string(),
                on: true,
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            3_000,
        );

        let Push::TicketResult(failure) = &effects.pushes[1] else {
            panic!("the final result must report failure");
        };
        assert_eq!(failure.kind, TicketResultKind::Failure);
        assert!(failure.issue.is_none());
        assert!(effects.confirmed.is_none());
    }

    #[test]
    fn label_color_validation_accepts_one_optional_hash_and_six_hex_digits() {
        assert_eq!(normalize_label_color("#55E6ff").unwrap(), "55e6ff");
        assert_eq!(normalize_label_color("55e6ff").unwrap(), "55e6ff");
        for invalid in ["#12345", "1234567", "#zzzzzz", "##123456"] {
            assert!(normalize_label_color(invalid).is_err(), "color {invalid}");
        }
    }

    #[test]
    fn new_label_creation_attaches_the_label_and_returns_the_issue() {
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/labels",
                        "-f",
                        "name=triage",
                        "-f",
                        "color=55e6ff",
                    ]),
                    ok(r#"{"name":"triage","color":"55e6ff"}"#),
                )
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/issues/7/labels",
                        "-f",
                        "labels[]=triage",
                    ]),
                    ok(r#"[{"name":"ui"},{"name":"triage"}]"#),
                ),
        );
        let mut controller = TicketController::new(exec);
        let effects = controller.handle(
            TicketAction::CreateLabel {
                request: "new-label-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
                name: "triage".to_string(),
                color: "#55E6ff".to_string(),
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            4_000,
        );

        let Push::TicketResult(success) = effects.pushes.last().unwrap() else {
            panic!("the final push must report success");
        };
        assert_eq!(success.kind, TicketResultKind::Success);
        assert!(success
            .issue
            .as_ref()
            .unwrap()
            .labels
            .contains(&"triage".to_string()));
        assert_eq!(effects.confirmed.as_ref().unwrap().2, 4_000);
    }

    #[test]
    fn attachment_failure_reports_that_label_creation_succeeded() {
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/labels",
                        "-f",
                        "name=triage",
                        "-f",
                        "color=55e6ff",
                    ]),
                    ok(r#"{"name":"triage","color":"55e6ff"}"#),
                )
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/issues/7/labels",
                        "-f",
                        "labels[]=triage",
                    ]),
                    CmdOut {
                        status: 1,
                        stdout: "HTTP/2 500\r\n\r\n{}".to_string(),
                        stderr: "server error".to_string(),
                    },
                ),
        );
        let mut controller = TicketController::new(exec);
        let effects = controller.handle(
            TicketAction::CreateLabel {
                request: "new-label-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
                name: "triage".to_string(),
                color: "55e6ff".to_string(),
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            4_000,
        );

        let Push::TicketResult(partial) = effects.pushes.last().unwrap() else {
            panic!("the final push must report partial failure");
        };
        assert_eq!(partial.kind, TicketResultKind::PartialFailure);
        assert!(partial.message.contains("created"));
        assert!(partial.message.contains("not attached"));
        assert!(effects.confirmed.is_none());
    }

    #[test]
    fn a_concurrent_existing_label_refreshes_the_catalog_before_attachment() {
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/labels",
                        "-f",
                        "name=triage",
                        "-f",
                        "color=55e6ff",
                    ]),
                    CmdOut {
                        status: 1,
                        stdout: "HTTP/2 422\r\n\r\n{}".to_string(),
                        stderr: "already exists".to_string(),
                    },
                )
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/borsuk/labels?per_page=100&page=1",
                    ]),
                    ok(r#"[{"name":"Triage","color":"ff0000"}]"#),
                )
                .expect(
                    gh(&[
                        "api",
                        "-i",
                        "-X",
                        "POST",
                        "repos/acme/borsuk/issues/7/labels",
                        "-f",
                        "labels[]=Triage",
                    ]),
                    ok(r#"[{"name":"ui"},{"name":"triage"}]"#),
                ),
        );
        let mut controller = TicketController::new(exec.clone());
        let effects = controller.handle(
            TicketAction::CreateLabel {
                request: "new-label-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
                name: "triage".to_string(),
                color: "55e6ff".to_string(),
            },
            &snapshot(issue("Old title", "Old body")),
            &config(),
            4_000,
        );

        let Push::TicketResult(success) = effects.pushes.last().unwrap() else {
            panic!("the final push must report success");
        };
        assert_eq!(success.kind, TicketResultKind::Success);
        let catalog = effects.pushes.iter().find_map(|push| match push {
            Push::TicketLabels(catalog) => Some(catalog),
            _ => None,
        });
        assert_eq!(catalog.unwrap().labels[0].color, "ff0000");
        assert_eq!(exec.calls().len(), 3);
    }

    #[test]
    fn an_unlabeled_issue_can_start_one_ticket_chat_request() {
        let config = config();
        let mut issue = issue("Original", "Original body");
        issue.labels.clear();
        let snapshot = snapshot(issue);
        let mut controller = TicketController::new(Arc::new(ScriptExec::new()));

        let effects = controller.handle(
            TicketAction::Chat {
                request: "chat-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
            },
            &snapshot,
            &config,
            50,
        );

        assert!(effects.pushes.is_empty());
    }

    #[test]
    fn an_invalid_chat_configuration_keeps_ticket_details_available() {
        let mut config = config();
        config.ticket_chat.model = None;
        config
            .stages
            .get_mut(&crate::model::Stage::Refine)
            .unwrap()
            .runner = "opencode".to_string();
        let snapshot = snapshot(issue("Original", "Original body"));
        let mut controller = TicketController::new(Arc::new(ScriptExec::new()));

        let effects = controller.handle(
            TicketAction::Details {
                request: "details-7".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
            },
            &snapshot,
            &config,
            50,
        );

        let Push::TicketDetails(details) = &effects.pushes[0] else {
            panic!("ticket details must remain available");
        };
        assert!(details
            .chat_error
            .as_deref()
            .is_some_and(|error| error.contains("ticket_chat.model")));
    }

    #[test]
    fn proposal_parser_accepts_only_one_final_complete_unquoted_block() {
        let valid = concat!(
            "The title can name the failure more clearly.\n\n",
            "<aif-ticket-proposal-v1>\n",
            "{\"title\":\"Clear title\",\"body\":\"Clear body\"}\n",
            "</aif-ticket-proposal-v1>\n",
        );
        assert_eq!(
            parse_ticket_proposal(valid),
            Some(TicketContent {
                title: "Clear title".to_string(),
                body: "Clear body".to_string(),
            })
        );

        for rejected in [
            "proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}\n</aif-ticket-proposal-v1>",
            "<aif-ticket-proposal-v1>\nnot-json\n</aif-ticket-proposal-v1>",
            "> <aif-ticket-proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}\n> </aif-ticket-proposal-v1>",
            "```\n<aif-ticket-proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}\n</aif-ticket-proposal-v1>\n```",
            "<aif-ticket-proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}\n</aif-ticket-proposal-v1>\nmore text",
            "<aif-ticket-proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}\n</aif-ticket-proposal-v1>\n<aif-ticket-proposal-v1>\n{\"title\":\"C\",\"body\":\"D\"}\n</aif-ticket-proposal-v1>",
            "<aif-ticket-proposal-v1>\n{\"title\":\"A\",\"body\":\"B\"}",
        ] {
            assert_eq!(parse_ticket_proposal(rejected), None, "accepted {rejected}");
        }
    }
}
