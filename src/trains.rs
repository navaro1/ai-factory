//! The release queue and the release policies of one repository.
//!
//! Ready pull requests collect in a queue. A human can stack a subset with
//! the `release-stacked` label on GitHub. The label is the truth; the
//! in-memory copy is a cache that every poll rebuilds. A train fires by
//! policy or by hand, and only one train per repository is in flight.

use anyhow::{bail, Context, Result};

use crate::config::ReleasePolicy;
use crate::gh::GhClient;

/// The GitHub label that marks a pull request as part of the stacked batch.
///
/// This is the label the naming rules call `release-stacked`. It carries the
/// stack across a daemon restart, a crash, and a reboot, because GitHub, not
/// this process, holds it.
pub const STACKED_LABEL: &str = "release-stacked";

/// The release train of one repository: its queue, its stacked subset, and
/// the batch in flight.
///
/// The queue holds ready pull requests in arrival order. [`Train::stacked`]
/// mirrors the `release-stacked` labels on GitHub, and every poll rebuilds
/// it with [`Train::rebuild_stacked`]. The daemon persists
/// [`Train::last_fire_ms`] in `state.json` and restores it here after a
/// restart. Build a train with [`Train::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Train {
    /// The repository alias this train belongs to.
    pub repo: String,
    /// The ready pull request numbers, in arrival order.
    pub queue: Vec<u64>,
    /// The stacked subset, as the last poll or stack call saw it.
    pub stacked: Vec<u64>,
    /// The task id of the batch in flight, when one is.
    pub in_flight: Option<String>,
    /// When the train last fired, in milliseconds since the Unix epoch.
    pub last_fire_ms: Option<u64>,
    /// The batch the in-flight train fires. It lives only between
    /// [`Train::fire`] and [`Train::finish`], so `finish` knows the exact
    /// set to drain or return, even when the queue changed meanwhile.
    fired: Option<Vec<u64>>,
}

impl Train {
    /// An empty train for `repo`.
    pub fn new(repo: &str) -> Self {
        Train {
            repo: repo.to_string(),
            queue: Vec::new(),
            stacked: Vec::new(),
            in_flight: None,
            last_fire_ms: None,
            fired: None,
        }
    }

    /// Add a ready pull request to the queue, unless it is already there.
    pub fn enqueue(&mut self, pr: u64) {
        if !self.queue.contains(&pr) {
            self.queue.push(pr);
        }
    }

    /// Remove a pull request that closed, merged, or went back to draft.
    ///
    /// The number leaves both the queue and the stacked cache. The GitHub
    /// label of a still-open pull request stays, so the stack choice of the
    /// human survives a round trip through draft; the next rebuild applies
    /// it again once the pull request is ready.
    pub fn dequeue(&mut self, pr: u64) {
        self.queue.retain(|n| *n != pr);
        self.stacked.retain(|n| *n != pr);
    }

    /// Stack or unstack one pull request for the next batch.
    ///
    /// `on = true` adds the [`STACKED_LABEL`] label on GitHub and puts the
    /// number in the cache; `on = false` removes both. The cache update is
    /// optimistic: it happens at once on success, not on the next poll.
    /// The pull request must be in the queue, so a stacked set stays a
    /// subset of the queue. Stacking an already stacked pull request and
    /// unstacking an absent one make no label call.
    pub fn stack(&mut self, pr: u64, on: bool, owner_repo: &str, gh: &GhClient<'_>) -> Result<()> {
        if on {
            if !self.queue.contains(&pr) {
                bail!("cannot stack pr {pr}: it is not in the release queue");
            }
            if self.stacked.contains(&pr) {
                return Ok(());
            }
            gh.add_label(owner_repo, pr, STACKED_LABEL)
                .with_context(|| format!("cannot add {STACKED_LABEL} to pr {pr}"))?;
            self.stacked.push(pr);
        } else {
            if !self.stacked.contains(&pr) {
                return Ok(());
            }
            gh.remove_label(owner_repo, pr, STACKED_LABEL)
                .with_context(|| format!("cannot remove {STACKED_LABEL} from pr {pr}"))?;
            self.stacked.retain(|n| *n != pr);
        }
        Ok(())
    }

