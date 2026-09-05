//! Runs one poller thread per repository and feeds one inbound channel.
//!
//! Each poller fetches its repository through the [`GhClient`] and sends the
//! whole [`RepoSnapshot`] to the daemon on one shared channel. Between passes
//! the poller waits on its own wake channel, so the daemon can force an early
//! pass. A failed pass is reported and the poller backs off, at most five
//! minutes, so one broken repository never stops the others. [`spawn_pollers`]
//! returns the wake sender of every repository, so the daemon holds them from
//! the start.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::{Config, RepoConfig};
use crate::exec::{Exec, RealExec};
use crate::gh::GhClient;
use crate::model::RepoSnapshot;

/// The normal wait between two poll passes of one repository.
const POLL_INTERVAL: Duration = Duration::from_secs(20);

/// The longest wait after repeated poll failures.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// One inbound message of the daemon event loop.
///
/// The pollers of this module send [`Polled`][DaemonMsg::Polled] and
/// [`PollFailed`][DaemonMsg::PollFailed]. Later chunks add the variants of the
/// runners, the trains, and the control socket.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonMsg {
    /// One finished poll pass. `snapshot` holds the complete current state of
    /// the repository; the reader already merged the cached entries of the
    /// pages that answered 304 into it.
    Polled {
        /// The poll start time in milliseconds since the Unix epoch.
        started_ms: u64,
        /// The repository alias of the poller.
        repo: String,
        /// The complete snapshot of the repository.
        snapshot: RepoSnapshot,
    },
    /// One failed poll pass. The poller keeps running and backs off.
    PollFailed {
        /// The repository alias of the poller.
        repo: String,
        /// The error of the failed pass, with its full context chain.
        error: String,
    },
    /// The daemon must shut down.
    Shutdown,
}

/// The join handles and the wake senders of a set of spawned pollers.
#[derive(Debug)]
pub struct Pollers {
    /// One join handle per spawned poller, in repository alias order.
    pub handles: Vec<JoinHandle<()>>,
    /// The wake sender of each repository, keyed by alias. The daemon sends
    /// on a sender to force an early pass, and drops it to end that poller.
    pub wake: BTreeMap<String, Sender<()>>,
}

/// Spawn one poller thread per configured repository.
///
/// Each thread loops: fetch, send [`DaemonMsg::Polled`], wait
/// [`POLL_INTERVAL`] on its wake channel. A failed pass sends
/// [`DaemonMsg::PollFailed`] and doubles the wait, at most [`MAX_BACKOFF`].
/// Each thread runs until the daemon drops its wake sender or the channel
/// closes. The join handles follow repository alias order.
pub fn spawn_pollers(cfg: &Config, tx: Sender<DaemonMsg>) -> Pollers {
    let exec_for = |_alias: &str| Arc::new(RealExec) as Arc<dyn Exec>;
    spawn_all(cfg, tx, &exec_for, POLL_INTERVAL, MAX_BACKOFF)
}

/// Spawn the poller thread of one repository with the production cadence.
///
/// The thread loops like every poller of [`spawn_pollers`]: it fetches,
/// sends on `tx`, and waits [`POLL_INTERVAL`] on its wake channel, with the
/// [`MAX_BACKOFF`] cap after failures. The call returns the wake sender, so
/// the caller can force an early pass and drop the sender to end the
/// poller. It returns `None` when the thread cannot start; the case sends
/// [`DaemonMsg::PollFailed`].
pub fn spawn_poller(repo: &RepoConfig, tx: Sender<DaemonMsg>) -> Option<Sender<()>> {
    let exec: Arc<dyn Exec> = Arc::new(RealExec);
    spawn_poller_with(repo, tx, exec, POLL_INTERVAL, MAX_BACKOFF).map(|(_, wake)| wake)
}

