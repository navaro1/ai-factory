use std::collections::BTreeMap;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::ids;
use crate::snapshot::{issue_to_state, pull_to_state, IssueJson, ItemState, PullJson};

pub const DEFAULT_POLL_SECS: u64 = 60;
pub const DEFAULT_RECONCILE_SECS: u64 = 600;
pub const STALE_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct Observation {
    pub items: Vec<ItemState>,
    pub forced: bool,
    pub stale: bool,
}

#[derive(Debug)]
pub struct GhOut {
    pub status: u32,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

pub trait GhRunner {
    fn run(&self, args: &[&str]) -> std::io::Result<GhOut>;
}

pub struct RealGh;

impl GhRunner for RealGh {
    fn run(&self, args: &[&str]) -> std::io::Result<GhOut> {
        let out = Command::new("gh").args(args).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        parse_gh_output(out.status.code().unwrap_or(-1), &stdout, &stderr)
    }
}

pub fn parse_gh_output(code: i32, stdout: &str, stderr: &str) -> std::io::Result<GhOut> {
    let (head, body) = match stdout.find("\n\n") {
        Some(idx) => (&stdout[..idx], stdout[idx + 2..].to_owned()),
        None => match stdout.find('\n') {
            Some(idx) if stdout[..idx].starts_with("HTTP/") => {
                (&stdout[..idx], stdout[idx + 1..].to_owned())
            }
            _ => ("", stdout.to_owned()),
        },
    };
    let mut status = 0u32;
    let mut headers = BTreeMap::new();
    for line in head.lines() {
        if line.starts_with("HTTP/") {
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    if status == 0 && !stderr.trim().is_empty() {
        return Err(std::io::Error::other(stderr.trim().to_owned()));
    }
    if status == 0 {
        return Err(std::io::Error::other(format!(
            "gh exited {code} without headers"
        )));
    }
    Ok(GhOut {
        status,
        headers,
        body,
    })
}

#[derive(Clone)]
struct Page {
    etag: Option<String>,
    items_json: String,
}

pub struct PollConfig {
    pub poll: Duration,
    pub reconcile: Duration,
}

impl PollConfig {
    pub fn from_env() -> Self {
        let poll = std::env::var("AIF_GITHUB_POLL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_POLL_SECS);
        let reconcile = std::env::var("AIF_RECONCILE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_RECONCILE_SECS);
        PollConfig {
            poll: Duration::from_secs(poll.max(1)),
            reconcile: Duration::from_secs(reconcile.max(poll.max(1))),
        }
    }
}

pub struct GithubPoller<R: GhRunner> {
    gh: R,
    repo_id: u64,
    #[allow(dead_code)]
    owner_repo: String,
    issues_url: String,
    pulls_url: String,
    pages: BTreeMap<String, Page>,
    last_success: Option<Instant>,
    last_forced: Option<Instant>,
    next_allowed: Option<Instant>,
}

impl<R: GhRunner> GithubPoller<R> {
    pub fn new(gh: R, repo_id: u64, owner_repo: String) -> Self {
        GithubPoller {
            gh,
            repo_id,
            owner_repo: "owner/repo".into(),
            issues_url: format!("repos/{owner_repo}/issues"),
            pulls_url: format!("repos/{owner_repo}/pulls"),
            pages: BTreeMap::new(),
            last_success: None,
            last_forced: None,
            next_allowed: None,
        }
    }

    fn fetch_page(&mut self, url: &str, etag: Option<&str>) -> Result<GhOut> {
        let mut args: Vec<String> = vec![
            "api".into(),
            "-i".into(),
            "-X".into(),
            "GET".into(),
            url.to_owned(),
        ];
        if let Some(etag) = etag {
            args.push("-H".into());
            args.push(format!("If-None-Match: {etag}"));
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        Ok(self.gh.run(&arg_refs)?)
    }

    fn honor_rate_headers(&mut self, out: &GhOut) {
        let retry_after = out
            .headers
            .get("retry-after")
            .and_then(|v| v.parse::<u64>().ok());
        let remaining = out
            .headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.parse::<u64>().ok());
        let reset = out
            .headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.parse::<u64>().ok());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(secs) = retry_after {
            self.next_allowed = Some(Instant::now() + Duration::from_secs(secs));
        } else if remaining == Some(0) {
            if let Some(reset) = reset {
                if reset > now {
                    self.next_allowed = Some(Instant::now() + Duration::from_secs(reset - now));
                }
            }
        }
    }

    fn collect_kind<T: serde::de::DeserializeOwned>(
        &mut self,
        base_url: &str,
        convert: impl Fn(&T) -> Option<ItemState> + Copy,
    ) -> Result<Vec<ItemState>> {
        let mut items = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{base_url}?state=open&sort=created&direction=asc&per_page=100&page={page}"
            );
            let entry = self.pages.get(&url);
            let etag = entry.and_then(|p| p.etag.clone());
            let mut out = self.fetch_page(&url, etag.as_deref())?;
            self.honor_rate_headers(&out);
            match out.status {
                304 => {
                    let cached = self
                        .pages
                        .get(&url)
                        .map(|p| p.items_json.clone())
                        .unwrap_or_default();
                    out.body = cached;
                }
                200 => {
                    let etag = out.headers.get("etag").cloned();
                    self.pages.insert(
                        url.clone(),
                        Page {
                            etag,
                            items_json: out.body.clone(),
                        },
                    );
                }
                code => anyhow::bail!("GET {url} returned {code}"),
            }
            let parsed: Vec<T> = serde_json::from_str(&out.body)
                .map_err(|err| anyhow::anyhow!("page {url} parse failed: {err}"))?;
            let count = parsed.len();
            for raw in &parsed {
                if let Some(item) = convert(raw) {
                    items.push(item);
                }
            }
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(items)
    }

    pub fn poll(&mut self, force: bool) -> Result<Observation> {
        if let Some(allowed) = self.next_allowed {
            if Instant::now() < allowed {
                anyhow::bail!("rate limited; next request allowed later");
            }
            self.next_allowed = None;
        }
        if force {
            self.pages.clear();
        }
        let issues_url = self.issues_url.clone();
        let pulls_url = self.pulls_url.clone();
        let repo_id = self.repo_id;
        let mut items =
            self.collect_kind::<IssueJson>(&issues_url, move |raw| issue_to_state(repo_id, raw))?;
        items.extend(
            self.collect_kind::<PullJson>(&pulls_url, move |raw| {
                Some(pull_to_state(repo_id, raw))
            })?,
        );
        self.last_success = Some(Instant::now());
        if force {
            self.last_forced = Some(Instant::now());
        }
        Ok(Observation {
            items,
            forced: force,
            stale: false,
        })
    }

    pub fn run_loop(
        mut self,
        tx: Sender<Observation>,
        config: PollConfig,
        mut wake: Box<dyn FnMut(Duration) + Send>,
    ) {
        loop {
            let force = self
                .last_forced
                .map(|t| t.elapsed() >= config.reconcile)
                .unwrap_or(true);
            match self.poll(force) {
                Ok(obs) => {
                    if tx.send(obs).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    eprintln!("aif: github poll failed: {err:#}");
                    let stale = self
                        .last_success
                        .map(|t| t.elapsed() >= Duration::from_secs(STALE_SECS))
                        .unwrap_or(true);
                    if stale
                        && tx
                            .send(Observation {
                                items: Vec::new(),
                                forced: false,
                                stale: true,
                            })
                            .is_err()
                    {
                        return;
                    }
                }
            }
            wake(config.poll);
        }
    }
}

pub fn owner_repo_of(root: &std::path::Path) -> Result<(String, u64)> {
    let out = Command::new("gh")
        .current_dir(root)
        .args(["repo", "view", "--json", "nameWithOwner"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "gh repo view failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let name = value["nameWithOwner"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing nameWithOwner"))?
        .to_owned();
    let api = Command::new("gh")
        .current_dir(root)
        .args(["api", &format!("repos/{name}"), "--jq", ".id"])
        .output()?;
    let repo_id = if api.status.success() {
        String::from_utf8_lossy(&api.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
    } else {
        0
    };
    Ok((name, repo_id))
}

pub fn poll_seconds_from_env() -> u64 {
    PollConfig::from_env().poll.as_secs()
}

pub fn record_count_marker() -> usize {
    0
}

pub fn unique_event_id() -> String {
    ids::new_id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::version_in_range;
    use std::cell::RefCell;

    struct ScriptGh {
        responses: RefCell<std::collections::VecDeque<GhOut>>,
        requests: RefCell<Vec<Vec<String>>>,
    }

    impl GhRunner for ScriptGh {
        fn run(&self, args: &[&str]) -> std::io::Result<GhOut> {
            self.requests
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| std::io::Error::other("script empty"))
        }
    }

    fn resp(status: u32, etag: &str, body: &str) -> GhOut {
        let mut headers = BTreeMap::new();
        if !etag.is_empty() {
            headers.insert("etag".to_owned(), etag.to_owned());
        }
        GhOut {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    fn issue_body(number: u64, label: &str) -> String {
        format!(
            "[{{\"id\":1,\"node_id\":\"I_{number}\",\"number\":{number},\"title\":\"t{number}\",\"state\":\"open\",\"labels\":[{{\"name\":\"{label}\"}}],\"body\":\"\"}}]"
        )
    }

    fn poller(gh: ScriptGh) -> GithubPoller<ScriptGh> {
        GithubPoller::new(gh, 42, "owner/repo".into())
    }

    #[test]
    fn parses_headers_and_status() {
        let out = parse_gh_output(
            1,
            "HTTP/2.0 304 Not Modified\nEtag: \"abc\"\nX-RateLimit-Remaining: 4988\n\n",
            "gh: HTTP 304",
        )
        .unwrap();
        assert_eq!(out.status, 304);
        assert_eq!(out.headers.get("etag").map(|s| s.as_str()), Some("\"abc\""));
        assert_eq!(out.body, "");
    }

    #[test]
    fn changed_then_304_emits_items_then_none_change() {
        let gh = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(1, "to-refine")),
                resp(200, "\"p1\"", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller = poller(gh);
        let obs = poller.poll(false).unwrap();
        assert_eq!(obs.items.len(), 1);
        assert_eq!(obs.items[0].number, 1);

        let gh2 = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(304, "", ""),
                resp(304, "", ""),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller2 = GithubPoller {
            pages: poller.pages.clone(),
            ..GithubPoller::new(gh2, 42, "owner/repo".into())
        };
        let obs2 = poller2.poll(false).unwrap();
        assert_eq!(obs2.items.len(), 1, "304 reuses the cached page items");
        let reqs = poller2.gh.requests.borrow();
        let cond_header = reqs
            .iter()
            .flatten()
            .find(|a| a.starts_with("If-None-Match:"))
            .cloned();
        assert_eq!(
            cond_header.as_deref(),
            Some("If-None-Match: \"e1\""),
            "conditional request reuses the stored etag"
        );
    }

    #[test]
    fn later_page_failure_keeps_prior_state() {
        let gh = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(1, "to-refine")),
                resp(200, "\"p1\"", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller = poller(gh);
        let first = poller.poll(false).unwrap();
        assert_eq!(first.items.len(), 1);

        let gh2 = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(1, "to-refine")),
                resp(500, "", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller2 = GithubPoller {
            pages: poller.pages.clone(),
            last_success: poller.last_success,
            ..GithubPoller::new(gh2, 42, "owner/repo".into())
        };
        assert!(poller2.poll(false).is_err());
        assert!(poller2.pages.contains_key(
            &("".to_owned() +
                "repos/owner/repo/issues?state=open&sort=created&direction=asc&per_page=100&page=1")
        ));
    }

    #[test]
    fn force_clears_etags() {
        let gh = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(1, "to-refine")),
                resp(200, "\"p1\"", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller = poller(gh);
        poller.poll(false).unwrap();
        assert_eq!(poller.pages.len(), 2);
        let gh2 = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(2, "to-refine")),
                resp(200, "\"p1\"", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller2 = GithubPoller {
            pages: poller.pages.clone(),
            ..GithubPoller::new(gh2, 42, "owner/repo".into())
        };
        let obs = poller2.poll(true).unwrap();
        assert!(obs.forced);
        assert_eq!(obs.items[0].number, 2);
    }

    #[test]
    fn rate_headers_gate_next_request() {
        let gh = ScriptGh {
            responses: RefCell::new(std::collections::VecDeque::from(vec![
                resp(200, "\"e1\"", &issue_body(1, "to-refine")),
                resp(200, "\"p1\"", "[]"),
            ])),
            requests: RefCell::new(Vec::new()),
        };
        let mut poller = poller(gh);
        poller.poll(false).unwrap();
        poller.next_allowed = Some(Instant::now() + Duration::from_secs(60));
        assert!(poller.poll(false).is_err());
    }

    #[test]
    fn version_helper_shared_with_codex() {
        assert!(version_in_range("1.2.3", ">=1.0.0,<2.0.0"));
        assert!(!version_in_range("2.0.0", ">=1.0.0,<2.0.0"));
    }
}