    /// Rebuild the stacked cache from the labels the last poll saw.
    ///
    /// `labeled` holds the pull request numbers that carry the
    /// [`STACKED_LABEL`] label. Only numbers that are also in the queue
    /// stay, so a draft or closed pull request never fires even when its
    /// label is still on GitHub. The order follows the queue.
    pub fn rebuild_stacked(&mut self, labeled: &[u64]) {
        self.stacked = self
            .queue
            .iter()
            .copied()
            .filter(|pr| labeled.contains(pr))
            .collect();
    }

    /// The batch the next fire would send: the stacked subset when one is
    /// stacked, otherwise the whole queue.
    pub fn fired_set(&self) -> Vec<u64> {
        if self.stacked.is_empty() {
            self.queue.clone()
        } else {
            self.stacked.clone()
        }
    }

    /// Whether the train may fire now, and with which pull requests.
    ///
    /// - `Manual` never fires on its own.
    /// - `Interval` fires when the queue is not empty and `minutes` passed
    ///   since the last fire. A train that never fired is due at once.
    /// - `Threshold` fires when the queue length reaches `count`.
    ///
    /// A train that is in flight never fires. The caller sends the returned
    /// set to [`Train::fire`]. This method changes nothing, so the daemon
    /// may call it after every message.
    pub fn should_fire(&self, policy: &ReleasePolicy, now_ms: u64) -> Option<Vec<u64>> {
        if self.in_flight.is_some() || self.queue.is_empty() {
            return None;
        }
        let due = match policy {
            ReleasePolicy::Manual => false,
            ReleasePolicy::Interval { minutes } => self.interval_due(*minutes, now_ms),
            ReleasePolicy::Threshold { count } => self.queue.len() >= *count,
        };
        due.then(|| self.fired_set())
    }

    /// Whether `minutes` passed since the last fire.
    fn interval_due(&self, minutes: u64, now_ms: u64) -> bool {
        let interval_ms = minutes.saturating_mul(60_000);
        match self.last_fire_ms {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= interval_ms,
        }
    }

