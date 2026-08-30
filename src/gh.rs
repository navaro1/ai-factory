//! Wraps the `gh` CLI, caches ETags, and maps JSON into the model types.
//!
//! Every call goes through the [`Exec`] indirection, so a test scripts every
//! `gh` answer and never touches the network.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::exec::Exec;
use crate::model::{Issue, Pr};

/// The page size every list request asks for.
const PAGE_SIZE: usize = 100;

/// The server refused the call and named a wait in its `Retry-After` head.
///
/// `GhClient` returns this error instead of sleeping; the caller decides when
/// to retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimited {
    /// The wait the server asked for, in whole seconds.
    pub seconds: u64,
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GitHub rate limit hit; retry after {} seconds",
            self.seconds
        )
    }
}

impl std::error::Error for RateLimited {}

/// One fetched list: the items of every page that changed, plus the pages
/// that answered 304.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection<T> {
    /// The items of the pages that returned 200, keyed by number.
    pub changed: BTreeMap<u64, T>,
    /// The numbers of the pages that answered 304, in fetch order.
    pub unchanged: Vec<u64>,
}

/// Which GitHub list a page belongs to; half of the ETag cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListKind {
    /// The issues endpoint. It also returns pull requests; the reader drops
    /// those.
    Issues,
    /// The pull requests endpoint.
    Pulls,
}

impl ListKind {
    /// The path segment of the REST endpoint.
    fn path(self) -> &'static str {
        match self {
            ListKind::Issues => "issues",
            ListKind::Pulls => "pulls",
        }
    }
}

/// What the reader remembers about one page.
#[derive(Debug, Clone)]
struct CachedPage {
    /// The ETag of the last 200 answer; the next request sends it as
    /// `If-None-Match`.
    etag: String,
    /// The page held exactly [`PAGE_SIZE`] items at its last 200.
    full: bool,
    /// The `Link` head named a `rel="next"` page at its last 200.
    next: bool,
}

/// A GitHub reader for one poller thread.
///
/// The client runs `gh api` through the [`Exec`] indirection and remembers,
/// per list and page, the ETag of the last 200 answer and the two pagination
/// flags learned from it. The next request for that page sends
/// `If-None-Match`. A page that answers 304 keeps its cache entry.
pub struct GhClient<'a> {
    exec: &'a dyn Exec,
    pages: BTreeMap<(ListKind, u64), CachedPage>,
}

impl<'a> GhClient<'a> {
    /// A client with an empty page cache.
    pub fn new(exec: &'a dyn Exec) -> Self {
        GhClient {
            exec,
            pages: BTreeMap::new(),
        }
    }

    /// Fetch the open issues of `owner_repo` and follow pagination.
    ///
    /// Objects that carry a `pull_request` key are pull requests; the method
    /// drops them. A 304 page is reported in `unchanged`, and the walk goes
    /// on while the cached flags of that page know a next page.
    pub fn fetch_issues(&mut self, owner_repo: &str) -> Result<Collection<Issue>> {
        self.fetch_list(owner_repo, ListKind::Issues, issue_from_value)
    }

    /// Fetch the open pull requests of `owner_repo` and follow pagination.
    pub fn fetch_pulls(&mut self, owner_repo: &str) -> Result<Collection<Pr>> {
        self.fetch_list(owner_repo, ListKind::Pulls, |value| {
            Ok(Some(pr_from_value(value)?))
        })
    }

    /// The stored ETag of one issues page.
    pub fn issue_etag(&self, page: u64) -> Option<&str> {
        self.pages
            .get(&(ListKind::Issues, page))
            .map(|cached| cached.etag.as_str())
    }

    /// The stored ETag of one pull requests page.
    pub fn pull_etag(&self, page: u64) -> Option<&str> {
        self.pages
            .get(&(ListKind::Pulls, page))
            .map(|cached| cached.etag.as_str())
    }