/// Spawn the poller thread of one repository with an explicit runner and
/// cadence.
///
/// The call returns the thread handle and the wake sender, so a test can
/// join the thread and force an early pass. It returns `None` when the
/// thread cannot start; the case sends [`DaemonMsg::PollFailed`].
fn spawn_poller_with(
    repo: &RepoConfig,
    tx: Sender<DaemonMsg>,
    exec: Arc<dyn Exec>,
    interval: Duration,
    max_backoff: Duration,
) -> Option<(JoinHandle<()>, Sender<()>)> {
    let (wake_tx, wake_rx) = mpsc::channel();
    spawn_one(
        repo.alias.clone(),
        repo.owner_repo.clone(),
        exec,
        tx,
        wake_rx,
        interval,
        max_backoff,
    )
    .map(|handle| (handle, wake_tx))
}

/// Spawn every poller with an explicit [`Exec`] factory and waits.
///
/// The function creates each wake channel before it spawns the thread, gives
/// the receiver to the thread, and returns the sender in the map. The tests
/// use it to inject a [`ScriptExec`][crate::exec::ScriptExec] and short
/// waits; production uses [`spawn_pollers`]. A repository whose thread cannot
/// start contributes no handle, no sender, and sends
/// [`DaemonMsg::PollFailed`].
fn spawn_all(
    cfg: &Config,
    tx: Sender<DaemonMsg>,
    exec_for: &dyn Fn(&str) -> Arc<dyn Exec>,
    interval: Duration,
    max_backoff: Duration,
) -> Pollers {
    let mut handles = Vec::new();
    let mut wake = BTreeMap::new();
    for repo in cfg.repos.values() {
        let (wake_tx, wake_rx) = mpsc::channel();
        if let Some(handle) = spawn_one(
            repo.alias.clone(),
            repo.owner_repo.clone(),
            exec_for(&repo.alias),
            tx.clone(),
            wake_rx,
            interval,
            max_backoff,
        ) {
            handles.push(handle);
            wake.insert(repo.alias.clone(), wake_tx);
        }
    }
    Pollers { handles, wake }
}

/// Spawn the poller thread of one repository.
///
/// Returns `None` when the thread cannot start; the case sends
/// [`DaemonMsg::PollFailed`].
fn spawn_one(
    repo: String,
    owner_repo: String,
    exec: Arc<dyn Exec>,
    tx: Sender<DaemonMsg>,
    wake_rx: Receiver<()>,
    interval: Duration,
    max_backoff: Duration,
) -> Option<JoinHandle<()>> {
    let name = format!("poll-{repo}");
    let thread_repo = repo.clone();
    let thread_tx = tx.clone();
    let spawned = thread::Builder::new().name(name).spawn(move || {
        let log_repo = thread_repo.clone();
        if let Err(error) = poller_loop(
            thread_repo,
            owner_repo,
            exec,
            thread_tx,
            wake_rx,
            interval,
            max_backoff,
        ) {
            eprintln!("poller {log_repo} stopped: {error:#}");
        }
    });
    match spawned {
        Ok(handle) => Some(handle),
        Err(error) => {
            let failed = DaemonMsg::PollFailed {
                repo,
                error: format!("cannot start the poller thread: {error}"),
            };
            if let Err(send_error) = tx.send(failed) {
                eprintln!("cannot report the poller start failure: {send_error}");
            }
            None
        }
    }
}

/// Run the poll loop of one repository until its wake channel disconnects.
/// The loop returns an error when it cannot send a message to the daemon.
fn poller_loop(
    repo: String,
    owner_repo: String,
    exec: Arc<dyn Exec>,
    tx: Sender<DaemonMsg>,
    wake_rx: Receiver<()>,
    interval: Duration,
    max_backoff: Duration,
) -> Result<()> {
    let mut client = GhClient::new(exec.as_ref());
    let mut backoff = interval;
    loop {
        let started_ms = wall_clock_ms();
        let failed = match fetch_repo(&mut client, &owner_repo) {
            Ok(snapshot) => {
                backoff = interval;
                let polled = DaemonMsg::Polled {
                    started_ms,
                    repo: repo.clone(),
                    snapshot,
                };
                tx.send(polled)
                    .context("cannot send poll result to daemon")?;
                false
            }
            Err(error) => {
                let failed = DaemonMsg::PollFailed {
                    repo: repo.clone(),
                    error: format!("{error:#}"),
                };
                tx.send(failed)
                    .context("cannot send poll failure to daemon")?;
                true
            }
        };
        match wake_rx.recv_timeout(backoff) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            // The daemon dropped the wake sender; the poller stops.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if failed {
            backoff = next_backoff(backoff, max_backoff);
        }
    }
}