    /// The moment the event loop must wake for this train, or `None` when
    /// the loop may block.
    ///
    /// Only an `Interval` policy with a non-empty queue and no batch in
    /// flight produces a deadline. It is `last_fire_ms + interval`, never
    /// earlier than `now_ms`, and it matches the moment [`Train::should_fire`]
    /// first says the train is due. `Manual` and `Threshold` return `None`:
    /// an arriving pull request or a human action wakes the loop anyway.
    pub fn next_deadline_ms(&self, policy: &ReleasePolicy, now_ms: u64) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        let minutes = match policy {
            ReleasePolicy::Interval { minutes } => *minutes,
            ReleasePolicy::Manual | ReleasePolicy::Threshold { .. } => return None,
        };
        if self.queue.is_empty() {
            return None;
        }
        let interval_ms = minutes.saturating_mul(60_000);
        let fire_at = self
            .last_fire_ms
            .map_or(now_ms, |last| last.saturating_add(interval_ms));
        Some(fire_at.max(now_ms))
    }

    /// Start a train with `prs` and return its task id.
    ///
    /// The id follows the naming rules: `<repo>/release-p<n>`, where `n` is
    /// the lowest number in the batch, so a retry of the same batch reuses
    /// the same id. The call records the batch, marks the train in flight,
    /// and stamps [`Train::last_fire_ms`] with `now_ms`. The daemon creates
    /// the task itself under the returned id; this method does not.
    pub fn fire(&mut self, prs: &[u64], now_ms: u64) -> Result<String> {
        if let Some(id) = &self.in_flight {
            bail!("a train is already in flight as {id}");
        }
        let first = match prs.iter().min() {
            Some(first) => *first,
            None => bail!("cannot fire an empty train"),
        };
        let id = format!("{}/release-p{first}", self.repo);
        self.in_flight = Some(id.clone());
        self.fired = Some(prs.to_vec());
        self.last_fire_ms = Some(now_ms);
        Ok(id)
    }

    /// Close the batch in flight and return it.
    ///
    /// On success the batch leaves the queue and the stacked cache, and the
    /// [`STACKED_LABEL`] labels go off GitHub. On failure the pull requests
    /// stay in the queue and keep their labels, so a retry fires the same
    /// set again. Each pull request is cleared in turn: label first, then
    /// cache entry. A label call that fails stops the drain with an error;
    /// the rest of the batch stays queued, and the next poll rebuilds the
    /// cache from GitHub. `finish` without a batch in flight is a no-op.
    pub fn finish(&mut self, ok: bool, owner_repo: &str, gh: &GhClient<'_>) -> Result<Vec<u64>> {
        self.in_flight = None;
        let Some(fired) = self.fired.take() else {
            return Ok(Vec::new());
        };
        if !ok {
            return Ok(fired);
        }
        for pr in &fired {
            gh.remove_label(owner_repo, *pr, STACKED_LABEL)
                .with_context(|| format!("cannot remove {STACKED_LABEL} from pr {pr}"))?;
            self.queue.retain(|n| *n != *pr);
            self.stacked.retain(|n| *n != *pr);
        }
        Ok(fired)
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
    fn response(status_line: &str, body: &str) -> String {
        format!("{status_line}\r\n\r\n{body}")
    }

    /// A train for `borsuk` with the given queue and no history.
    fn train(queue: &[u64]) -> Train {
        let mut t = Train::new("borsuk");
        for pr in queue {
            t.enqueue(*pr);
        }
        t
    }

    #[test]
    fn enqueue_adds_a_ready_pr_once() {
        let mut t = Train::new("borsuk");
        t.enqueue(1);
        t.enqueue(2);
        t.enqueue(1);
        assert_eq!(t.queue, vec![1, 2]);
    }

    #[test]
    fn dequeue_removes_a_pr_from_the_queue_and_the_cache() {
        let mut t = train(&[1, 2, 3]);
        t.stacked = vec![2];
        t.dequeue(2);
        assert_eq!(t.queue, vec![1, 3]);
        assert!(t.stacked.is_empty());
        t.dequeue(9);
        assert_eq!(t.queue, vec![1, 3], "an absent number is a no-op");
    }

    #[test]
    fn a_threshold_policy_fires_when_the_count_is_reached() {
        let mut t = train(&[1, 2]);
        let policy = ReleasePolicy::Threshold { count: 3 };
        assert_eq!(t.should_fire(&policy, 1_000), None);
        t.enqueue(3);
        assert_eq!(t.should_fire(&policy, 1_000), Some(vec![1, 2, 3]));
    }

    #[test]
    fn a_threshold_train_does_not_fire_again_while_in_flight() {
        let mut t = train(&[1, 2, 3]);
        let policy = ReleasePolicy::Threshold { count: 3 };
        let batch = t.should_fire(&policy, 1_000).unwrap();
        let id = t.fire(&batch, 1_000).unwrap();
        assert_eq!(id, "borsuk/release-p1");
        t.enqueue(4);
        assert_eq!(t.should_fire(&policy, 2_000), None);
        let mut exec = ScriptExec::new();
        for pr in [1, 2, 3] {
            exec = exec.expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    &format!("repos/acme/borsuk/issues/{pr}/labels/release-stacked"),
                ]),
                CmdOut::ok(response("HTTP/2 204", "")),
            );
        }
        let client = GhClient::new(&exec);
        t.finish(true, "acme/borsuk", &client).unwrap();
        assert_eq!(exec.calls().len(), 3);
        assert_eq!(t.queue, vec![4], "only the fired batch drains");
        assert_eq!(t.should_fire(&policy, 3_000), None);
    }

    #[test]
    fn an_interval_policy_fires_only_at_or_after_its_deadline() {
        let mut t = train(&[1]);
        t.last_fire_ms = Some(0);
        let policy = ReleasePolicy::Interval { minutes: 1 };
        assert_eq!(t.should_fire(&policy, 59_999), None);
        assert_eq!(t.should_fire(&policy, 60_000), Some(vec![1]));
    }

    #[test]
    fn an_interval_train_that_never_fired_is_due_now() {
        let t = train(&[1]);
        let policy = ReleasePolicy::Interval { minutes: 30 };
        assert_eq!(t.should_fire(&policy, 5), Some(vec![1]));
        assert_eq!(t.next_deadline_ms(&policy, 5), Some(5));
    }

    #[test]
    fn the_next_deadline_is_the_interval_fire_moment() {
        let mut t = train(&[1]);
        t.last_fire_ms = Some(0);
        let interval = ReleasePolicy::Interval { minutes: 1 };
        assert_eq!(t.next_deadline_ms(&interval, 30_000), Some(60_000));
        assert_eq!(t.next_deadline_ms(&interval, 60_000), Some(60_000));
        assert_eq!(t.next_deadline_ms(&interval, 70_000), Some(70_000));
        let threshold = ReleasePolicy::Threshold { count: 1 };
        assert_eq!(t.next_deadline_ms(&threshold, 30_000), None);
        assert_eq!(t.next_deadline_ms(&ReleasePolicy::Manual, 30_000), None);
        t.fire(&[1], 70_000).unwrap();
        assert_eq!(t.next_deadline_ms(&interval, 30_000), None);
    }

    #[test]
    fn an_interval_train_with_an_empty_queue_has_no_deadline() {
        let mut t = Train::new("borsuk");
        t.last_fire_ms = Some(0);
        let policy = ReleasePolicy::Interval { minutes: 1 };
        assert_eq!(t.next_deadline_ms(&policy, 30_000), None);
        assert_eq!(t.should_fire(&policy, 90_000), None);
    }

    #[test]
    fn manual_never_fires_but_fire_works() {
        let mut t = train(&[1, 2]);
        let policy = ReleasePolicy::Manual;
        assert_eq!(t.should_fire(&policy, 100_000), None);
        assert_eq!(t.next_deadline_ms(&policy, 100_000), None);
        let id = t.fire(&[2], 5_000).unwrap();
        assert_eq!(id, "borsuk/release-p2");
        assert_eq!(t.in_flight.as_deref(), Some("borsuk/release-p2"));
        assert_eq!(t.last_fire_ms, Some(5_000));
    }

    #[test]
    fn a_stacked_subset_fires_instead_of_the_whole_queue() {
        let mut t = train(&[1, 2, 3, 4]);
        t.stacked = vec![2, 4];
        let policy = ReleasePolicy::Threshold { count: 1 };
        assert_eq!(t.should_fire(&policy, 1_000), Some(vec![2, 4]));
        assert_eq!(t.fired_set(), vec![2, 4]);
    }

    #[test]
    fn a_failed_train_returns_its_prs_and_a_retry_reuses_the_same_set() {
        let mut t = train(&[1, 2, 3]);
        t.stacked = vec![2, 3];
        let policy = ReleasePolicy::Threshold { count: 3 };
        let batch = t.should_fire(&policy, 1_000).unwrap();
        assert_eq!(batch, vec![2, 3]);
        t.fire(&batch, 1_000).unwrap();
        let exec = ScriptExec::new();
        let client = GhClient::new(&exec);
        let finished = t.finish(false, "acme/borsuk", &client).unwrap();
        assert_eq!(finished, vec![2, 3]);
        assert_eq!(t.in_flight, None);
        assert_eq!(t.queue, vec![1, 2, 3], "the batch is back in the queue");
        assert_eq!(exec.calls().len(), 0, "failure makes no label call");
        let again = t.should_fire(&policy, 2_000).unwrap();
        assert_eq!(again, vec![2, 3], "the retry reuses the same set");
    }

    #[test]
    fn stack_adds_the_github_label_and_updates_the_cache() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/2/labels",
                "-f",
                "labels[]=release-stacked",
            ]),
            CmdOut::ok(response("HTTP/2 200", "{}")),
        );
        let client = GhClient::new(&exec);
        let mut t = train(&[1, 2]);
        t.stack(2, true, "acme/borsuk", &client).unwrap();
        assert_eq!(t.stacked, vec![2]);
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn unstack_removes_the_github_label_and_the_cache_entry() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "DELETE",
                "repos/acme/borsuk/issues/2/labels/release-stacked",
            ]),
            CmdOut::ok(response("HTTP/2 200", "{}")),
        );
        let client = GhClient::new(&exec);
        let mut t = train(&[1, 2]);
        t.stacked = vec![2];
        t.stack(2, false, "acme/borsuk", &client).unwrap();
        assert!(t.stacked.is_empty());
    }

    #[test]
    fn stacking_twice_makes_one_label_call() {
        let exec = ScriptExec::new().expect(
            gh(&[
                "api",
                "-i",
                "-X",
                "POST",
                "repos/acme/borsuk/issues/2/labels",
                "-f",
                "labels[]=release-stacked",
            ]),
            CmdOut::ok(response("HTTP/2 200", "{}")),
        );
        let client = GhClient::new(&exec);
        let mut t = train(&[2]);
        t.stack(2, true, "acme/borsuk", &client).unwrap();
        t.stack(2, true, "acme/borsuk", &client).unwrap();
        assert_eq!(t.stacked, vec![2]);
        assert_eq!(exec.calls().len(), 1);
    }

    #[test]
    fn unstacking_an_absent_pr_is_a_no_op() {
        let exec = ScriptExec::new();
        let client = GhClient::new(&exec);
        let mut t = train(&[1]);
        t.stack(1, false, "acme/borsuk", &client).unwrap();
        assert_eq!(exec.calls().len(), 0);
    }

    #[test]
    fn stacking_a_pr_that_is_not_queued_is_refused() {
        let exec = ScriptExec::new();
        let client = GhClient::new(&exec);
        let mut t = Train::new("borsuk");
        let err = t.stack(9, true, "acme/borsuk", &client).unwrap_err();
        assert!(err.to_string().contains("not in the release queue"));
        assert_eq!(exec.calls().len(), 0, "no label call happens");
    }

    #[test]
    fn rebuild_stacked_keeps_only_queued_prs_in_queue_order() {
        let mut t = train(&[1, 2, 3]);
        t.rebuild_stacked(&[3, 9, 1]);
        assert_eq!(t.stacked, vec![1, 3], "9 is not queued and drops out");
    }

    #[test]
    fn a_successful_train_drains_the_batch_and_clears_the_labels() {
        let exec = ScriptExec::new()
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/2/labels/release-stacked",
                ]),
                CmdOut::ok(response("HTTP/2 200", "{}")),
            )
            .expect(
                gh(&[
                    "api",
                    "-i",
                    "-X",
                    "DELETE",
                    "repos/acme/borsuk/issues/5/labels/release-stacked",
                ]),
                CmdOut::ok(response("HTTP/2 200", "{}")),
            );
        let client = GhClient::new(&exec);
        let mut t = train(&[1, 2, 5]);
        t.stacked = vec![2, 5];
        let batch = t.fired_set();
        t.fire(&batch, 1_000).unwrap();
        let finished = t.finish(true, "acme/borsuk", &client).unwrap();
        assert_eq!(finished, vec![2, 5]);
        assert_eq!(t.queue, vec![1]);
        assert!(t.stacked.is_empty());
        assert_eq!(t.in_flight, None);
        assert_eq!(exec.calls().len(), 2);
    }

    #[test]
    fn finish_without_a_train_touches_nothing() {
        let exec = ScriptExec::new();
        let client = GhClient::new(&exec);
        let mut t = train(&[1]);
        let finished = t.finish(true, "acme/borsuk", &client).unwrap();
        assert!(finished.is_empty());
        assert_eq!(exec.calls().len(), 0);
    }

    #[test]
    fn a_second_fire_while_in_flight_is_refused() {
        let mut t = train(&[1, 2]);
        t.fire(&[1], 1_000).unwrap();
        let err = t.fire(&[2], 2_000).unwrap_err();
        assert!(err.to_string().contains("already in flight"));
    }

    #[test]
    fn firing_an_empty_set_is_refused() {
        let mut t = Train::new("borsuk");
        let err = t.fire(&[], 1_000).unwrap_err();
        assert!(err.to_string().contains("empty"));
        assert_eq!(t.in_flight, None);
    }

    #[test]
    fn the_task_id_names_the_lowest_pr_of_the_batch() {
        let mut t = train(&[4, 2, 7]);
        let id = t.fire(&[4, 2, 7], 1_000).unwrap();
        assert_eq!(id, "borsuk/release-p2");
    }
}