    /// Fetch every page of one list and merge the changed pages.
    ///
    /// The walk goes to the next page while the current page is known to
    /// carry one: a 200 with exactly [`PAGE_SIZE`] items and a `Link` head
    /// that names `rel="next"`, or a 304 whose cached flags say both. A 304
    /// without a cache entry stops the walk instead of looping.
    fn fetch_list<T>(
        &mut self,
        owner_repo: &str,
        kind: ListKind,
        map_item: impl Fn(&Value) -> Result<Option<T>>,
    ) -> Result<Collection<T>> {
        let mut fetched = Collection {
            changed: BTreeMap::new(),
            unchanged: Vec::new(),
        };
        let mut page: u64 = 1;
        loop {
            let path = kind.path();
            let url =
                format!("repos/{owner_repo}/{path}?state=open&per_page={PAGE_SIZE}&page={page}");
            let cached = self.pages.get(&(kind, page)).cloned();
            let mut args: Vec<&str> = vec!["api", "-i"];
            let header;
            if let Some(cached_page) = &cached {
                header = format!("If-None-Match: {}", cached_page.etag);
                args.push("-H");
                args.push(&header);
            }
            args.extend(["-X", "GET", url.as_str()]);
            let out = self
                .exec
                .run("gh", &args, None)
                .context("gh api failed to run")?;
            let response = parse_response(&out.stdout)?;
            ensure_ok(&response, &out.stderr)?;
            if response.status == 304 {
                // A 304 says only that this page is byte-identical. Pages
                // after it can still carry changes, so the walk continues
                // while the cached flags know a next page.
                fetched.unchanged.push(page);
                if !cached.is_some_and(|entry| entry.full && entry.next) {
                    break;
                }
                page += 1;
                continue;
            }
            let items: Vec<Value> = serde_json::from_str(&response.body)
                .context("gh api returned a body that is not a JSON array")?;
            let full = items.len() == PAGE_SIZE;
            let next = response.link_next;
            match &response.etag {
                Some(etag) => {
                    self.pages.insert(
                        (kind, page),
                        CachedPage {
                            etag: etag.clone(),
                            full,
                            next,
                        },
                    );
                }
                None => {
                    self.pages.remove(&(kind, page));
                }
            }
            for item in &items {
                let number = u64_field(item, "number")?;
                if let Some(mapped) = map_item(item)? {
                    fetched.changed.insert(number, mapped);
                }
            }
            if !full || !next {
                break;
            }
            page += 1;
        }
        Ok(fetched)
    }

