//! Wraps the `gh` CLI, caches ETags, and maps JSON into the model types.
//!
//! Every call goes through the [`Exec`] indirection, so a test scripts every
//! `gh` answer and never touches the network.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::exec::{CmdOut, Exec};
use crate::model::{Issue, Pr};
use crate::sock::RepoLabel;

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

/// One fetched list with all current items and the pages that answered 304.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection<T> {
    /// All current items, keyed by number.
    pub items: BTreeMap<u64, T>,
    /// The numbers of the pages that answered 304, in fetch order.
    pub unchanged: Vec<u64>,
}

/// One comment on an issue or a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueComment {
    /// The login of the comment author; `unknown` after a deleted account.
    pub author: String,
    /// The creation time in RFC 3339 form.
    pub created_at: String,
    /// The raw comment body.
    pub body: String,
}

/// Which GitHub list a page belongs to.
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
#[derive(Debug)]
struct CachedPage<T> {
    /// The ETag of the last 200 answer; the next request sends it as
    /// `If-None-Match`.
    etag: Option<String>,
    /// The page held exactly [`PAGE_SIZE`] items at its last 200.
    full: bool,
    /// The `Link` head named a `rel="next"` page at its last 200.
    next: bool,
    /// The mapped entries from the last successful response for this page.
    items: BTreeMap<u64, T>,
}

/// A GitHub reader for one poller thread.
///
/// The client runs `gh api` through the [`Exec`] indirection and remembers,
/// per repository, list, and page, the ETag and the last mapped entries.
/// The next request for that page sends
/// `If-None-Match`. A page that answers 304 keeps its cache entry.
pub struct GhClient<'a> {
    exec: &'a dyn Exec,
    issue_pages: BTreeMap<(String, u64), CachedPage<Issue>>,
    pull_pages: BTreeMap<(String, u64), CachedPage<Pr>>,
}

impl<'a> GhClient<'a> {
    /// A client with an empty page cache.
    pub fn new(exec: &'a dyn Exec) -> Self {
        GhClient {
            exec,
            issue_pages: BTreeMap::new(),
            pull_pages: BTreeMap::new(),
        }
    }

    /// Fetch the open issues of `owner_repo` and follow pagination.
    ///
    /// Objects that carry a `pull_request` key are pull requests; the method
    /// drops them. A 304 page is reported in `unchanged`, and the walk goes
    /// on while the cached flags of that page know a next page.
    pub fn fetch_issues(&mut self, owner_repo: &str) -> Result<Collection<Issue>> {
        Self::fetch_list(
            self.exec,
            &mut self.issue_pages,
            owner_repo,
            ListKind::Issues,
            issue_from_value,
        )
    }

    /// Fetch the open pull requests of `owner_repo` and follow pagination.
    pub fn fetch_pulls(&mut self, owner_repo: &str) -> Result<Collection<Pr>> {
        Self::fetch_list(
            self.exec,
            &mut self.pull_pages,
            owner_repo,
            ListKind::Pulls,
            |value| Ok(Some(pr_from_value(value)?)),
        )
    }

