//! Ticket review actions and their GitHub effects.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;

use crate::config::Config;
use crate::exec::Exec;
use crate::gh::GhClient;
use crate::mentions;
use crate::model::{Issue, Snapshot};
use crate::sock::{
    MentionStatus, Push, RepoLabel, TicketAction, TicketConflict, TicketContent,
    TicketContentSource, TicketDetails, TicketLabels, TicketMentionStatus, TicketMentions,
    TicketResult, TicketResultKind,
};

/// The most mentions one body resolves. The rest stay plain.
const MENTION_CAP: usize = 12;

/// How long one resolved mention status answers repeat requests.
const STATUS_TTL_MS: u64 = 90_000;

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
    /// The resolved mention statuses with their fetch time, keyed by the
    /// lowercased `owner/repo` and the number.
    mention_statuses: BTreeMap<(String, u64), (MentionStatus, u64)>,
    /// The numbers whose last fetch failed, with that time. A failure
    /// makes no further attempt before the status TTL passes.
    mention_failures: BTreeMap<(String, u64), u64>,
    /// The statuses each focus last received, to suppress unchanged
    /// refresh pushes.
    last_mentions: BTreeMap<(String, u64), Vec<TicketMentionStatus>>,
}

impl TicketController {
    /// Build the controller over one command runner.
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self {
            exec,
            last_mutation_ms: BTreeMap::new(),
            label_catalogs: BTreeMap::new(),
            mention_statuses: BTreeMap::new(),
            mention_failures: BTreeMap::new(),
            last_mentions: BTreeMap::new(),
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
            TicketAction::Create {
                request,
                repo,
                title,
                body,
            } => self.create_ticket(request, repo, title, body, config, now_ms),
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
            TicketAction::Mentions { .. } => TicketEffects::default(),
            TicketAction::PrMentions { .. } => TicketEffects::default(),
        }
    }

    /// Resolve the mention statuses of one focused item and return its
    /// push, or `None` when nothing resolved or nothing changed.
    ///
    /// The caller sends the details push before this method runs, so the
    /// first paint never waits on the network. The plan consults the
    /// snapshot first, then the status cache: numbers that a configured
    /// repository holds as open issues or open pull requests, or that the
    /// TTL window still covers, resolve with no network call. Every other
    /// number costs one REST call; an unconfigured repository is fetched
    /// directly under its canonical key. A failed fetch makes no further
    /// attempt before the TTL passes. `force_push` answers even when
    /// nothing changed, which a fresh focus needs after it cleared its
    /// own copy of the statuses.
    #[allow(clippy::too_many_arguments)]
    pub fn mentions_push(
        &mut self,
        snapshot: &Snapshot,
        config: &Config,
        repo: &str,
        number: u64,
        subject_is_pr: bool,
        now_ms: u64,
        force_push: bool,
    ) -> Option<Push> {
        let items = snapshot.repos.get(repo)?;
        let body = if subject_is_pr {
            items.prs.get(&number).map(|pr| pr.body.as_str())
        } else {
            items.issues.get(&number).map(|issue| issue.body.as_str())
        }?;
        let focus_owner = config.repos.get(repo)?.owner_repo.clone();
        let scanned = mentions::scan(body);
        let mut seen = std::collections::BTreeSet::new();
        let planned: Vec<&mentions::Mention> = scanned
            .iter()
            .filter(|mention| seen.insert((mention.repo.clone(), mention.number)))
            .take(MENTION_CAP)
            .collect();
        let mut statuses = Vec::new();
        let gh = GhClient::new(&*self.exec);
        for mention in planned {
            // Resolve the target repository of the mention: the focus
            // repository for a bare number, a configured repository whose
            // owner/repo matches, or the unconfigured key itself.
            let fetch_repo: String = match mention.repo.as_deref() {
                None => focus_owner.clone(),
                Some(key) => config
                    .repos
                    .values()
                    .find(|repo| repo.owner_repo.eq_ignore_ascii_case(key))
                    .map(|repo| repo.owner_repo.clone())
                    .unwrap_or_else(|| key.to_string()),
            };
            let snapshot_repo: Option<&str> = match mention.repo.as_deref() {
                None => Some(repo),
                Some(key) => config
                    .repos
                    .iter()
                    .find(|(_, repo)| repo.owner_repo.eq_ignore_ascii_case(key))
                    .map(|(alias, _)| alias.as_str()),
            };
            let mention_number = mention.number;
            let known = snapshot_repo.and_then(|alias| {
                let items = snapshot.repos.get(alias)?;
                items
                    .issues
                    .get(&mention_number)
                    .map(|_| MentionStatus::OpenIssue)
                    .or_else(|| {
                        items.prs.get(&mention_number).map(|pr| {
                            if pr.draft {
                                MentionStatus::DraftPr
                            } else {
                                MentionStatus::OpenPr
                            }
                        })
                    })
            });
            let cache_key = (fetch_repo.to_ascii_lowercase(), mention_number);
            let cached = self.cached_status(&cache_key, now_ms);
            let status = match known.or(cached) {
                Some(status) => Some(status),
                None => {
                    if self
                        .mention_failures
                        .get(&cache_key)
                        .is_some_and(|failed_at| now_ms - failed_at < STATUS_TTL_MS)
                    {
                        None
                    } else {
                        match gh.fetch_mention_status(&fetch_repo, mention_number) {
                            Ok(Some(fields)) => {
                                match mentions::classify(
                                    &fields.state,
                                    fields.merged,
                                    fields.draft,
                                    fields.is_pr,
                                ) {
                                    Ok(status) => {
                                        self.mention_statuses
                                            .insert(cache_key.clone(), (status, now_ms));
                                        self.mention_failures.remove(&cache_key);
                                        Some(status)
                                    }
                                    Err(_) => None,
                                }
                            }
                            Ok(None) => {
                                let missing = MentionStatus::Missing;
                                self.mention_statuses
                                    .insert(cache_key.clone(), (missing, now_ms));
                                self.mention_failures.remove(&cache_key);
                                Some(missing)
                            }
                            Err(_) => {
                                self.mention_failures.insert(cache_key.clone(), now_ms);
                                None
                            }
                        }
                    }
                }
            };
            if let Some(status) = status {
                statuses.push(TicketMentionStatus {
                    repo: mention.repo.clone(),
                    number: mention_number,
                    status,
                });
            }
        }
        let focus_key = (repo.to_string(), number);
        if !force_push && self.last_mentions.get(&focus_key) == Some(&statuses) {
            return None;
        }
        if statuses.is_empty() {
            return None;
        }
        self.last_mentions.insert(focus_key, statuses.clone());
        Some(Push::TicketMentions(TicketMentions {
            request: uuid::Uuid::new_v4().to_string(),
            repo: repo.to_string(),
            number,
            statuses,
        }))
    }

    /// The cached status of one number while its TTL window lasts.
    fn cached_status(&self, key: &(String, u64), now_ms: u64) -> Option<MentionStatus> {
        self.mention_statuses
            .get(key)
            .filter(|(_, fetched_ms)| now_ms - fetched_ms < STATUS_TTL_MS)
            .map(|(status, _)| *status)
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

    /// Create one ticket from the direct form content.
    fn create_ticket(
        &mut self,
        request: String,
        repo: String,
        title: String,
        body: String,
        config: &Config,
        now_ms: u64,
    ) -> TicketEffects {
        let mut pushes = vec![Push::TicketResult(result(
            request.clone(),
            repo.clone(),
            0,
            TicketResultKind::Pending,
            "GitHub ticket creation pending.",
        ))];
        let title = title.trim().to_string();
        if title.is_empty() {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                0,
                TicketResultKind::Failure,
                "The ticket title must not be empty.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        }
        let Some(repo_config) = config.repos.get(&repo) else {
            pushes.push(Push::TicketResult(result(
                request,
                repo,
                0,
                TicketResultKind::Failure,
                "The repository is not configured.",
            )));
            return TicketEffects {
                pushes,
                confirmed: None,
            };
        };
        let gh = GhClient::new(&*self.exec);
        let issue = match gh.create_issue(&repo_config.owner_repo, &title, &body) {
            Ok(issue) => issue,
            Err(error) => {
                pushes.push(Push::TicketResult(result(
                    request,
                    repo,
                    0,
                    TicketResultKind::Failure,
                    &format!("GitHub rejected the ticket creation: {error:#}"),
                )));
                return TicketEffects {
                    pushes,
                    confirmed: None,
                };
            }
        };
        let number = issue.number;
        self.last_mutation_ms.insert(repo.clone(), now_ms);
        pushes.push(Push::TicketResult(TicketResult {
            request,
            repo: repo.clone(),
            number,
            kind: TicketResultKind::Success,
            message: format!("GitHub created ticket #{number}."),
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
        let mut text = "schema_version = 1\n".to_string();
        for stage in crate::model::Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nharness = \"claude\"\n"
            ));
        }
        text.push_str("[ticket.create]\nmodel = \"model\"\nharness = \"opencode\"\n");
        text.push_str("[ticket.chat]\nmodel = \"model\"\nharness = \"claude\"\n");
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
    fn a_ticket_creation_pushes_pending_then_success_and_confirms() {
        let created = serde_json::json!({
            "number": 12,
            "node_id": "node-12",
            "title": "Direct title",
            "body": "Direct body",
            "state": "open",
            "labels": [],
            "user": {"login": "piotr"},
            "assignees": [],
            "updated_at": "2026-09-03T12:00:00Z",
            "html_url": "https://github.com/acme/borsuk/issues/12"
        })
        .to_string();
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues",
                "-f",
                "title=Direct title",
                "-f",
                "body=Direct body",
            ]),
            ok(&created),
        ));
        let mut controller = TicketController::new(exec.clone());
        let effects = controller.handle(
            TicketAction::Create {
                request: "create-1".to_string(),
                repo: "borsuk".to_string(),
                title: "  Direct title  ".to_string(),
                body: "Direct body".to_string(),
            },
            &Snapshot::default(),
            &config(),
            6_000,
        );

        assert_eq!(effects.pushes.len(), 2);
        let Push::TicketResult(pending) = &effects.pushes[0] else {
            panic!("the first push must report pending");
        };
        assert_eq!(pending.kind, TicketResultKind::Pending);
        assert_eq!(pending.number, 0);
        let Push::TicketResult(success) = &effects.pushes[1] else {
            panic!("the second push must report success");
        };
        assert_eq!(success.kind, TicketResultKind::Success);
        assert_eq!(success.number, 12);
        assert_eq!(success.message, "GitHub created ticket #12.");
        assert_eq!(success.issue.as_ref().unwrap().title, "Direct title");
        assert_eq!(
            effects
                .confirmed
                .as_ref()
                .map(|(repo, issue, _)| (repo.as_str(), issue.number)),
            Some(("borsuk", 12))
        );
        assert_eq!(controller.last_mutation_ms("borsuk"), Some(6_000));
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn an_empty_title_and_an_unknown_repo_fail_without_a_gh_call() {
        let exec = Arc::new(ScriptExec::new());
        let mut controller = TicketController::new(exec.clone());

        let effects = controller.handle(
            TicketAction::Create {
                request: "create-2".to_string(),
                repo: "borsuk".to_string(),
                title: "   ".to_string(),
                body: String::new(),
            },
            &Snapshot::default(),
            &config(),
            6_000,
        );
        assert_eq!(effects.pushes.len(), 2);
        let Push::TicketResult(pending) = &effects.pushes[0] else {
            panic!("the first push must report pending");
        };
        assert_eq!(pending.kind, TicketResultKind::Pending);
        assert_eq!(pending.number, 0);
        let Push::TicketResult(failure) = &effects.pushes[1] else {
            panic!("the second push must report failure");
        };
        assert_eq!(failure.kind, TicketResultKind::Failure);
        assert_eq!(failure.message, "The ticket title must not be empty.");
        assert!(exec.calls().is_empty());

        let effects = controller.handle(
            TicketAction::Create {
                request: "create-3".to_string(),
                repo: "ghost".to_string(),
                title: "Direct title".to_string(),
                body: String::new(),
            },
            &Snapshot::default(),
            &config(),
            6_000,
        );
        let Push::TicketResult(failure) = &effects.pushes[1] else {
            panic!("the second push must report failure");
        };
        assert_eq!(failure.kind, TicketResultKind::Failure);
        assert_eq!(failure.message, "The repository is not configured.");
        assert!(exec.calls().is_empty());
        assert!(effects.confirmed.is_none());
    }

    #[test]
    fn a_github_failure_during_creation_reports_one_failure() {
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues",
                "-f",
                "title=Direct title",
                "-f",
                "body=Direct body",
            ]),
            CmdOut {
                status: 1,
                stdout: "HTTP/2 422\r\n\r\n{}".to_string(),
                stderr: "validation failed".to_string(),
            },
        ));
        let mut controller = TicketController::new(exec);
        let effects = controller.handle(
            TicketAction::Create {
                request: "create-4".to_string(),
                repo: "borsuk".to_string(),
                title: "Direct title".to_string(),
                body: "Direct body".to_string(),
            },
            &Snapshot::default(),
            &config(),
            6_000,
        );

        assert_eq!(effects.pushes.len(), 2);
        let Push::TicketResult(failure) = &effects.pushes[1] else {
            panic!("the final push must report failure");
        };
        assert_eq!(failure.kind, TicketResultKind::Failure);
        assert!(failure.message.contains("GitHub"), "{}", failure.message);
        assert!(failure.issue.is_none());
        assert!(effects.confirmed.is_none());
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
            .is_some_and(|error| error.contains("ticket.chat.model")));
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

    fn with_number(mut issue: Issue, number: u64) -> Issue {
        issue.number = number;
        issue
    }

    fn not_found(number: u64) -> CmdOut {
        CmdOut {
            status: 1,
            stdout: format!("HTTP/2 404\r\n\r\n{{\"number\":{number}}}"),
            stderr: "HTTP 404\n".into(),
        }
    }

    #[test]
    fn details_answers_from_the_snapshot_before_any_gh_call() {
        let exec = Arc::new(ScriptExec::new());
        let mut controller = TicketController::new(exec.clone());

        let effects = controller.handle(
            TicketAction::Details {
                request: "d1".to_string(),
                repo: "borsuk".to_string(),
                number: 7,
            },
            &snapshot(issue("Title", "Depends on #8")),
            &config(),
            1_000,
        );

        assert_eq!(effects.pushes.len(), 1);
        assert!(matches!(effects.pushes[0], Push::TicketDetails(_)));
        assert!(
            exec.calls().is_empty(),
            "the details push must never wait on the network"
        );
    }

    #[test]
    fn mentions_push_plans_snapshot_hits_first_and_fetches_the_rest() {
        let mut base = snapshot(issue("Title", "Depends on #8, tracks #9, misses #10"));
        let items = base.repos.get_mut("borsuk").unwrap();
        items.issues.insert(9, with_number(issue("Title", "b"), 9));
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    ok("{\"number\":8,\"state\":\"closed\"}"),
                )
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/10"]),
                    not_found(10),
                ),
        );
        let mut controller = TicketController::new(exec.clone());

        let push = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true)
            .expect("resolved mentions must push");

        let Push::TicketMentions(mentions) = &push else {
            panic!("the push must carry mention statuses");
        };
        assert_eq!(mentions.repo, "borsuk");
        assert_eq!(mentions.number, 7);
        assert_eq!(
            mentions.statuses,
            vec![
                TicketMentionStatus {
                    repo: None,
                    number: 8,
                    status: MentionStatus::ClosedIssue,
                },
                TicketMentionStatus {
                    repo: None,
                    number: 9,
                    status: MentionStatus::OpenIssue,
                },
                TicketMentionStatus {
                    repo: None,
                    number: 10,
                    status: MentionStatus::Missing,
                },
            ]
        );
        assert_eq!(
            exec.calls().len(),
            2,
            "the snapshot hit must resolve without a fetch"
        );
    }

    #[test]
    fn cross_repo_mentions_resolve_configured_then_fetch_unconfigured() {
        let mut base = snapshot(issue("Title", "Needs acme/borsuk#9 and other/repo#5."));
        let items = base.repos.get_mut("borsuk").unwrap();
        items.issues.insert(9, with_number(issue("Title", "b"), 9));
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/other/repo/issues/5"]),
            ok("{\"number\":5,\"state\":\"open\"}"),
        ));
        let mut controller = TicketController::new(exec.clone());

        let push = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true)
            .expect("both cross-repo mentions must resolve");

        let Push::TicketMentions(mentions) = &push else {
            panic!("the push must carry mention statuses");
        };
        assert_eq!(
            mentions.statuses,
            vec![
                TicketMentionStatus {
                    repo: Some("acme/borsuk".to_string()),
                    number: 9,
                    status: MentionStatus::OpenIssue,
                },
                TicketMentionStatus {
                    repo: Some("other/repo".to_string()),
                    number: 5,
                    status: MentionStatus::OpenIssue,
                },
            ]
        );
        assert_eq!(
            exec.calls().len(),
            1,
            "the configured repository must answer from the snapshot"
        );
    }

    #[test]
    fn repeat_resolution_inside_the_ttl_spends_no_new_gh_calls() {
        let base = snapshot(issue("Title", "See #8."));
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    ok("{\"number\":8,\"state\":\"open\"}"),
                )
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    ok("{\"number\":8,\"state\":\"closed\"}"),
                ),
        );
        let mut controller = TicketController::new(exec.clone());

        let first = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true)
            .unwrap();
        let second = controller.mentions_push(&base, &config(), "borsuk", 7, false, 2_000, true);
        let third = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 200_000, true)
            .unwrap();

        let status = |push: &Push| match push {
            Push::TicketMentions(mentions) => mentions.statuses[0].status,
            _ => panic!("the push must carry mention statuses"),
        };
        assert_eq!(status(&first), MentionStatus::OpenIssue);
        assert_eq!(status(&second.unwrap()), MentionStatus::OpenIssue);
        assert_eq!(
            status(&third),
            MentionStatus::ClosedIssue,
            "past the TTL the answer must refresh"
        );
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn an_unchanged_refresh_pushes_nothing() {
        let base = snapshot(issue("Title", "See #8."));
        let exec = Arc::new(ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
            ok("{\"number\":8,\"state\":\"open\"}"),
        ));
        let mut controller = TicketController::new(exec.clone());

        let first = controller.mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true);
        let refresh = controller.mentions_push(&base, &config(), "borsuk", 7, false, 2_000, false);

        assert!(first.is_some(), "the focus needs its first statuses");
        assert!(
            refresh.is_none(),
            "an unchanged refresh must not push again"
        );
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn a_rate_limited_fetch_waits_for_the_ttl_window() {
        let base = snapshot(issue("Title", "See #8."));
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    CmdOut {
                        status: 1,
                        stdout: "HTTP/2 403\r\n\r\n{}".to_string(),
                        stderr: "HTTP 403\nretry-after: 30\n".to_string(),
                    },
                )
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    ok("{\"number\":8,\"state\":\"closed\"}"),
                ),
        );
        let mut controller = TicketController::new(exec.clone());

        let limited = controller.mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true);
        let blocked = controller.mentions_push(&base, &config(), "borsuk", 7, false, 2_000, true);
        let retried = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 200_000, true)
            .unwrap();

        assert!(limited.is_none(), "a failed number stays without a push");
        assert!(
            blocked.is_none(),
            "the failed number must not retry inside the TTL window"
        );
        let Push::TicketMentions(mentions) = &retried else {
            panic!("the retry must resolve the status");
        };
        assert_eq!(mentions.statuses[0].status, MentionStatus::ClosedIssue);
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_not_found_answer_caches_the_missing_status() {
        let base = snapshot(issue("Title", "See #8."));
        let exec = Arc::new(
            ScriptExec::new()
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    not_found(8),
                )
                .expect(
                    gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
                    not_found(8),
                ),
        );
        let mut controller = TicketController::new(exec.clone());

        let first = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true)
            .unwrap();
        let refresh = controller.mentions_push(&base, &config(), "borsuk", 7, false, 2_000, false);
        let again = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 200_000, true)
            .unwrap();

        let status = |push: &Push| match push {
            Push::TicketMentions(mentions) => mentions.statuses[0].status,
            _ => panic!("the push must carry mention statuses"),
        };
        assert_eq!(status(&first), MentionStatus::Missing);
        assert!(refresh.is_none(), "the cached answer suppresses the push");
        assert_eq!(status(&again), MentionStatus::Missing);
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_body_with_more_mentions_than_the_cap_resolves_the_first_twelve() {
        // Fourteen unique mentions. The focus number 7 stays out so every
        // planned mention needs one fetch, in document order.
        let numbers: Vec<u64> = (1..=15).filter(|number| *number != 7).collect();
        let body = numbers
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(" ");
        let base = snapshot(issue("Title", &body));
        let mut exec = ScriptExec::new();
        for number in &numbers[..12] {
            exec = exec.expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    &format!("repos/acme/borsuk/issues/{number}"),
                ]),
                not_found(*number),
            );
        }
        let exec = Arc::new(exec);
        let mut controller = TicketController::new(exec.clone());

        let push = controller
            .mentions_push(&base, &config(), "borsuk", 7, false, 1_000, true)
            .expect("the capped mentions must still push");

        let Push::TicketMentions(mentions) = &push else {
            panic!("the push must carry mention statuses");
        };
        assert_eq!(mentions.statuses.len(), 12);
        assert_eq!(mentions.statuses[0].number, 1);
        assert_eq!(mentions.statuses[11].number, 13);
        assert_eq!(exec.calls().len(), 12, "mentions 14 and 15 must stay plain");
    }
}