    /// Add one label to an issue or a pull request.
    pub fn add_label(&self, owner_repo: &str, number: u64, label: &str) -> Result<()> {
        let url = format!("repos/{owner_repo}/issues/{number}/labels");
        let field = format!("labels[]={label}");
        let args = ["api", "-X", "POST", url.as_str(), "-f", field.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let response = parse_response(&out.stdout)?;
        ensure_ok(&response, &out.stderr)
    }

    /// Remove one label from an issue or a pull request.
    pub fn remove_label(&self, owner_repo: &str, number: u64, label: &str) -> Result<()> {
        let url = format!("repos/{owner_repo}/issues/{number}/labels/{label}");
        let args = ["api", "-X", "DELETE", url.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let response = parse_response(&out.stdout)?;
        ensure_ok(&response, &out.stderr)
    }

    /// Create an issue and return the issue GitHub answered with.
    pub fn create_issue(&self, owner_repo: &str, title: &str, body: &str) -> Result<Issue> {
        let url = format!("repos/{owner_repo}/issues");
        let title_field = format!("title={title}");
        let body_field = format!("body={body}");
        let args = [
            "api",
            "-X",
            "POST",
            url.as_str(),
            "-f",
            title_field.as_str(),
            "-f",
            body_field.as_str(),
        ];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let response = parse_response(&out.stdout)?;
        ensure_ok(&response, &out.stderr)?;
        let value: Value = serde_json::from_str(&response.body)
            .context("gh api returned a broken create_issue body")?;
        issue_from_value(&value)?
            .ok_or_else(|| anyhow!("create_issue returned a pull request object"))
    }
}

/// One parsed `gh api -i` response: the status, the interesting head fields,
/// and the body.
struct Response {
    status: u32,
    etag: Option<String>,
    link_next: bool,
    retry_after: Option<u64>,
    body: String,
}

/// Parse the response head and body out of the raw `gh api -i` output.
fn parse_response(raw: &str) -> Result<Response> {
    let (head, body) =
        split_head(raw).ok_or_else(|| anyhow!("gh api printed no HTTP response head"))?;
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u32 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .parse()
        .map_err(|_| anyhow!("unparsable status line \"{status_line}\""))?;
    let mut parsed = Response {
        status,
        etag: None,
        link_next: false,
        retry_after: None,
        body: body.to_string(),
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "etag" => parsed.etag = Some(value.to_string()),
            "retry-after" => parsed.retry_after = value.parse().ok(),
            "link" => parsed.link_next = value.contains("rel=\"next\""),
            _ => {}
        }
    }
    Ok(parsed)
}

/// Split the raw output at the first blank line into head and body.
///
/// HTTP heads end lines with `\r\n`; this splitter also accepts a bare `\n`.
fn split_head(raw: &str) -> Option<(&str, &str)> {
    let crlf = raw.find("\r\n\r\n");
    let lf = raw.find("\n\n");
    match (crlf, lf) {
        (Some(c), Some(l)) if l < c => Some((&raw[..l], &raw[l + 2..])),
        (Some(c), _) => Some((&raw[..c], &raw[c + 4..])),
        (None, Some(l)) => Some((&raw[..l], &raw[l + 2..])),
        (None, None) => None,
    }
}

/// Turn a non-success response into an error.
///
/// A 403 or 429 with a `Retry-After` head becomes [`RateLimited`].
fn ensure_ok(response: &Response, stderr: &str) -> Result<()> {
    if matches!(response.status, 200..=299 | 304) {
        return Ok(());
    }
    if matches!(response.status, 403 | 429) {
        if let Some(seconds) = response.retry_after {
            return Err(anyhow::Error::new(RateLimited { seconds }));
        }
    }
    let status = response.status;
    let detail = stderr.lines().next().unwrap_or("no stderr");
    bail!("gh api returned HTTP {status}: {detail}")
}

/// Map one GitHub object to an [`Issue`]; `None` for a pull request object.
fn issue_from_value(value: &Value) -> Result<Option<Issue>> {
    if value.get("pull_request").is_some() {
        return Ok(None);
    }
    Ok(Some(Issue {
        number: u64_field(value, "number")?,
        node_id: str_field(value, "node_id")?.to_string(),
        title: str_field(value, "title")?.to_string(),
        body: optional_str(value, "body").to_string(),
        labels: label_names(value)?,
        open: state_is_open(value)?,
    }))
}

/// Map one GitHub object to a [`Pr`].
fn pr_from_value(value: &Value) -> Result<Pr> {
    Ok(Pr {
        number: u64_field(value, "number")?,
        node_id: str_field(value, "node_id")?.to_string(),
        title: str_field(value, "title")?.to_string(),
        body: optional_str(value, "body").to_string(),
        labels: label_names(value)?,
        open: state_is_open(value)?,
        draft: value.get("draft").and_then(Value::as_bool).unwrap_or(false),
        head_sha: value
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// The string value of `key`, or an error when the key is missing.
fn str_field<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub object has no string field \"{key}\""))
}

/// The u64 value of `key`, or an error when the key is missing.
fn u64_field(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("GitHub object has no number field \"{key}\""))
}

/// The string value of `key`; empty when the key is absent or null.
fn optional_str<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The label names of one GitHub object.
fn label_names(value: &Value) -> Result<Vec<String>> {
    let Some(labels) = value.get("labels").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    labels
        .iter()
        .map(|label| Ok(str_field(label, "name")?.to_string()))
        .collect()
}