    /// Fetch one issue and reject a pull request object.
    pub fn fetch_issue(&self, owner_repo: &str, number: u64) -> Result<Issue> {
        let url = format!("repos/{owner_repo}/issues/{number}");
        let args = ["api", "-i", "-X", "GET", url.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        issue_response(&out, "fetch_issue")
    }

    /// Fetch the raw status fields of one mentioned issue or pull request.
    ///
    /// A 404 answer maps to `None`, so the caller can cache the not-found
    /// state. The method reads the raw `state`, `pull_request.merged_at`,
    /// and `draft` fields only and leaves the classification to
    /// [`crate::mentions::classify`].
    pub fn fetch_mention_status(
        &self,
        owner_repo: &str,
        number: u64,
    ) -> Result<Option<MentionFields>> {
        let url = format!("repos/{owner_repo}/issues/{number}");
        let args = ["api", "-i", "-X", "GET", url.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let parsed = parse_response(&out.stdout).with_context(|| {
            let detail = out.stderr.lines().next().unwrap_or("no stderr");
            format!("gh api exited with status {}: {detail}", out.status)
        })?;
        if parsed.status == 404 {
            return Ok(None);
        }
        let response = checked_response(&out)?;
        let value: Value = serde_json::from_str(&response.body)
            .context("gh api returned a broken fetch_mention_status body")?;
        let _ = u64_field(&value, "number")?;
        let state = str_field(&value, "state")?.to_string();
        let merged = value
            .pointer("/pull_request/merged_at")
            .and_then(Value::as_str)
            .is_some();
        let draft = value.get("draft").and_then(Value::as_bool).unwrap_or(false);
        let is_pr = value.get("pull_request").is_some();
        Ok(Some(MentionFields {
            state,
            merged,
            draft,
            is_pr,
        }))
    }

    /// Fetch every repository label in name order.
    pub fn fetch_labels(&self, owner_repo: &str) -> Result<Vec<RepoLabel>> {
        let mut labels = Vec::new();
        let mut page = 1u64;
        loop {
            let url = format!("repos/{owner_repo}/labels?per_page={PAGE_SIZE}&page={page}");
            let args = ["api", "-i", "-X", "GET", url.as_str()];
            let out = self
                .exec
                .run("gh", &args, None)
                .context("gh api failed to run")?;
            let response = checked_response(&out)?;
            let values: Vec<Value> = serde_json::from_str(&response.body)
                .context("gh api returned a broken label catalog body")?;
            for value in &values {
                labels.push(RepoLabel {
                    name: str_field(value, "name")?.to_string(),
                    color: str_field(value, "color")?.to_string(),
                });
            }
            if values.len() < PAGE_SIZE || !response.link_next {
                break;
            }
            page += 1;
        }
        labels.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(labels)
    }

    /// Fetch every comment of one issue or pull request, oldest first.
    ///
    /// The method pages the comments endpoint exactly like
    /// [`GhClient::fetch_labels`] pages the label catalog: the walk ends at
    /// a page below [`PAGE_SIZE`] entries or without a `rel="next"` link.
    pub fn fetch_issue_comments(&self, owner_repo: &str, number: u64) -> Result<Vec<IssueComment>> {
        let mut comments = Vec::new();
        let mut page = 1u64;
        loop {
            let url = format!(
                "repos/{owner_repo}/issues/{number}/comments?per_page={PAGE_SIZE}&page={page}"
            );
            let args = ["api", "-i", "-X", "GET", url.as_str()];
            let out = self
                .exec
                .run("gh", &args, None)
                .context("gh api failed to run")?;
            let response = checked_response(&out)?;
            let values: Vec<Value> = serde_json::from_str(&response.body)
                .context("gh api returned a broken issue comment body")?;
            for value in &values {
                comments.push(IssueComment {
                    author: author_login(value)?.to_string(),
                    created_at: str_field(value, "created_at")?.to_string(),
                    body: str_field(value, "body")?.to_string(),
                });
            }
            if values.len() < PAGE_SIZE || !response.link_next {
                break;
            }
            page += 1;
        }
        Ok(comments)
    }

    /// Create one repository label and return GitHub's label.
    pub fn create_label(&self, owner_repo: &str, name: &str, color: &str) -> Result<RepoLabel> {
        let url = format!("repos/{owner_repo}/labels");
        let name_field = format!("name={name}");
        let color_field = format!("color={color}");
        let args = [
            "api",
            "-i",
            "-X",
            "POST",
            url.as_str(),
            "-f",
            name_field.as_str(),
            "-f",
            color_field.as_str(),
        ];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let response = checked_response(&out)?;
        let value: Value = serde_json::from_str(&response.body)
            .context("gh api returned a broken create label body")?;
        Ok(RepoLabel {
            name: str_field(&value, "name")?.to_string(),
            color: str_field(&value, "color")?.to_string(),
        })
    }

    /// Update one issue title and description and return GitHub's issue.
    pub fn update_issue(
        &self,
        owner_repo: &str,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<Issue> {
        let url = format!("repos/{owner_repo}/issues/{number}");
        let title_field = format!("title={title}");
        let body_field = format!("body={body}");
        let args = [
            "api",
            "-i",
            "-X",
            "PATCH",
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
        issue_response(&out, "update_issue")
    }

    /// Fetch every page of one list and merge its current items.
    ///
    /// The walk goes to the next page while the current page is known to
    /// carry one: a 200 with exactly [`PAGE_SIZE`] items and a `Link` head
    /// that names `rel="next"`, or a 304 whose cached flags say both. The
    /// method rejects a 304 without a cache entry.
    fn fetch_list<T: Clone>(
        exec: &dyn Exec,
        pages: &mut BTreeMap<(String, u64), CachedPage<T>>,
        owner_repo: &str,
        kind: ListKind,
        map_item: impl Fn(&Value) -> Result<Option<T>>,
    ) -> Result<Collection<T>> {
        let mut fetched = Collection {
            items: BTreeMap::new(),
            unchanged: Vec::new(),
        };
        let mut page: u64 = 1;
        loop {
            let path = kind.path();
            let url =
                format!("repos/{owner_repo}/{path}?state=open&per_page={PAGE_SIZE}&page={page}");
            let key = (owner_repo.to_string(), page);
            let mut args: Vec<&str> = vec!["api", "-i"];
            let header;
            if let Some(etag) = pages.get(&key).and_then(|cached| cached.etag.as_deref()) {
                header = format!("If-None-Match: {etag}");
                args.push("-H");
                args.push(&header);
            }
            args.extend(["-X", "GET", url.as_str()]);
            let out = exec
                .run("gh", &args, None)
                .context("gh api failed to run")?;
            let response = checked_response(&out)?;
            if response.status == 304 {
                // A 304 says only that this page is byte-identical. Pages
                // after it can still carry changes, so the walk continues
                // while the cached flags know a next page.
                fetched.unchanged.push(page);
                let cached = pages.get(&key).ok_or_else(|| {
                    anyhow!("gh api returned 304 for page {page} with no cached page")
                })?;
                let has_next = cached.full && cached.next;
                fetched.items.extend(cached.items.clone());
                if !has_next {
                    break;
                }
                page += 1;
                continue;
            }
            let items: Vec<Value> = serde_json::from_str(&response.body)
                .context("gh api returned a body that is not a JSON array")?;
            let full = items.len() == PAGE_SIZE;
            let next = response.link_next;
            let mut page_items = BTreeMap::new();
            for item in &items {
                let number = u64_field(item, "number")?;
                if let Some(mapped) = map_item(item)? {
                    page_items.insert(number, mapped);
                }
            }
            fetched.items.extend(page_items.clone());
            pages.insert(
                key,
                CachedPage {
                    etag: response.etag,
                    full,
                    next,
                    items: page_items,
                },
            );
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
        let args = [
            "api",
            "-i",
            "-X",
            "POST",
            url.as_str(),
            "-f",
            field.as_str(),
        ];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        checked_response(&out).map(|_| ())
    }

    /// Add one label and return the complete confirmed label names.
    pub fn add_label_names(
        &self,
        owner_repo: &str,
        number: u64,
        label: &str,
    ) -> Result<Vec<String>> {
        let url = format!("repos/{owner_repo}/issues/{number}/labels");
        let field = format!("labels[]={label}");
        let args = [
            "api",
            "-i",
            "-X",
            "POST",
            url.as_str(),
            "-f",
            field.as_str(),
        ];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let response = checked_response(&out)?;
        label_array_names(&response.body, "add label")
    }

    /// Remove one label from an issue or a pull request.
    ///
    /// A label that is already absent is a success: GitHub answers 404,
    /// and the wanted state holds. The daemon removes labels that an
    /// agent may have removed a moment earlier, so the race is routine.
    pub fn remove_label(&self, owner_repo: &str, number: u64, label: &str) -> Result<()> {
        let label = encode_path_segment(label);
        let url = format!("repos/{owner_repo}/issues/{number}/labels/{label}");
        let args = ["api", "-i", "-X", "DELETE", url.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let parsed = parse_response(&out.stdout).with_context(|| {
            let detail = out.stderr.lines().next().unwrap_or("no stderr");
            format!("gh api exited with status {}: {detail}", out.status)
        })?;
        if parsed.status == 404 {
            return Ok(());
        }
        checked_response(&out).map(|_| ())
    }

    /// Remove one label and return confirmed names.
    ///
    /// `None` means GitHub reported that the label was already absent.
    pub fn remove_label_names(
        &self,
        owner_repo: &str,
        number: u64,
        label: &str,
    ) -> Result<Option<Vec<String>>> {
        let label = encode_path_segment(label);
        let url = format!("repos/{owner_repo}/issues/{number}/labels/{label}");
        let args = ["api", "-i", "-X", "DELETE", url.as_str()];
        let out = self
            .exec
            .run("gh", &args, None)
            .context("gh api failed to run")?;
        let parsed = parse_response(&out.stdout).with_context(|| {
            let detail = out.stderr.lines().next().unwrap_or("no stderr");
            format!("gh api exited with status {}: {detail}", out.status)
        })?;
        if parsed.status == 404 {
            return Ok(None);
        }
        let response = checked_response(&out)?;
        Ok(Some(label_array_names(&response.body, "remove label")?))
    }

    /// Create an issue and return the issue GitHub answered with.
    pub fn create_issue(&self, owner_repo: &str, title: &str, body: &str) -> Result<Issue> {
        let url = format!("repos/{owner_repo}/issues");
        let title_field = format!("title={title}");
        let body_field = format!("body={body}");
        let args = [
            "api",
            "-i",
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
        let response = checked_response(&out)?;
        let value: Value = serde_json::from_str(&response.body)
            .context("gh api returned a broken create_issue body")?;
        issue_from_value(&value)?
            .ok_or_else(|| anyhow!("create_issue returned a pull request object"))
    }
}

/// The raw status fields of one mentioned GitHub object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionFields {
    /// The raw `state` string: `open` or `closed`.
    pub state: String,
    /// Whether `pull_request.merged_at` carries a timestamp.
    pub merged: bool,
    /// The raw `draft` flag. A plain issue reports `false`.
    pub draft: bool,
    /// Whether the object carries the `pull_request` key.
    pub is_pr: bool,
}

/// Parse one issue response and reject a pull request object.
fn issue_response(out: &CmdOut, operation: &str) -> Result<Issue> {
    let response = checked_response(out)?;
    let value: Value = serde_json::from_str(&response.body)
        .with_context(|| format!("gh api returned a broken {operation} body"))?;
    issue_from_value(&value)?.ok_or_else(|| anyhow!("{operation} returned a pull request object"))
}

/// Parse the label names from one label-array response.
fn label_array_names(body: &str, operation: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body)
        .with_context(|| format!("gh api returned a broken {operation} body"))?;
    let labels = value
        .as_array()
        .ok_or_else(|| anyhow!("gh api returned a non-array {operation} body"))?;
    labels
        .iter()
        .map(|label| Ok(str_field(label, "name")?.to_string()))
        .collect()
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

/// Parse one command output and reject command or HTTP failures.
fn checked_response(out: &CmdOut) -> Result<Response> {
    let response = parse_response(&out.stdout).with_context(|| {
        let detail = out.stderr.lines().next().unwrap_or("no stderr");
        format!("gh api exited with status {}: {detail}", out.status)
    })?;
    ensure_ok(&response, &out.stderr)?;
    // `gh api` exits with status 1 for a valid conditional HTTP 304.
    if out.status != 0 && !(out.status == 1 && response.status == 304) {
        let detail = out.stderr.lines().next().unwrap_or("no stderr");
        bail!("gh api exited with status {}: {detail}", out.status);
    }
    Ok(response)
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
        body: nullable_str(value, "body")?.to_string(),
        labels: label_names(value)?,
        author: author_login(value)?.to_string(),
        assignees: assignee_logins(value)?,
        updated_at: str_field(value, "updated_at")?.to_string(),
        github_url: str_field(value, "html_url")?.to_string(),
        open: state_is_open(value)?,
    }))
}

/// Map one GitHub object to a [`Pr`].
fn pr_from_value(value: &Value) -> Result<Pr> {
    Ok(Pr {
        number: u64_field(value, "number")?,
        node_id: str_field(value, "node_id")?.to_string(),
        title: str_field(value, "title")?.to_string(),
        body: nullable_str(value, "body")?.to_string(),
        labels: label_names(value)?,
        open: state_is_open(value)?,
        draft: bool_field(value, "draft")?,
        head_sha: value
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("GitHub object has no string field \"head.sha\""))?
            .to_string(),
        head_ref: value
            .pointer("/head/ref")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("GitHub object has no string field \"head.ref\""))?
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

/// The string value of `key`; an explicit null becomes an empty string.
fn nullable_str<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    match value.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(Value::Null) => Ok(""),
        _ => Err(anyhow!(
            "GitHub object has no string or null field \"{key}\""
        )),
    }
}

/// The boolean value of `key`, or an error when the key is missing.
fn bool_field(value: &Value, key: &str) -> Result<bool> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("GitHub object has no boolean field \"{key}\""))
}

