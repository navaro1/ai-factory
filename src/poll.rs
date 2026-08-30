//! Runs one poller thread per repository and feeds one inbound channel.
//!
//! Each poller fetches its repository through the [`GhClient`] and sends the
//! whole [`RepoSnapshot`] to the daemon on one shared channel. Between passes
//! the poller waits on its own wake channel, so the daemon can force an early
//! pass. A failed pass is reported and the poller backs off, at most five
//! minutes, so one broken repository never stops the others.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;

use crate::config::Config;
use crate::exec::{Exec, RealExec};
use crate::gh::GhClient;
use crate::model::RepoSnapshot;

/// The normal wait between two poll passes of one repository.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// The longest wait after repeated poll failures.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// One inbound message of the daemon event loop.
///
/// The pollers of this module send [`PollerOnline`][DaemonMsg::PollerOnline],
/// [`Polled`][DaemonMsg::Polled], and [`PollFailed`][DaemonMsg::PollFailed].
/// Later chunks add the variants of the runners, the trains, and the control
/// socket.
#[derive(Debug)]
pub enum DaemonMsg {
    /// The first message of each poller. The daemon stores `wake` and sends
    /// on it to force an early pass. Dropping the sender ends that poller
    /// after its current wait.
    PollerOnline {
        /// The repository alias of the poller.
        repo: String,
        /// The wake channel of that one poller.
        wake: Sender<()>,
    },
    /// One finished poll pass. `snapshot` holds the complete current state of
    /// the repository; the reader already merged the cached entries of the
    /// pages that answered 304 into it.
    Polled {
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

/// Spawn one poller thread per configured repository.
///
/// Each thread announces itself with [`DaemonMsg::PollerOnline`] and then
/// loops: fetch, send [`DaemonMsg::Polled`], wait [`POLL_INTERVAL`] on its
/// wake channel. A failed pass sends [`DaemonMsg::PollFailed`] and doubles
/// the wait, at most [`MAX_BACKOFF`]. Each thread runs until the daemon
/// drops its wake sender or the channel closes. The join handles follow the
/// config order of the repositories.
pub fn spawn_pollers(cfg: &Config, tx: Sender<DaemonMsg>) -> Vec<JoinHandle<()>> {
    let exec_for = |_alias: &str| Arc::new(RealExec) as Arc<dyn Exec>;
    spawn_all(cfg, tx, &exec_for, POLL_INTERVAL, MAX_BACKOFF)
}

/// Spawn every poller with an explicit [`Exec`] factory and waits.
///
/// The tests use it to inject a [`ScriptExec`][crate::exec::ScriptExec] and
/// short waits; production uses [`spawn_pollers`]. A repository whose thread
/// cannot start contributes no handle and sends [`DaemonMsg::PollFailed`].
fn spawn_all(
    cfg: &Config,
    tx: Sender<DaemonMsg>,
    exec_for: &dyn Fn(&str) -> Arc<dyn Exec>,
    interval: Duration,
    max_backoff: Duration,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for repo in cfg.repos.values() {
        if let Some(handle) = spawn_one(
            repo.alias.clone(),
            repo.owner_repo.clone(),
            exec_for(&repo.alias),
            tx.clone(),
            interval,
            max_backoff,
        ) {
            handles.push(handle);
        }
    }
    handles
}

/// Spawn the poller thread of one repository and create its wake channel.
///
/// Returns `None` when the thread cannot start or the daemon channel is
/// already closed; both cases send [`DaemonMsg::PollFailed`].
fn spawn_one(
    repo: String,
    owner_repo: String,
    exec: Arc<dyn Exec>,
    tx: Sender<DaemonMsg>,
    interval: Duration,
    max_backoff: Duration,
) -> Option<JoinHandle<()>> {
    let (wake_tx, wake_rx) = mpsc::channel();
    let spec = PollerSpec {
        repo: repo.clone(),
        owner_repo,
        exec,
        tx: tx.clone(),
        wake_tx,
        wake_rx,
        interval,
        max_backoff,
    };
    let name = format!("poll-{repo}");
    match thread::Builder::new()
        .name(name)
        .spawn(move || poller_loop(spec))
    {
        Ok(handle) => Some(handle),
        Err(error) => {
            let _ = tx.send(DaemonMsg::PollFailed {
                repo,
                error: format!("cannot start the poller thread: {error}"),
            });
            None
        }
    }
}

/// Everything one poller thread needs.
struct PollerSpec {
    /// The repository alias it reports under.
    repo: String,
    /// The `owner/name` slug it fetches.
    owner_repo: String,
    /// The command indirection it fetches through.
    exec: Arc<dyn Exec>,
    /// The daemon channel it reports on.
    tx: Sender<DaemonMsg>,
    /// The sender half of its wake channel; the poller hands it to the
    /// daemon with its first message.
    wake_tx: Sender<()>,
    /// The receiver half of its wake channel.
    wake_rx: Receiver<()>,
    /// The wait between two passes.
    interval: Duration,
    /// The cap the wait grows to after failures.
    max_backoff: Duration,
}

/// Run the poll loop of one repository until its wake channel disconnects.
fn poller_loop(spec: PollerSpec) {
    let PollerSpec {
        repo,
        owner_repo,
        exec,
        tx,
        wake_tx,
        wake_rx,
        interval,
        max_backoff,
    } = spec;
    let online = DaemonMsg::PollerOnline {
        repo: repo.clone(),
        wake: wake_tx,
    };
    if tx.send(online).is_err() {
        return;
    }
    let mut client = GhClient::new(exec.as_ref());
    let mut backoff = interval;
    loop {
        let failed = match fetch_repo(&mut client, &owner_repo) {
            Ok(snapshot) => {
                backoff = interval;
                let polled = DaemonMsg::Polled {
                    repo: repo.clone(),
                    snapshot,
                };
                if tx.send(polled).is_err() {
                    return;
                }
                false
            }
            Err(error) => {
                let failed = DaemonMsg::PollFailed {
                    repo: repo.clone(),
                    error: format!("{error:#}"),
                };
                if tx.send(failed).is_err() {
                    return;
                }
                true
            }
        };
        match wake_rx.recv_timeout(backoff) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            // The daemon dropped the wake sender; the poller stops.
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if failed {
            backoff = next_backoff(backoff, max_backoff);
        }
    }
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
    use std::time::Instant;

    use super::*;
    use crate::exec::{Call, CmdOut, ScriptExec};
    use crate::model::{Issue, Pr};

    const REPO: &str = "borsuk";
    const OWNER_REPO: &str = "acme/borsuk";
    const ISSUES_URL: &str = "repos/acme/borsuk/issues?state=open&per_page=100&page=1";
    const PULLS_URL: &str = "repos/acme/borsuk/pulls?state=open&per_page=100&page=1";

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

    /// One GitHub draft pull request object.
    fn pr_json(number: u64, sha: &str) -> String {
        format!(
            r#"{{"number":{number},"node_id":"prnode-{number}","title":"pr {number}","body":"","state":"open","labels":[],"draft":true,"head":{{"sha":"{sha}"}}}}"#
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

    /// Every message that arrives within `window`.
    fn drain(rx: &Receiver<DaemonMsg>, window: Duration) -> Vec<DaemonMsg> {
        let deadline = Instant::now() + window;
        let mut messages = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(msg) => messages.push(msg),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        messages
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
        let handle = spawn_one(
            REPO.to_string(),
            OWNER_REPO.to_string(),
            exec.clone(),
            tx,
            Duration::from_secs(10),
            Duration::from_secs(20),
        )
        .unwrap();

        let DaemonMsg::PollerOnline { repo, wake } = next_msg(&rx, "the online message") else {
            panic!("the first message was not PollerOnline");
        };
        assert_eq!(repo, REPO);
        let DaemonMsg::Polled {
            repo,
            snapshot: first,
        } = next_msg(&rx, "the first poll")
        else {
            panic!("the second message was not Polled");
        };
        assert_eq!(repo, REPO);
        assert_eq!(
            first,
            snapshot(vec![model_issue(1)], vec![model_pr(2, "aaa")])
        );

        wake.send(()).unwrap();
        let DaemonMsg::Polled {
            repo,
            snapshot: second,
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
        let exec = Arc::new(pass_steps(
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
        ));
        let (tx, rx) = mpsc::channel();
        let handle = spawn_one(
            REPO.to_string(),
            OWNER_REPO.to_string(),
            exec,
            tx,
            Duration::from_millis(20),
            Duration::from_millis(40),
        )
        .unwrap();

        let DaemonMsg::PollerOnline { wake, .. } = next_msg(&rx, "the online message") else {
            panic!("the first message was not PollerOnline");
        };
        let DaemonMsg::PollFailed { repo, error } = next_msg(&rx, "the first failure") else {
            panic!("the second message was not PollFailed");
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
    fn a_wake_reaches_only_its_own_repository() {
        let mut config = Config::parse(
            r#"
[stage.refine]
model = "m"
runner = "claude"

[stage.implement]
model = "m"
runner = "opencode"

[stage.review]
model = "m"
runner = "opencode"

[stage.release]
model = "m"
runner = "claude"

[repo.a]
path = "/repos/a"

[repo.b]
path = "/repos/b"
"#,
        )
        .unwrap();
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
        let handles = spawn_all(
            &config,
            tx,
            &exec_for,
            Duration::from_millis(30),
            Duration::from_millis(60),
        );
        assert_eq!(handles.len(), 2);

        let mut wakes = BTreeMap::new();
        let mut first_polls = BTreeMap::new();
        for _ in 0..4 {
            match next_msg(&rx, "the first messages of both pollers") {
                DaemonMsg::PollerOnline { repo, wake } => {
                    wakes.insert(repo, wake);
                }
                DaemonMsg::Polled { repo, snapshot } => {
                    first_polls.insert(repo, snapshot);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert_eq!(wakes.len(), 2);
        assert_eq!(
            first_polls["a"],
            snapshot(vec![model_issue(1)], vec![model_pr(2, "aaa")])
        );
        assert_eq!(
            first_polls["b"],
            snapshot(vec![model_issue(3)], vec![model_pr(5, "bbb")])
        );

        wakes["a"].send(()).unwrap();
        let mut saw_a_update = false;
        while !saw_a_update {
            match next_msg(&rx, "the woken poll of a") {
                DaemonMsg::Polled {
                    repo,
                    snapshot: snap,
                } if repo == "a" => {
                    assert_eq!(
                        snap,
                        snapshot(vec![model_issue(7)], vec![model_pr(2, "aaa")])
                    );
                    saw_a_update = true;
                }
                _ => {}
            }
        }

        // Repository b must never run a second successful pass: its script
        // holds one pass, so every later pass of b fails. A second Polled
        // from b would mean the wake of a reached b.
        for msg in drain(&rx, Duration::from_millis(300)) {
            if let DaemonMsg::Polled { repo, .. } = msg {
                assert_ne!(repo, "b", "the wake of a reached b");
            }
        }

        drop(wakes);
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn daemon_msg_has_the_shutdown_variant() {
        assert_eq!(format!("{:?}", DaemonMsg::Shutdown), "Shutdown");
    }
}