/// Whether the GitHub object is in the open state.
fn state_is_open(value: &Value) -> Result<bool> {
    Ok(str_field(value, "state")? == "open")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Call, CmdOut, ScriptExec};

    /// A matcher for one exact `gh` argument vector.
    fn gh(argv: &[&str]) -> impl Fn(&Call) -> bool + Send + Sync {
        let expected: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        move |call| call.program == "gh" && call.args == expected
    }

    /// One recorded `gh api -i` response text.
    fn response(status_line: &str, headers: &[&str], body: &str) -> String {
        let mut text = format!("{status_line}\r\n");
        for header in headers {
            text.push_str(header);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        text.push_str(body);
        text
    }

    /// One GitHub issue object with the given number.
    fn issue_json(number: u64) -> String {
        format!(
            r#"{{"number":{number},"node_id":"node-{number}","title":"issue {number}","body":"body {number}","state":"open","labels":[]}}"#
        )
    }

    /// A JSON array of issue objects numbered `first` to `last`.
    fn issues_json(first: u64, last: u64) -> String {
        let items: Vec<String> = (first..=last).map(issue_json).collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn fetch_issues_runs_the_exact_gh_call_and_maps_the_items() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response(
                "HTTP/2 200",
                &["etag: \"e1\""],
                &issues_json(1, 2),
            )),
        );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert!(fetched.unchanged.is_empty());
        assert_eq!(fetched.changed.len(), 2);
        let one = &fetched.changed[&1];
        assert_eq!(one.title, "issue 1");
        assert_eq!(one.node_id, "node-1");
        assert_eq!(one.body, "body 1");
        assert!(one.open);
        assert_eq!(client.issue_etag(1), Some("\"e1\""));
        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "gh");
        assert_eq!(
            calls[0].argv(),
            [
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1"
            ]
        );
    }

    #[test]
    fn a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page() {
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"v1\""],
                    &issues_json(1, 1),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"v1\"",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &["etag: \"v1\""], "")),
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();
        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(second.unchanged, vec![1]);
        assert!(second.changed.is_empty());
        assert_eq!(client.issue_etag(1), Some("\"v1\""));
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_304_body_is_never_parsed() {
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"v1\""],
                    &issues_json(1, 1),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"v1\"",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok("HTTP/2 304\r\n\r\nTHIS IS NOT JSON {"),
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();
        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert!(second.changed.is_empty());
        assert_eq!(second.unchanged, vec![1]);
    }

    #[test]
    fn a_304_page_with_a_cached_next_page_does_not_end_the_walk() {
        let next_link = "link: <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"next\", <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"last\"";
        let edited_page = r#"[{"number":101,"node_id":"node-101","title":"issue 101","body":"body 101","state":"open","labels":[{"name":"refined"}]}]"#;
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p1\"", next_link],
                    &issues_json(1, 100),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=2",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p2\""],
                    &issues_json(101, 103),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"p1\"",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &["etag: \"p1\""], "")),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"p2\"",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=2",
                ]),
                CmdOut::ok(response("HTTP/2 200", &["etag: \"p2b\""], edited_page)),
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();
        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(second.unchanged, vec![1]);
        assert_eq!(second.changed.len(), 1);
        assert_eq!(second.changed[&101].labels, vec!["refined".to_string()]);
        assert_eq!(client.issue_etag(1), Some("\"p1\""));
        assert_eq!(client.issue_etag(2), Some("\"p2b\""));
        assert_eq!(exec.calls().len(), 4);
    }

    #[test]
    fn a_304_page_known_to_be_short_ends_the_walk() {
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"s1\""],
                    &issues_json(1, 1),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"s1\"",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &["etag: \"s1\""], "")),
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();
        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(second.unchanged, vec![1]);
        assert!(second.changed.is_empty());
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn an_unknown_page_that_answers_304_ends_the_walk() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok("HTTP/2 304\r\n\r\n"),
        );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.unchanged, vec![1]);
        assert!(fetched.changed.is_empty());
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn pagination_merges_two_pages_into_one_map() {
        let next_link = "link: <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"next\", <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"last\"";
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p1\"", next_link],
                    &issues_json(1, 100),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues?state=open&per_page=100&page=2",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p2\""],
                    &issues_json(101, 103),
                )),
            );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.changed.len(), 103);
        assert!(fetched.changed.contains_key(&1));
        assert!(fetched.changed.contains_key(&103));
        assert!(fetched.unchanged.is_empty());
        assert_eq!(client.issue_etag(1), Some("\"p1\""));
        assert_eq!(client.issue_etag(2), Some("\"p2\""));
        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1]
            .argv()
            .contains(&"repos/acme/borsuk/issues?state=open&per_page=100&page=2"));
    }

    #[test]
    fn a_full_page_without_a_next_link_ends_pagination() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response(
                "HTTP/2 200",
                &["etag: \"p1\""],
                &issues_json(1, 100),
            )),
        );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.changed.len(), 100);
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn an_issue_with_a_pull_request_key_never_appears_in_issues() {
        let mixed = format!(
            "[{},{{\"number\":9,\"node_id\":\"pr-9\",\"title\":\"pr 9\",\"body\":\"\",\
             \"state\":\"open\",\"labels\":[],\"pull_request\":{{\"url\":\"x\"}}}}]",
            issue_json(8)
        );
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &["etag: \"p1\""], &mixed)),
        );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.changed.len(), 1);
        assert!(fetched.changed.contains_key(&8));
    }

    #[test]
    fn a_403_with_retry_after_names_the_wait_in_seconds() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 403", &["retry-after: 60"], "{}")),
        );
        let mut client = GhClient::new(&exec);
        let err = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(err.to_string().contains("60"), "error was: {err}");
        let limited = err.downcast_ref::<RateLimited>().unwrap();
        assert_eq!(limited.seconds, 60);
    }

    #[test]
    fn a_429_with_retry_after_names_the_wait_in_seconds() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/pulls?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 429", &["retry-after: 7"], "{}")),
        );
        let mut client = GhClient::new(&exec);
        let err = client.fetch_pulls("acme/borsuk").unwrap_err();

        let limited = err.downcast_ref::<RateLimited>().unwrap();
        assert_eq!(limited.seconds, 7);
    }

    #[test]
    fn an_http_500_is_an_error_naming_the_status() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut {
                status: 1,
                stdout: response("HTTP/2 500", &[], ""),
                stderr: "boom\n".into(),
            },
        );
        let mut client = GhClient::new(&exec);
        let err = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(err.to_string().contains("500"), "error was: {err}");
    }

    #[test]
    fn fetch_pulls_maps_draft_and_head_sha() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"open","labels":[{"name":"release-stacked"},{"name":"x"}],"draft":true,"head":{"sha":"abc123"}}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/pulls?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response(
                "HTTP/2 200",
                &["etag: \"pr1\""],
                &format!("[{pr_json}]"),
            )),
        );
        let mut client = GhClient::new(&exec);
        let fetched = client.fetch_pulls("acme/borsuk").unwrap();

        let pr = &fetched.changed[&5];
        assert!(pr.draft);
        assert_eq!(pr.head_sha, "abc123");
        assert_eq!(
            pr.labels,
            vec!["release-stacked".to_string(), "x".to_string()]
        );
        assert_eq!(pr.body, "");
        assert!(pr.open);
        assert_eq!(client.pull_etag(1), Some("\"pr1\""));
        assert_eq!(client.issue_etag(1), None);
    }

    #[test]
    fn add_label_posts_to_the_labels_endpoint() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=refined",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], "{}")),
        );
        let client = GhClient::new(&exec);
        client.add_label("acme/borsuk", 7, "refined").unwrap();

        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].argv(),
            [
                "api",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/7/labels",
                "-f",
                "labels[]=refined"
            ]
        );
    }

    #[test]
    fn remove_label_sends_a_delete() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-X",
                "DELETE",
                "repos/acme/borsuk/issues/7/labels/to-refine",
            ]),
            CmdOut::ok(response("HTTP/2 204", &[], "")),
        );
        let client = GhClient::new(&exec);
        client.remove_label("acme/borsuk", 7, "to-refine").unwrap();

        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn create_issue_returns_the_created_issue() {
        let created = r#"{"number":42,"node_id":"IC_42","title":"decision","body":"why","state":"open","labels":[]}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-X",
                "POST",
                "repos/acme/borsuk/issues",
                "-f",
                "title=decision",
                "-f",
                "body=why",
            ]),
            CmdOut::ok(response("HTTP/2 201", &[], created)),
        );
        let client = GhClient::new(&exec);
        let issue = client
            .create_issue("acme/borsuk", "decision", "why")
            .unwrap();

        assert_eq!(issue.number, 42);
        assert_eq!(issue.node_id, "IC_42");
        assert_eq!(issue.title, "decision");
        assert_eq!(issue.body, "why");
    }
}