/// The label names of one GitHub object.
fn label_names(value: &Value) -> Result<Vec<String>> {
    let labels = value
        .get("labels")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("GitHub object has no array field \"labels\""))?;
    labels
        .iter()
        .map(|label| Ok(str_field(label, "name")?.to_string()))
        .collect()
}

/// The issue author login, or `unknown` after GitHub deletes the account.
fn author_login(value: &Value) -> Result<&str> {
    match value.get("user") {
        Some(Value::Null) => Ok("unknown"),
        Some(user) => user
            .get("login")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("GitHub object has no string field \"user.login\"")),
        None => Err(anyhow!("GitHub object has no field \"user\"")),
    }
}

/// Encode one UTF-8 value as one RFC 3986 path segment.
fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

/// The assigned GitHub logins of one issue.
fn assignee_logins(value: &Value) -> Result<Vec<String>> {
    value
        .get("assignees")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("GitHub object has no array field \"assignees\""))?
        .iter()
        .map(|assignee| Ok(str_field(assignee, "login")?.to_string()))
        .collect()
}

/// Whether the GitHub object is in the open state.
fn state_is_open(value: &Value) -> Result<bool> {
    match str_field(value, "state")? {
        "open" => Ok(true),
        "closed" => Ok(false),
        state => bail!("GitHub object has unknown state \"{state}\""),
    }
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
            r#"{{"number":{number},"node_id":"node-{number}","title":"issue {number}","body":"body {number}","state":"open","labels":[],"user":{{"login":"author-{number}"}},"assignees":[{{"login":"owner-{number}"}}],"updated_at":"2026-08-{number:02}T12:00:00Z","html_url":"https://github.com/acme/borsuk/issues/{number}"}}"#
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
        assert_eq!(fetched.items.len(), 2);
        let one = &fetched.items[&1];
        assert_eq!(one.title, "issue 1");
        assert_eq!(one.node_id, "node-1");
        assert_eq!(one.body, "body 1");
        assert_eq!(one.author, "author-1");
        assert_eq!(one.assignees, vec!["owner-1"]);
        assert_eq!(one.updated_at, "2026-08-01T12:00:00Z");
        assert_eq!(one.github_url, "https://github.com/acme/borsuk/issues/1");
        assert!(one.open);
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
        assert_eq!(second.items.len(), 1);
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_nonzero_gh_exit_for_304_reuses_the_cached_page() {
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
                CmdOut {
                    status: 1,
                    stdout: response("HTTP/2 304", &["etag: \"v1\""], ""),
                    stderr: "gh: HTTP 304\n".to_string(),
                },
            );
        let mut client = GhClient::new(&exec);
        let first = client.fetch_issues("acme/borsuk").unwrap();

        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(second.items, first.items);
        assert_eq!(second.unchanged, vec![1]);
    }

    #[test]
    fn a_process_status_other_than_one_keeps_a_304_as_an_error() {
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
                CmdOut {
                    status: 2,
                    stdout: response("HTTP/2 304", &["etag: \"v1\""], ""),
                    stderr: "unexpected command failure\n".to_string(),
                },
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();

        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("status 2"), "error was: {error}");
    }

    #[test]
    fn a_nonzero_process_status_for_a_200_remains_an_error() {
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
                stdout: response("HTTP/2 200", &[], &issues_json(1, 1)),
                stderr: "unexpected command failure\n".to_string(),
            },
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("status 1"), "error was: {error}");
    }

    #[test]
    fn an_etag_from_one_repository_is_not_sent_to_another_repository() {
        let exec = ScriptExec::new()
            .expect(
                |call| call.program == "gh",
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"repo-a\""],
                    &issues_json(1, 1),
                )),
            )
            .expect(
                |call| call.program == "gh",
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"repo-b\""],
                    &issues_json(2, 2),
                )),
            );
        let mut client = GhClient::new(&exec);

        client.fetch_issues("acme/one").unwrap();
        client.fetch_issues("acme/two").unwrap();

        assert_eq!(
            exec.calls()[1].argv(),
            [
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/two/issues?state=open&per_page=100&page=1"
            ]
        );
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
        let first = client.fetch_issues("acme/borsuk").unwrap();
        let second = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(second.items, first.items);
        assert_eq!(second.unchanged, vec![1]);
    }

    #[test]
    fn a_304_page_with_a_cached_next_page_does_not_end_the_walk() {
        let next_link = "link: <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"next\", <https://api.github.com/repositories/1/issues?state=open&page=2>\
             ; rel=\"last\"";
        let edited_page = r#"[{"number":101,"node_id":"node-101","title":"issue 101","body":"body 101","state":"open","labels":[{"name":"refined"}],"user":{"login":"author-101"},"assignees":[{"login":"owner-101"}],"updated_at":"2026-08-01T12:00:00Z","html_url":"https://github.com/acme/borsuk/issues/101"}]"#;
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
        assert_eq!(second.items.len(), 101);
        assert_eq!(second.items[&101].labels, vec!["refined".to_string()]);
        assert_eq!(exec.calls().len(), 4);
    }

    #[test]
    fn a_304_page_known_to_be_short_ends_the_walk() {
        let next_link = "link: <https://api.github.com/repositories/1/issues?page=2>; rel=\"next\"";
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
                    &["etag: \"s1\"", next_link],
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
        assert_eq!(second.items.len(), 1);
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn a_304_without_a_cached_page_is_an_error() {
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
        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("no cached page"));
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

        assert_eq!(fetched.items.len(), 103);
        assert!(fetched.items.contains_key(&1));
        assert!(fetched.items.contains_key(&103));
        assert!(fetched.unchanged.is_empty());
        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1]
            .argv()
            .contains(&"repos/acme/borsuk/issues?state=open&per_page=100&page=2"));
    }

    #[test]
    fn a_304_page_without_a_cached_next_link_ends_pagination() {
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
                    &["etag: \"p1\""],
                    &issues_json(1, 100),
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
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            );
        let mut client = GhClient::new(&exec);
        client.fetch_issues("acme/borsuk").unwrap();
        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.items.len(), 100);
        assert_eq!(fetched.unchanged, vec![1]);
        assert_eq!(exec.calls().len(), 2);
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

        assert_eq!(fetched.items.len(), 1);
        assert!(fetched.items.contains_key(&8));
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
            CmdOut {
                status: 1,
                stdout: response("HTTP/2 403", &["retry-after: 60"], "{}"),
                stderr: "HTTP 403\n".into(),
            },
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
            CmdOut {
                status: 1,
                stdout: response("HTTP/2 429", &["retry-after: 7"], "{}"),
                stderr: "HTTP 429\n".into(),
            },
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
    fn a_command_failure_without_a_response_head_keeps_stderr() {
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
                stdout: String::new(),
                stderr: "authentication failed\n".into(),
            },
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("authentication failed"));
    }

    #[test]
    fn fetch_mention_status_maps_a_merged_pr() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"closed","labels":[],"draft":false,"pull_request":{"merged_at":"2026-09-01T10:00:00Z"},"head":{"sha":"abc","ref":"x"}}"#;
        let exec = ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/5"]),
            CmdOut::ok(response("HTTP/2 200", &["etag: \"m1\""], pr_json)),
        );
        let client = GhClient::new(&exec);

        let fields = client
            .fetch_mention_status("acme/borsuk", 5)
            .unwrap()
            .unwrap();

        assert!(fields.is_pr);
        assert!(fields.merged);
        assert_eq!(fields.state, "closed");
        assert!(!fields.draft);
    }

    #[test]
    fn fetch_mention_status_maps_a_plain_closed_issue() {
        let exec = ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/8"]),
            CmdOut::ok(response("HTTP/2 200", &["etag: \"c1\""], &issue_json(8))),
        );
        let client = GhClient::new(&exec);

        let fields = client
            .fetch_mention_status("acme/borsuk", 8)
            .unwrap()
            .unwrap();

        assert!(!fields.is_pr);
        assert!(!fields.merged);
        assert!(!fields.draft);
        assert_eq!(fields.state, "open", "the fixture issue is open");

        let closed = r#"{"number":9,"node_id":"n9","title":"i 9","body":"","state":"closed","labels":[],"user":{"login":"a"},"assignees":[],"updated_at":"2026-08-09T12:00:00Z","html_url":"u"}"#;
        let exec = ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/9"]),
            CmdOut::ok(response("HTTP/2 200", &["etag: \"c2\""], closed)),
        );
        let client = GhClient::new(&exec);
        let fields = client
            .fetch_mention_status("acme/borsuk", 9)
            .unwrap()
            .unwrap();
        assert_eq!(fields.state, "closed");
    }

    #[test]
    fn a_404_mention_answer_maps_to_none() {
        let exec = ScriptExec::new().expect(
            gh(&["api", "-i", "-X", "GET", "repos/acme/borsuk/issues/99"]),
            CmdOut {
                status: 1,
                stdout: response("HTTP/2 404", &[], "{\"message\":\"Not Found\"}"),
                stderr: "HTTP 404\n".into(),
            },
        );
        let client = GhClient::new(&exec);

        let answer = client.fetch_mention_status("acme/borsuk", 99).unwrap();

        assert!(answer.is_none());
    }

    #[test]
    fn fetch_pulls_maps_draft_and_head_sha() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"open","labels":[{"name":"release-stacked"},{"name":"x"}],"draft":true,"head":{"sha":"abc123","ref":"aif/borsuk/issue-142"}}"#;
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

        let pr = &fetched.items[&5];
        assert!(pr.draft);
        assert_eq!(pr.head_sha, "abc123");
        assert_eq!(pr.head_ref, "aif/borsuk/issue-142");
        assert_eq!(
            pr.labels,
            vec!["release-stacked".to_string(), "x".to_string()]
        );
        assert_eq!(pr.body, "");
        assert!(pr.open);
    }

    #[test]
    fn a_pull_without_a_draft_field_is_rejected() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"open","labels":[],"head":{"sha":"abc123"}}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/pulls?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], &format!("[{pr_json}]"))),
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_pulls("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("draft"));
    }

    #[test]
    fn a_pull_without_a_head_sha_is_rejected() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"open","labels":[],"draft":false,"head":{}}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/pulls?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], &format!("[{pr_json}]"))),
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_pulls("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("head.sha"));
    }

    #[test]
    fn a_pull_without_a_head_ref_is_rejected() {
        let pr_json = r#"{"number":5,"node_id":"PR_5","title":"pr 5","body":null,"state":"open","labels":[],"draft":false,"head":{"sha":"abc123"}}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/pulls?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], &format!("[{pr_json}]"))),
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_pulls("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("head.ref"));
    }

    #[test]
    fn an_issue_without_labels_is_rejected() {
        let issue =
            r#"[{"number":1,"node_id":"node-1","title":"issue 1","body":"body","state":"open"}]"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], issue)),
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("labels"));
    }

    #[test]
    fn an_unknown_item_state_is_rejected() {
        let issue = r#"[{"number":1,"node_id":"node-1","title":"issue 1","body":"body","state":"unknown","labels":[],"user":{"login":"author"},"assignees":[],"updated_at":"2026-08-01T12:00:00Z","html_url":"https://github.com/acme/borsuk/issues/1"}]"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], issue)),
        );
        let mut client = GhClient::new(&exec);

        let error = client.fetch_issues("acme/borsuk").unwrap_err();

        assert!(error.to_string().contains("unknown state"));
    }

    #[test]
    fn add_label_posts_to_the_labels_endpoint() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
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
                "-i",
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
                "-i",
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
    fn remove_label_treats_an_absent_label_as_removed() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "DELETE",
                "repos/acme/borsuk/issues/7/labels/refined",
            ]),
            CmdOut {
                status: 1,
                stdout: response("HTTP/2 404", &[], r#"{"message":"Label does not exist"}"#),
                stderr: "gh: Label does not exist (HTTP 404)\n".to_string(),
            },
        );
        let client = GhClient::new(&exec);
        client.remove_label("acme/borsuk", 7, "refined").unwrap();

        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn remove_label_encodes_the_label_as_one_path_segment() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "DELETE",
                "repos/acme/borsuk/issues/7/labels/needs%2Freview%20%2B%20qa",
            ]),
            CmdOut::ok(response("HTTP/2 204", &[], "")),
        );
        let client = GhClient::new(&exec);
        client
            .remove_label("acme/borsuk", 7, "needs/review + qa")
            .unwrap();

        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn an_issue_with_a_deleted_author_stays_in_the_issue_list() {
        let issue = r#"[{"number":1,"node_id":"node-1","title":"issue 1","body":"body","state":"open","labels":[],"user":null,"assignees":[],"updated_at":"2026-08-01T12:00:00Z","html_url":"https://github.com/acme/borsuk/issues/1"}]"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues?state=open&per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &[], issue)),
        );
        let mut client = GhClient::new(&exec);

        let fetched = client.fetch_issues("acme/borsuk").unwrap();

        assert_eq!(fetched.items[&1].author, "unknown");
    }

    #[test]
    fn create_issue_returns_the_created_issue() {
        let created = r#"{"number":42,"node_id":"IC_42","title":"decision","body":"why","state":"open","labels":[],"user":{"login":"author"},"assignees":[],"updated_at":"2026-08-30T12:00:00Z","html_url":"https://github.com/acme/borsuk/issues/42"}"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
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

    /// A JSON array of comment objects whose bodies carry their index.
    fn comments_json(count: usize) -> String {
        let items: Vec<String> = (0..count)
            .map(|index| {
                format!(
                    r#"{{"user":{{"login":"agent"}},"created_at":"2026-09-01T10:00:00Z","body":"comment {index}"}}"#
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn fetch_issue_comments_runs_the_exact_gh_call_and_maps_the_comments() {
        let body = r#"[{"user":{"login":"agent"},"created_at":"2026-09-01T10:00:00Z","body":"Which mode ships first?"},{"user":{"login":"human"},"created_at":"2026-09-02T11:30:00Z","body":"plain prose"}]"#;
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=1",
            ]),
            CmdOut::ok(response("HTTP/2 200", &["etag: \"c1\""], body)),
        );
        let client = GhClient::new(&exec);
        let comments = client.fetch_issue_comments("acme/borsuk", 9).unwrap();

        assert_eq!(
            comments,
            vec![
                IssueComment {
                    author: "agent".to_string(),
                    created_at: "2026-09-01T10:00:00Z".to_string(),
                    body: "Which mode ships first?".to_string(),
                },
                IssueComment {
                    author: "human".to_string(),
                    created_at: "2026-09-02T11:30:00Z".to_string(),
                    body: "plain prose".to_string(),
                },
            ]
        );
        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].argv(),
            [
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=1"
            ]
        );
    }

    #[test]
    fn fetch_issue_comments_walks_a_link_next_page_and_merges_in_order() {
        let next_link = "link: <https://api.github.com/repositories/1/issues/9/comments?page=2>\
             ; rel=\"next\", <https://api.github.com/repositories/1/issues/9/comments?page=2>\
             ; rel=\"last\"";
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues/9/comments?per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"c1\"", next_link],
                    &comments_json(100),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/borsuk/issues/9/comments?per_page=100&page=2",
                ]),
                CmdOut::ok(response("HTTP/2 200", &["etag: \"c2\""], &comments_json(2))),
            );
        let client = GhClient::new(&exec);
        let comments = client.fetch_issue_comments("acme/borsuk", 9).unwrap();

        assert_eq!(comments.len(), 102);
        assert_eq!(comments[0].body, "comment 0");
        assert_eq!(comments[99].body, "comment 99");
        assert_eq!(comments[100].body, "comment 0");
        assert_eq!(comments[101].body, "comment 1");
        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].argv(),
            [
                "api",
                "-i",
                "-X",
                "GET",
                "repos/acme/borsuk/issues/9/comments?per_page=100&page=2"
            ]
        );
    }
}