/// The current wall-clock time in milliseconds since the Unix epoch.
fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Fetch the issues and the pull requests of one repository into a snapshot.
///
/// The reader merges the cached entries of the pages that answered 304 with
/// the fresh entries of the changed pages, so each fetched `items` map is
/// the complete current set and the poller stores it whole. A partial result
/// is never sent: when either list fails, the whole pass fails, because
/// applying a partial snapshot would drop the items of the failed list.
fn fetch_repo(client: &mut GhClient<'_>, owner_repo: &str) -> Result<RepoSnapshot> {
    let issues = client.fetch_issues(owner_repo)?;
    let pulls = client.fetch_pulls(owner_repo)?;
    Ok(RepoSnapshot {
        issues: issues.items,
        prs: pulls.items,
    })
}

/// The wait after one more failure: double `current`, capped at `max`.
fn next_backoff(current: Duration, max: Duration) -> Duration {
    current
        .checked_mul(2)
        .map_or(max, |doubled| doubled.min(max))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Mutex, PoisonError};
    use std::time::Instant;

    use super::*;
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, Pr};

    const REPO: &str = "borsuk";
    const OWNER_REPO: &str = "acme/borsuk";
    const ISSUES_URL: &str = "repos/acme/borsuk/issues?state=open&per_page=100&page=1";
    const PULLS_URL: &str = "repos/acme/borsuk/pulls?state=open&per_page=100&page=1";

    struct TimedExec {
        inner: ScriptExec,
        starts: Mutex<Vec<Instant>>,
    }

    impl TimedExec {
        fn new(inner: ScriptExec) -> Self {
            Self {
                inner,
                starts: Mutex::new(Vec::new()),
            }
        }

        fn starts(&self) -> Vec<Instant> {
            self.starts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Exec for TimedExec {
        fn run(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<CmdOut> {
            self.starts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Instant::now());
            self.inner.run(program, args, cwd)
        }
    }

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
            r#"{{"number":{number},"node_id":"node-{number}","title":"issue {number}","body":"body {number}","state":"open","labels":[],"user":{{"login":"author-{number}"}},"assignees":[],"updated_at":"2026-08-01T00:00:00Z","html_url":"https://github.com/acme/borsuk/issues/{number}"}}"#
        )
    }

    /// One GitHub draft pull request object.
    fn pr_json(number: u64, sha: &str) -> String {
        format!(
            r#"{{"number":{number},"node_id":"prnode-{number}","title":"pr {number}","body":"","state":"open","labels":[],"draft":true,"head":{{"sha":"{sha}","ref":"aif/borsuk/issue-{number}"}}}}"#
        )
    }

    /// The model form of [`issue_json`].
    fn model_issue(number: u64) -> Issue {
        Issue {
            number,
            node_id: format!("node-{number}"),
            title: format!("issue {number}"),
            body: format!("body {number}"),
            labels: vec![],
            author: format!("author-{number}"),
            assignees: Vec::new(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
            github_url: format!("https://github.com/acme/borsuk/issues/{number}"),
            open: true,
        }
    }

    /// The model form of [`pr_json`].
    fn model_pr(number: u64, sha: &str) -> Pr {
        Pr {
            number,
            node_id: format!("prnode-{number}"),
            title: format!("pr {number}"),
            body: String::new(),
            labels: vec![],
            open: true,
            draft: true,
            head_sha: sha.to_string(),
            head_ref: format!("aif/borsuk/issue-{number}"),
        }
    }

    /// A snapshot from model issues and pull requests.
    fn snapshot(issues: Vec<Issue>, prs: Vec<Pr>) -> RepoSnapshot {
        RepoSnapshot {
            issues: issues.into_iter().map(|i| (i.number, i)).collect(),
            prs: prs.into_iter().map(|p| (p.number, p)).collect(),
        }
    }

    /// The next message, or a panic that names what the test waited for.
    fn next_msg(rx: &Receiver<DaemonMsg>, what: &str) -> DaemonMsg {
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("timed out waiting for {what}: {error}"))
    }

    /// The issues and the pulls steps of one full pass of [`REPO`].
    ///
    /// The steps are appended in call order: issues first, then pulls.
    fn pass_steps(
        exec: ScriptExec,
        issues: (Vec<&str>, CmdOut),
        pulls: (Vec<&str>, CmdOut),
    ) -> ScriptExec {
        exec.expect(gh(&issues.0), issues.1)
            .expect(gh(&pulls.0), pulls.1)
    }

    #[test]
    fn a_wake_forces_an_early_pass_and_merges_the_unchanged_pages() {
        // The interval is ten seconds, so the second pass can only come from
        // the wake; the test would time out otherwise.
        let exec = Arc::new(
            pass_steps(
                ScriptExec::new(),
                (
                    vec!["api", "-i", "-X", "GET", ISSUES_URL],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"i1\""],
                        &format!("[{}]", issue_json(1)),
                    )),
                ),
                (
                    vec!["api", "-i", "-X", "GET", PULLS_URL],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"p1\""],
                        &format!("[{}]", pr_json(2, "aaa")),
                    )),
                ),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"i1\"",
                    "-X",
                    "GET",
                    ISSUES_URL,
                ]),
                // Page 1 of the issues is unchanged; its cached entries must
                // still reach the snapshot.
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"p1\"",
                    "-X",
                    "GET",
                    PULLS_URL,
                ]),
                // The pulls changed while the issues did not.
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p2\""],
                    &format!("[{}]", pr_json(2, "bbb")),
                )),
            ),
        );
        let (tx, rx) = mpsc::channel();
        let (wake, wake_rx) = mpsc::channel();
        let handle = spawn_one(
            REPO.to_string(),
            OWNER_REPO.to_string(),
            exec.clone(),
            tx,
            wake_rx,
            Duration::from_secs(10),
            Duration::from_secs(20),
        )
        .unwrap();

        let DaemonMsg::Polled {
            repo,
            snapshot: first,
            started_ms: first_started_ms,
        } = next_msg(&rx, "the first poll")
        else {
            panic!("the first message was not Polled");
        };
        assert!(first_started_ms > 0);
        assert_eq!(repo, REPO);
        assert_eq!(
            first,
            snapshot(vec![model_issue(1)], vec![model_pr(2, "aaa")])
        );

        wake.send(()).unwrap();
        let DaemonMsg::Polled {
            repo,
            snapshot: second,
            ..
        } = next_msg(&rx, "the woken poll")
        else {
            panic!("the wake did not produce a Polled message");
        };
        assert_eq!(repo, REPO);
        // The cached issue from the 304 page and the fresh pull request.
        assert_eq!(
            second,
            snapshot(vec![model_issue(1)], vec![model_pr(2, "bbb")])
        );

        // The poller waits for the next interval; dropping the wake sender
        // must end it.
        drop(wake);
        handle.join().unwrap();
        let calls = exec.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].argv(), ["api", "-i", "-X", "GET", ISSUES_URL]);
        assert_eq!(calls[1].argv(), ["api", "-i", "-X", "GET", PULLS_URL]);
        assert_eq!(
            calls[2].argv(),
            [
                "api",
                "-i",
                "-H",
                "If-None-Match: \"i1\"",
                "-X",
                "GET",
                ISSUES_URL
            ]
        );
        assert_eq!(
            calls[3].argv(),
            [
                "api",
                "-i",
                "-H",
                "If-None-Match: \"p1\"",
                "-X",
                "GET",
                PULLS_URL
            ]
        );
    }

    #[test]
    fn failures_back_off_and_the_thread_stays_alive() {
        let broken = CmdOut {
            status: 1,
            stdout: String::new(),
            stderr: "gh is unhappy\n".into(),
        };
        let base = ScriptExec::new()
            .expect(gh(&["api", "-i", "-X", "GET", ISSUES_URL]), broken.clone())
            .expect(gh(&["api", "-i", "-X", "GET", ISSUES_URL]), broken);
        let exec = Arc::new(TimedExec::new(pass_steps(
            base,
            (
                vec!["api", "-i", "-X", "GET", ISSUES_URL],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"i1\""],
                    &format!("[{}]", issue_json(1)),
                )),
            ),
            (
                vec!["api", "-i", "-X", "GET", PULLS_URL],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p1\""],
                    &format!("[{}]", pr_json(2, "aaa")),
                )),
            ),
        )));
        let (tx, rx) = mpsc::channel();
        let (wake, wake_rx) = mpsc::channel();
        let handle = spawn_one(
            REPO.to_string(),
            OWNER_REPO.to_string(),
            exec.clone(),
            tx,
            wake_rx,
            Duration::from_millis(50),
            Duration::from_millis(100),
        )
        .unwrap();

        let DaemonMsg::PollFailed { repo, error } = next_msg(&rx, "the first failure") else {
            panic!("the first message was not PollFailed");
        };
        assert_eq!(repo, REPO);
        assert!(error.contains("gh is unhappy"), "error was: {error}");
        let DaemonMsg::PollFailed { repo, .. } = next_msg(&rx, "the second failure") else {
            panic!("the third message was not PollFailed");
        };
        assert_eq!(repo, REPO);
        // The thread survived two failures and completed the next pass.
        let DaemonMsg::Polled {
            repo,
            snapshot: snap,
            ..
        } = next_msg(&rx, "the recovery poll")
        else {
            panic!("the fourth message was not Polled");
        };
        assert_eq!(repo, REPO);
        assert_eq!(
            snap,
            snapshot(vec![model_issue(1)], vec![model_pr(2, "aaa")])
        );

        drop(wake);
        handle.join().unwrap();

        let starts = exec.starts();
        assert_eq!(starts.len(), 4);
        assert!(starts[1].duration_since(starts[0]) >= Duration::from_millis(40));
        assert!(starts[2].duration_since(starts[1]) >= Duration::from_millis(90));
    }

    #[test]
    fn a_closed_daemon_channel_returns_the_send_error() {
        let exec = Arc::new(pass_steps(
            ScriptExec::new(),
            (
                vec!["api", "-i", "-X", "GET", ISSUES_URL],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"i1\""],
                    &format!("[{}]", issue_json(1)),
                )),
            ),
            (
                vec!["api", "-i", "-X", "GET", PULLS_URL],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"p1\""],
                    &format!("[{}]", pr_json(2, "aaa")),
                )),
            ),
        ));
        let (tx, rx) = mpsc::channel();
        let (_wake, wake_rx) = mpsc::channel();
        drop(rx);

        let error = poller_loop(
            REPO.to_string(),
            OWNER_REPO.to_string(),
            exec,
            tx,
            wake_rx,
            Duration::from_secs(10),
            Duration::from_secs(20),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot send poll result to daemon"),
            "error was: {error:#}"
        );
    }

    #[test]
    fn the_backoff_doubles_and_caps() {
        assert_eq!(
            next_backoff(Duration::from_secs(60), MAX_BACKOFF),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(240), MAX_BACKOFF),
            MAX_BACKOFF
        );
        assert_eq!(next_backoff(MAX_BACKOFF, MAX_BACKOFF), MAX_BACKOFF);
        // An overflow saturates at the cap instead of panicking.
        assert_eq!(next_backoff(Duration::MAX, MAX_BACKOFF), MAX_BACKOFF);
    }

    #[test]
    fn the_production_poll_interval_is_between_ten_and_thirty_seconds() {
        assert!(
            (Duration::from_secs(10)..=Duration::from_secs(30)).contains(&POLL_INTERVAL),
            "the production interval was {POLL_INTERVAL:?}"
        );
    }

    /// A config with the four stages and the two repositories `a` and `b`.
    fn two_repo_config() -> Config {
        Config::parse(
            r#"
schema_version = 1

[stage.refine]
model = "m"
harness = "claude"

[stage.implement]
model = "m"
harness = "opencode"

[stage.review]
model = "m"
harness = "opencode"

[stage.release]
model = "m"
harness = "claude"

[ticket.create]
harness = "opencode"
model = "m"

[ticket.chat]
harness = "claude"
model = "m"

[repo.a]
path = "/repos/a"

[repo.b]
path = "/repos/b"
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_wake_reaches_only_its_own_repository() {
        let mut config = two_repo_config();
        config.repos.get_mut("a").unwrap().owner_repo = "acme/one".to_string();
        config.repos.get_mut("b").unwrap().owner_repo = "acme/two".to_string();

        let exec_a: Arc<dyn Exec> = Arc::new(
            pass_steps(
                ScriptExec::new(),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/one/issues?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"ia1\""],
                        &format!("[{}]", issue_json(1)),
                    )),
                ),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/one/pulls?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"pa1\""],
                        &format!("[{}]", pr_json(2, "aaa")),
                    )),
                ),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"ia1\"",
                    "-X",
                    "GET",
                    "repos/acme/one/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"ia2\""],
                    &format!("[{}]", issue_json(7)),
                )),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"pa1\"",
                    "-X",
                    "GET",
                    "repos/acme/one/pulls?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            ),
        );
        let exec_b: Arc<dyn Exec> = Arc::new(pass_steps(
            ScriptExec::new(),
            (
                vec![
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/two/issues?state=open&per_page=100&page=1",
                ],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"ib1\""],
                    &format!("[{}]", issue_json(3)),
                )),
            ),
            (
                vec![
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/two/pulls?state=open&per_page=100&page=1",
                ],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"pb1\""],
                    &format!("[{}]", pr_json(5, "bbb")),
                )),
            ),
        ));
        let execs = BTreeMap::from([("a", exec_a), ("b", exec_b)]);
        let exec_for = move |alias: &str| execs[alias].clone();

        let (tx, rx) = mpsc::channel();
        let pollers = spawn_all(
            &config,
            tx,
            &exec_for,
            Duration::from_secs(10),
            Duration::from_secs(20),
        );
        assert_eq!(pollers.handles.len(), 2);

        let mut first_polls = BTreeMap::new();
        for _ in 0..2 {
            match next_msg(&rx, "the first poll of each repository") {
                DaemonMsg::Polled { repo, snapshot, .. } => {
                    first_polls.insert(repo, snapshot);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert_eq!(
            first_polls["a"],
            snapshot(vec![model_issue(1)], vec![model_pr(2, "aaa")])
        );
        assert_eq!(
            first_polls["b"],
            snapshot(vec![model_issue(3)], vec![model_pr(5, "bbb")])
        );

        pollers.wake["a"].send(()).unwrap();
        match next_msg(&rx, "the woken poll of a") {
            DaemonMsg::Polled {
                repo,
                snapshot: snap,
                ..
            } if repo == "a" => {
                assert_eq!(
                    snap,
                    snapshot(vec![model_issue(7)], vec![model_pr(2, "aaa")])
                );
            }
            other => panic!("the wake for a reached another repository: {other:?}"),
        }

        // The ten-second interval cannot produce another message here.
        if let Ok(other) = rx.recv_timeout(Duration::from_millis(200)) {
            panic!("the wake for a produced an extra message: {other:?}");
        }

        drop(pollers.wake);
        for handle in pollers.handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn the_wake_map_holds_one_sender_per_repository() {
        let mut config = two_repo_config();
        config.repos.get_mut("a").unwrap().owner_repo = "acme/one".to_string();
        config.repos.get_mut("b").unwrap().owner_repo = "acme/two".to_string();

        let exec_a: Arc<dyn Exec> = Arc::new(pass_steps(
            ScriptExec::new(),
            (
                vec![
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/one/issues?state=open&per_page=100&page=1",
                ],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"ia1\""],
                    &format!("[{}]", issue_json(1)),
                )),
            ),
            (
                vec![
                    "api",
                    "-i",
                    "-X",
                    "GET",
                    "repos/acme/one/pulls?state=open&per_page=100&page=1",
                ],
                CmdOut::ok(response(
                    "HTTP/2 200",
                    &["etag: \"pa1\""],
                    &format!("[{}]", pr_json(2, "aaa")),
                )),
            ),
        ));
        // The second pass of b answers 304 on both lists, so the wake of b
        // produces a second Polled with the same snapshot.
        let exec_b: Arc<dyn Exec> = Arc::new(
            pass_steps(
                ScriptExec::new(),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/two/issues?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"ib1\""],
                        &format!("[{}]", issue_json(3)),
                    )),
                ),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/two/pulls?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"pb1\""],
                        &format!("[{}]", pr_json(5, "bbb")),
                    )),
                ),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"ib1\"",
                    "-X",
                    "GET",
                    "repos/acme/two/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"pb1\"",
                    "-X",
                    "GET",
                    "repos/acme/two/pulls?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            ),
        );
        let execs = BTreeMap::from([("a", exec_a), ("b", exec_b)]);
        let exec_for = move |alias: &str| execs[alias].clone();

        // The interval is ten seconds, so the second pass of b can only come
        // from the wake; the test would time out otherwise.
        let (tx, rx) = mpsc::channel();
        let pollers = spawn_all(
            &config,
            tx,
            &exec_for,
            Duration::from_secs(10),
            Duration::from_secs(20),
        );

        assert_eq!(pollers.handles.len(), 2);
        let aliases: Vec<&str> = pollers.wake.keys().map(String::as_str).collect();
        assert_eq!(aliases, ["a", "b"]);

        for _ in 0..2 {
            match next_msg(&rx, "the first poll of each repository") {
                DaemonMsg::Polled { .. } => {}
                other => panic!("unexpected message: {other:?}"),
            }
        }

        pollers.wake["b"].send(()).unwrap();
        match next_msg(&rx, "the woken poll of b") {
            DaemonMsg::Polled { repo, .. } => assert_eq!(repo, "b"),
            other => panic!("the wake did not produce a Polled message: {other:?}"),
        }

        drop(pollers.wake);
        for handle in pollers.handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn daemon_msg_has_the_shutdown_variant() {
        assert_eq!(format!("{:?}", DaemonMsg::Shutdown), "Shutdown");
    }

    #[test]
    fn spawn_poller_returns_the_wake_sender_of_one_repository() {
        let mut config = two_repo_config();
        config.repos.get_mut("a").unwrap().owner_repo = "acme/one".to_string();
        let repo = config.repos.get("a").unwrap().clone();
        // The interval is ten seconds, so the second pass can only come
        // from the returned wake sender.
        let exec = Arc::new(
            pass_steps(
                ScriptExec::new(),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/one/issues?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"i1\""],
                        &format!("[{}]", issue_json(1)),
                    )),
                ),
                (
                    vec![
                        "api",
                        "-i",
                        "-X",
                        "GET",
                        "repos/acme/one/pulls?state=open&per_page=100&page=1",
                    ],
                    CmdOut::ok(response(
                        "HTTP/2 200",
                        &["etag: \"p1\""],
                        &format!("[{}]", pr_json(2, "aaa")),
                    )),
                ),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"i1\"",
                    "-X",
                    "GET",
                    "repos/acme/one/issues?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-H",
                    "If-None-Match: \"p1\"",
                    "-X",
                    "GET",
                    "repos/acme/one/pulls?state=open&per_page=100&page=1",
                ]),
                CmdOut::ok(response("HTTP/2 304", &[], "")),
            ),
        );
        let (tx, rx) = mpsc::channel();
        let (handle, wake) = spawn_poller_with(
            &repo,
            tx,
            exec,
            Duration::from_secs(10),
            Duration::from_secs(20),
        )
        .expect("the poller thread must start");

        let DaemonMsg::Polled { repo: alias, .. } = next_msg(&rx, "the first poll") else {
            panic!("the first message was not Polled");
        };
        assert_eq!(alias, "a");
        wake.send(()).unwrap();
        let DaemonMsg::Polled { repo: alias, .. } = next_msg(&rx, "the woken poll") else {
            panic!("the wake did not produce a Polled message");
        };
        assert_eq!(alias, "a");

        // The wake sender stays usable while the poller lives, and the
        // poller ends when the sender drops.
        drop(wake);
        handle.join().unwrap();
    }
}
