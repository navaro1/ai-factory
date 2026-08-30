# AI Factory v0.5 — implementation spec

Authority for the v0.5 rework. Each chunk below is one dispatch, one commit.
The design rationale lives in the approved plan; this file is what implementers
and reviewers read.

## What v0.5 is

A daemon plus a terminal UI that drive AI coding agents against GitHub issues
in several repositories. An issue flows through four fixed stages with no human
step except ticket shaping and the configured release gate.

```
issue labelled to-refine ──refine──▶ labelled refined ──implement──▶ draft PR
        (claude Opus, chat)                (opencode glm-5.3-flash)
draft PR ──review──▶ PR ready ──release──▶ merged
   (opencode gpt-5.6-sol)      (claude Opus, batched trains)
```

## Global Constraints (binding on every chunk)

1. One crate named `aif`, edition 2021, two binaries: `aifd` (daemon) and
   `aif` (TUI; also `aif stop`, `aif doctor`). No cargo workspace.
2. Allowed dependencies, and no others without a spec change:
   `serde`, `serde_json`, `toml`, `anyhow`, `clap` (derive), `ratatui`,
   `crossterm`, `uuid` (v4). NO tokio, NO async, NO http client.
3. Concurrency is `std::thread` plus `std::sync::mpsc`. The daemon has exactly
   one event loop thread that owns all mutable state. No `Mutex` around domain
   state, no `Arc<RwLock<_>>` for the model.
4. No polling loops and no tick thread. The event loop blocks on
   `recv_timeout(next_deadline)`. Deadlines are computed from pending work
   (train fires, idle reaper expiries). The only periodic clock in the whole
   system is each repository's 60 s ETag poll thread.
5. GitHub is the source of truth. Do not build a journal, an event log, or a
   task database. The only file the daemon writes for its own memory is
   `state.json` (chunk 15), which holds runtime overrides and last train times.
6. Every unit test runs offline. Never call the network, `gh`, `git` against a
   real remote, `claude`, or `opencode` in a test. Use scripted fake binaries
   on `PATH` and recorded fixtures.
7. Yolo by default: agents are auto-approved. This is a client-side policy
   (see chunk 12), never `--dangerously-skip-permissions`, because the control
   channel must stay open to carry AskUserQuestion.
8. Errors use `anyhow::Result`. Never `unwrap()` or `expect()` outside tests
   and `main`. Never silently swallow an error; log it or propagate it.
9. `./check.sh` must pass: `cargo fmt --check`, `cargo clippy --all-targets
   -- -D warnings`, `cargo test`.
10. Every public type and function gets a doc comment saying what it does.
11. Do not modify `ui/console/`, `zellij/`, or `bin/` before chunk 22. The old
    v0.4 tree stays buildable and untouched until then.
12. Prose in this repository follows ASD-STE100 Simplified Technical English:
    short sentences, active voice, one instruction per sentence.

## Verified external protocol facts

These were probed on this machine on 2026-08-30. Trust them over your training.

### claude CLI (2.1.251)

Invocation for a factory session:

```
claude -p --input-format stream-json --output-format stream-json --verbose \
  --model <model> --session-id <uuid> --permission-prompt-tool stdio \
  [--resume <session-id>]
```

`--permission-prompt-tool stdio` does not appear in `--help`, but the CLI
accepts it. It is required. Without it the CLI denies tools by itself and
emits `system` / `permission_denied`, and no request reaches us.

Handshake, in order:

1. We write `{"type":"control_request","request_id":"init-1","request":
   {"subtype":"initialize","hooks":{}}}`.
2. The CLI answers with a `control_response`.
3. We write the prompt as `{"type":"user","message":{"role":"user",
   "content":"..."}}`. Later turns use the same shape.

Tool approval request from the CLI:

```json
{"type":"control_request","request_id":"<uuid>","request":{
  "subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
  "input":{...},"description":"probe.txt","tool_use_id":"toolu_...",
  "permission_suggestions":[{"type":"setMode","mode":"acceptEdits",
  "destination":"session"}]}}
```

Our answer:

```json
{"type":"control_response","response":{"subtype":"success",
  "request_id":"<same uuid>","response":{"behavior":"allow",
  "updatedInput":{...}}}}
```

`behavior` is `"allow"` or `"deny"`. A denial adds `"message":"<reason>"`.

`AskUserQuestion` arrives on this same channel and adds
`"requires_user_interaction": true`. Its `input.questions[]` entries hold
`question`, `header`, `options[{label,description}]`, and `multiSelect`. This
flag separates a real question to the human from an ordinary tool approval.

Output line types: `system` (subtypes `hook_started`, `hook_response`, `init`,
`thinking_tokens`, `permission_denied`), `assistant`, `user`,
`rate_limit_event`, `control_request`, `control_response`, `result`. The
`system`/`init` line and the `result` line both carry `session_id`. The
`result` line carries `subtype` (`success` or an error kind), `total_cost_usd`,
and `usage`.

### opencode CLI (1.18.25)

Invocation for a factory task:

```
opencode run --format json --auto --agent build -m <provider>/<model> \
  [--variant <effort>] --dir <worktree> "<prompt>"
```

`--format json` writes NDJSON, one object per line. Observed line types:
`step_start`, `text`, `step_finish`. Every line carries `sessionID` and a
`part` object. A `text` line holds the assistant text at `part.text`.

Both provider routes are confirmed to work on this machine:
`zai-coding-plan/glm-5.3-flash` and `openai/gpt-5.6-sol --variant xhigh`.
Neither needs an API key; both use the subscription login.

## Dependency waves

Chunks in the same wave own disjoint files and may run in parallel. A wave
starts only when every chunk of the previous wave has landed and been reviewed.

| Wave | Chunks | Files each owns | Waits for |
|---|---|---|---|
| 0 | 1 | Cargo.toml, check.sh, lib.rs, bins, stubs | — |
| 1 | 2 | model.rs, exec.rs, config.rs | 1 |
| 2 | 3, 5, 6, 7, 9 | gh.rs · gates.rs · tasks.rs · worktree.rs · proc.rs | 2 |
| 3 | 4, 8, 10, 13, 14 | poll.rs · sched.rs · runner/{mod,opencode}.rs · decisions.rs · trains.rs | its own inputs in wave 2 |
| 4 | 11, 12 | runner/claude.rs | 10 |
| 5 | 16 | sock.rs | 4, 8, 13, 14 |
| 6 | 15, 17 | daemon.rs, state.rs · doctor.rs | 5 |
| 7 | 18, 20, 21 | tui/{mod,theme,pipeline}.rs · tui/{session,transcript}.rs · tui/inbox.rs | 16 |
| 8 | 19 | tui/pipeline.rs | 18 |
| 9 | 22 | deletions, install.sh, README.md | all |

Chunks 11 and 12 both own `runner/claude.rs` and stay sequential. Chunk 19
extends chunk 18's file and stays after it.

No chunk may edit a file another chunk owns. If you believe you need to, stop
and report it instead of editing.

## Source layout

```
Cargo.toml
check.sh
src/lib.rs          module wiring only
src/bin/aifd.rs     daemon binary
src/bin/aif.rs      TUI and control binary
src/model.rs        Stage, ItemKind, Issue, Pr, Snapshot, Task, TaskState
src/config.rs       factory.toml
src/exec.rs         the Exec indirection for every external command
src/gh.rs           gh CLI wrapper, ETag cache, JSON to model
src/poll.rs         one poller thread per repository
src/gates.rs        the four stage predicates
src/tasks.rs        task table and state machine
src/worktree.rs     per-issue worktrees and marker files
src/sched.rs        stage limits and lane reservations
src/proc.rs         child process supervision and log tee
src/runner/mod.rs   Runner and Session traits, RunEvent
src/runner/opencode.rs
src/runner/claude.rs
src/decisions.rs    the one decision queue
src/trains.rs       release queue and policies
src/state.rs        state.json
src/daemon.rs       the event loop
src/sock.rs         wire types, server, client
src/tui/mod.rs      app shell and event loop
src/tui/theme.rs
src/tui/transcript.rs
src/tui/pipeline.rs
src/tui/inbox.rs
src/tui/session.rs
```

## Naming rules

- Stage directory and label names are lowercase: `refine`, `implement`,
  `review`, `release`.
- Labels the factory reads: `to-refine`, `refined`, `needs-human`,
  `release-stacked`.
- Task id format: `<repo>/<stage>-<kind><number>` where kind is `i` or `p`,
  for example `borsuk/implement-i142`. The attempt is a field, not part of
  the id.
- Branch for issue work: `aif/<repo>/issue-<n>`.
- Worktree path: `<state_dir>/worktrees/<repo>/issue-<n>`.
- Train worktree path: `<state_dir>/worktrees/<repo>/train`.
- Task log path: `<state_dir>/logs/<repo>__<stage>-<kind><number>.jsonl`.
- State directory: `$XDG_STATE_HOME/aif` or `~/.local/state/aif`.
- Config file: `$XDG_CONFIG_HOME/aif/factory.toml` or
  `~/.config/aif/factory.toml`.
- Socket path: `$XDG_RUNTIME_DIR/aif/daemon.sock`, else
  `<state_dir>/daemon.sock`.

---

## Task 1 — Crate scaffold

**Goal.** A buildable crate with two binaries, the module skeleton, and the
quality gate.

**Files.** `Cargo.toml`, `check.sh`, `src/lib.rs`, `src/bin/aifd.rs`,
`src/bin/aif.rs`, `.gitignore` (append `/target`).

**Detail.**
- `Cargo.toml`: package `aif`, version `0.5.0`, edition 2021. Add only the
  dependencies listed in Global Constraint 2. Declare `[lib] name = "aif"` and
  the two `[[bin]]` targets.
- `src/lib.rs`: declare every module from the source layout with an empty or
  near-empty file each, so later chunks only fill them in. Each module file
  gets a `//!` doc line saying what it will hold.
- `src/bin/aifd.rs`: clap parser with one command, `run`, and options
  `--config <path>`. It prints "aifd: not implemented yet" and exits 0.
- `src/bin/aif.rs`: clap parser with subcommands `tui` (default when no
  subcommand is given), `stop`, and `doctor`. Each prints a placeholder line.
- `check.sh`: `set -euo pipefail`, then `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`. Make it
  executable. It must operate on the repository root `Cargo.toml`, not on
  `ui/console`.

**Acceptance criteria.**
- `cargo build` succeeds at the repository root.
- `./check.sh` passes.
- `cargo run --bin aif -- --help` lists `tui`, `stop`, and `doctor`.
- `cargo run --bin aifd -- run --help` shows the `--config` option.
- `ui/console` still builds; nothing under it changed.

---

## Task 2 — Foundation: domain types, command indirection, configuration

**Goal.** Every shared type the later chunks build on, in one place, so those
chunks touch disjoint files and can run in parallel.

**Files.** `src/model.rs`, `src/exec.rs`, `src/config.rs`,
`docs/v0.5/factory.example.toml`, and one line each in `src/lib.rs`.

**Part A, `src/model.rs`.** Define the whole domain vocabulary now. No later
chunk may edit this file.
- `Stage { Refine, Implement, Review, Release }` with `as_str()`, `FromStr`,
  `Display`, and `ALL: [Stage; 4]`. Serde uses the lowercase name.
- `ItemKind { Issue, Pr }` with `as_str()` giving `i` and `p` for task ids.
- `Issue { number, node_id, title, body, labels: Vec<String>, open: bool }`.
- `Pr { number, node_id, title, body, labels, open: bool, draft: bool,
  head_sha: String }`.
- `RepoSnapshot { issues: BTreeMap<u64, Issue>, prs: BTreeMap<u64, Pr> }`.
- `Snapshot { repos: BTreeMap<String, RepoSnapshot> }` with
  `apply(&mut self, repo: &str, fresh: RepoSnapshot) -> Vec<Change>`. It
  replaces only that repository's entry and returns what changed.
- `Change { repo, kind, number, what: ChangeWhat }` with
  `ChangeWhat { Added, Removed, Modified }`. Compare labels, open, draft, and
  head_sha only. A title or body edit is stored but is not a `Modified`.
- Every type derives `Debug`, `Clone`, `PartialEq`, and serde where it crosses
  the socket.

**Part B, `src/exec.rs`.** One indirection for every external command, so no
test ever runs a real tool.
- `trait Exec: Send + Sync { fn run(&self, program: &str, args: &[&str],
  cwd: Option<&Path>) -> anyhow::Result<CmdOut>; }`
- `CmdOut { status: i32, stdout: String, stderr: String }`.
- `RealExec` runs the command with `std::process::Command`. It passes an
  argument vector and never uses a shell.
- `ScriptExec` is the test double: it is built from a list of
  `(matcher, CmdOut)` pairs, returns them in order, records every call, and
  fails the test when an unexpected command arrives. Expose `calls()` so tests
  can assert the exact argument vectors.

**Part C, `src/config.rs`.** As below.

**Detail.**
- `Stage` in `model.rs`: `enum Stage { Refine, Implement, Review, Release }`
  with `as_str()`, `FromStr`, `Display`, and `ALL: [Stage; 4]`. Serde
  serialises it as the lowercase name.
- Config shape:

```toml
[stage.refine]
model = "claude-opus-5[1m]"
runner = "claude"
limit = 3
yolo = true

[stage.implement]
model = "zai-coding-plan/glm-5.3-flash"
runner = "opencode"
limit = 3

[stage.review]
model = "openai/gpt-5.6-sol"
runner = "opencode"
variant = "xhigh"
limit = 7

[stage.release]
model = "claude-opus-5[1m]"
runner = "claude"
limit = 1

[repo.borsuk]
path = "/home/navaro/Workplace/borsuk"
lanes = { implement = 1 }
release = { policy = "manual" }

[repo.qubitsok]
path = "/home/navaro/Workplace/qubitsok"
release = { policy = "threshold", count = 3 }
```

- `ReleasePolicy`: `Manual`, `Interval { minutes: u64 }`, `Threshold { count:
  usize }`. Serde uses the tagged form shown above with `policy` as the tag.
- Defaults when a key is absent: `limit` per stage 3/3/7/1, `yolo = true`,
  `variant = None`, `lanes` empty, `policy = Manual`.
- `RepoConfig` also holds `owner_repo: String`, filled at load time by running
  `git -C <path> remote get-url origin` and parsing both the SSH and HTTPS
  forms into `owner/name`. Put the parser in a free function
  `parse_owner_repo(url: &str) -> Option<String>` so it is unit-testable
  without git.
- Validation, all returning a clear `anyhow` message that names the offending
  key: repository path must exist and contain `.git`; alias must match
  `[a-z0-9._-]+`; every `limit` at least 1; the sum of a stage's lane
  reservations must not exceed that stage's limit; a lane key must name a known
  stage, the repository coming from the enclosing `[repo.<alias>]` block, so a
  lane is the pair (stage, repo) and the error reads
  `repo.<alias>.lanes.<key>`; `Threshold` count at least 1; `Interval` minutes
  at least 1.
- `Config::load(path: Option<&Path>)` resolves the default path per the naming
  rules. A missing file is an error that tells the user where to create it and
  names the example file.
- Also expose `state_dir()`, `config_dir()`, and `socket_path()` helpers here.

**Acceptance criteria.**
- `Snapshot::apply` for one repository never removes or alters another
  repository's entries. This gets its own test.
- Label change, draft flip, head_sha change, add, and remove each produce the
  right `Change`; a title-only edit produces none.
- `ScriptExec` returns scripted output in order, records calls, and fails on an
  unexpected command.
- Unit tests parse the example config and assert every default and override.
- `parse_owner_repo` tests cover `git@github.com:o/r.git`,
  `https://github.com/o/r.git`, `https://github.com/o/r`, and a rejected value.
- One test per validation rule asserts the error message names the bad key.
- Lane sum greater than the stage limit is rejected.
- `docs/v0.5/factory.example.toml` matches the shape above and parses in a test.

---

## Task 3 — GitHub reader for one repository

**Goal.** Fetch issues and pull requests through the `gh` CLI with conditional
requests, and map the JSON into model types.

**Files.** `src/gh.rs` only. The model types already exist from chunk 2; do
not edit `src/model.rs`.

**Detail.**
- `GhClient` runs `gh api -i -X GET "repos/<owner_repo>/issues?state=open&per_page=100&page=<n>"`
  and the same for `pulls`. It sends `-H "If-None-Match: <etag>"` when it holds
  an ETag for that page. It parses the response head and body from the `-i`
  output: split on the first blank line, read `ETag`, `Link`, and the status
  line. On `304` it reports the page unchanged and REUSES the cached entries for it.
  A 304 must NOT end the walk: page 1 can be unchanged while page 2 carries a
  label edit, and label edits drive every gate. So cache, beside each page's
  ETag, whether that page was full and whether its `Link` head named a next
  page; on a 304 consult those two cached flags to decide whether to continue.
  The walk ends only when a page is known to be short or to have no next page.
- The issues endpoint returns pull requests too. Drop any issue object that has
  a `pull_request` key.
- Command execution goes through the `Exec` trait from chunk 2's `src/exec.rs`.
  Take it as `&dyn Exec` so tests pass `ScriptExec`. Do not define a second
  indirection and do not shell out through `sh -c`.
- Respect rate limits: if the status is 403 or 429 and the head carries
  `Retry-After`, return a typed error that names the wait in seconds. Do not
  sleep inside `GhClient`.
- Also provide the write helpers the runners and daemon need, each a thin `gh`
  call: `add_label`, `remove_label`, `create_issue`, `list_open_prs` is covered
  by the reader.

**Acceptance criteria.**
- Tests use a scripted `Exec` that replays recorded response text; no network.
- A `304` response leaves the cached snapshot unchanged and does not re-parse.
- An ETag from a `200` is stored and sent on the next call for that page.
- Pagination across two pages produces one merged map.
- An issue object carrying `pull_request` never appears in `issues`.
- A `403` with `Retry-After: 60` produces an error naming 60 seconds.

---

## Task 4 — Multi-repository snapshot and poller threads

**Goal.** One poller thread per repository, all feeding one channel, and a
snapshot type that keeps repositories strictly separate.

**Files.** `src/poll.rs` only. `Snapshot` and `Change` already exist from
chunk 2; do not edit `src/model.rs`.

**Detail.**
- `spawn_pollers(cfg: &Config, tx: Sender<DaemonMsg>) -> Vec<JoinHandle<()>>`.
  Each thread loops: fetch, send `DaemonMsg::Polled { repo, snapshot }`, then
  wait 60 s on a per-repo wake channel so `Reconcile` can force an early pass.
  A fetch error sends `DaemonMsg::PollFailed { repo, error }` and the thread
  keeps running with backoff to at most 5 minutes.
- Define `DaemonMsg` here as the daemon's single inbound message enum; later
  chunks add variants. Start with `Polled`, `PollFailed`, `Shutdown`.

**Acceptance criteria.**
- A poller failure keeps the thread alive and backs off, capped at 5 minutes.
- `Reconcile` through the wake channel forces an early pass.
- Poller threads are tested with a fake `Exec`, a short interval injected for
  the test, and assertions on the messages received.

---

## Task 5 — Stage gates

**Goal.** The four hardcoded predicates, and edge triggering so work starts
once per transition.

**Files.** `src/gates.rs`.

**Detail.**
- `ReadyWork { repo: String, stage: Stage, kind: ItemKind, number: u64,
  head_sha: Option<String> }`.
- Predicates against a `RepoSnapshot`:
  - refine: issue is open and carries `to-refine`.
  - implement: issue is open, carries `refined`, does not carry `to-refine`,
    and every dependency is closed.
  - review: pull request is open and `draft` is true.
  - release: pull request is open and `draft` is false.
- Dependencies come from the issue body. Parse `blocked by`, `blocked-by`,
  and `depends on`, case-insensitive. Each phrase introduces a LIST, not a
  single number: collect every `#N` that follows it, separated by commas,
  the word `and`, or plain spaces, and stop at the first token that is not a
  separator or a `#N`. A body may carry several such phrases; collect them
  all. So `blocked by #1, #2 and #3` yields 1, 2 and 3, and
  `blocked by #1 then ship #9` yields only 1. A dependency is met when that issue is absent from the open set.
  Put this in `parse_blocked_by(body: &str) -> Vec<u64>`.
- `GateTracker` holds the last seen truth per `(repo, stage, kind, number)` and
  emits `ReadyWork` only on a false to true edge. For the review stage the key
  also carries `head_sha`, so a new push re-triggers. Provide
  `forget(repo, kind, number)` for items that disappear.
- The tracker never creates tasks and never touches the release queue. It only
  reports. The daemon decides what to do with release-stage `ReadyWork`.

**Acceptance criteria.**
- Each predicate has a positive and a negative test.
- A label that stays set across two polls yields exactly one `ReadyWork`.
- A label removed and re-added yields a second `ReadyWork`.
- A draft pull request that gets a new `head_sha` yields a second `ReadyWork`;
  an unchanged one does not.
- `parse_blocked_by` covers all three phrasings, several numbers in one body,
  and text that must not match, such as a bare `#12`.
- An implement gate stays shut while a dependency issue is still open.

---

## Task 6 — Task table and state machine

**Goal.** The five task states, transitions, and attempt counting.

**Files.** `src/tasks.rs`.

**Detail.**
- `TaskState { Queued, Running, AwaitingUser, Done, Failed(String) }`.
- `Task { id: String, repo: String, stage: Stage, kind: ItemKind, number: u64,
  state: TaskState, attempt: u32, session_id: Option<String>,
  log_path: PathBuf, head_sha: Option<String>, created_ms: u64,
  updated_ms: u64 }`.
- `TaskTable { by_id: BTreeMap<String, Task>, order: Vec<String> }` keeps
  insertion order for fair dispatch.
- `Task::new` builds the id per the naming rules. `TaskTable::upsert_queued`
  refuses to add a second task for the same `(repo, stage, kind, number)` while
  one is not terminal, and reports which existing task blocked it.
- Legal transitions only: Queued to Running or Failed; Running to AwaitingUser,
  Done, or Failed; AwaitingUser to Running, Done, or Failed; Failed to Queued
  when the attempt count is below 3.
- Queued to Failed exists so a task can be cancelled or dropped before it ever
  starts. The UI offers abort on any listed task, and the daemon must be able
  to drop queued work whose trigger has gone away. Without it the lifecycle
  would need a second, separate removal path outside the state machine, which
  is worse than one more legal edge. Everything else returns an error naming both
  states. `MAX_ATTEMPTS = 3`.
- Helpers: `counts_by_stage()`, `counts_by_stage_repo()`, `running()`,
  `active()` where active means Queued, Running, or AwaitingUser.
- Cancelling is `Failed("cancelled".into())`.

**Acceptance criteria.**
- A table test asserts every legal transition and rejects a representative
  illegal one for each state.
- Retry past `MAX_ATTEMPTS` is refused with a clear message.
- A duplicate queued task for the same item and stage is refused while the
  first is active, and allowed once it is terminal.
- Counting helpers are covered.

---

## Task 7 — Worktree manager

**Goal.** One worktree per issue, reused across stages, plus the marker files
that carry state instead of a database.

**Files.** `src/worktree.rs`.

**Detail.**
- `WorktreeManager { state_dir: PathBuf }` with methods taking a `RepoConfig`.
- `ensure_issue(repo, number) -> Result<PathBuf>`: if the path exists and is a
  registered worktree, return it. Otherwise run
  `git -C <repo_path> worktree add -b aif/<repo>/issue-<n> <path> <base>` where
  base is `origin/HEAD` resolved through
  `git -C <repo_path> symbolic-ref refs/remotes/origin/HEAD`, falling back to
  the current `HEAD`. If the branch already exists, add the worktree without
  `-b`.
- `ensure_train(repo) -> Result<PathBuf>`: same, branch `aif/<repo>/train`,
  always cut from the resolved default branch, and reset to it when the
  worktree already exists.
- Markers live inside the worktree, in a `.aif` directory that the manager
  creates and that the implementer must add to `.git/info/exclude` for that
  worktree so the agents never commit it:
  - `.aif/session` holds the claude session id.
  - `.aif/reviewed-sha` holds the head sha of the last completed review.
  Provide typed read and write helpers for both. Write with a temporary file
  and a rename so a crash cannot leave a half-written marker.
- `exists_issue(repo, number) -> bool` reports whether the worktree is already
  there. It decides create versus reuse. It is NOT a dispatch blocker: after a
  restart the gates legitimately re-open work whose worktree already exists,
  and that work must resume in place. The only duplicate-dispatch guard is the
  in-memory task table from chunk 6.
- `remove_issue(repo, number, proof: Cleanable)` runs `git worktree remove` and
  then deletes the branch. `Cleanable` has one variant, `MergedOrClosed`, so no
  code path can delete the worktree of a live issue by accident. There is no
  boolean force flag.
- Git runs through the `Exec` trait from `src/exec.rs`. Take it as `&dyn Exec`.

**Acceptance criteria.**
- Tests build a real temporary git repository with an initial commit, then
  exercise create, reuse, marker round trip, and removal. Git itself may run in
  these tests; the network must not.
- Creating twice for the same issue does not fail and returns the same path.
- Marker write is atomic: no partial file is visible under a simulated failure,
  tested by writing through the helper and asserting no `.tmp` file remains.
- `remove_issue` cannot be called without the proof value. This is a
  compile-time property; add a test that the merged path succeeds.
- The `.aif` directory is excluded from git inside the worktree.

---

## Task 8 — Scheduler

**Goal.** Decide what may start, honouring stage limits and strict per
repository lane reservations.

**Files.** `src/sched.rs`.

**Detail.**
- `Limits { stage: BTreeMap<Stage, usize>, lanes: BTreeMap<(Stage, String),
  usize> }`, built from `Config` and mutable at run time.
- Lanes are strict. A reservation of `n` for `(stage, repo)` keeps `n` slots of
  that stage for that repository even when that repository has nothing to run.
  Free capacity for any other repository is
  `limit(stage) - running(stage) - sum_over_other_repos(max(0, reserve(other) -
  running(stage, other)))`.
- `can_start(&Limits, &TaskTable, stage, repo) -> Verdict` where `Verdict` is
  `Yes`, or `No(Reason)` with `Reason` naming `StageFull`, `LaneBlocked`, or
  `Paused`.
- `next_dispatch(&Limits, &TaskTable, &Paused) -> Option<TaskId>` walks queued
  tasks in insertion order, skips those blocked, and returns the first that may
  start. It never reorders and never starves an earlier task by preferring a
  later one from another repository.
- `Paused { global: bool, stages: BTreeSet<Stage>, repos: BTreeSet<String> }`.
- `warnings(&Limits) -> Vec<String>` reports a stage whose lane reservations
  equal its limit, since no other repository can ever run there. `doctor` shows
  these.

**Acceptance criteria.**
- A reserved slot stays free for its repository while another repository has
  queued work: with implement limit 3, borsuk reserve 1, and three qubitsok
  tasks queued, only two qubitsok tasks start.
- The reserving repository can use its slot at once when its work arrives.
- A repository with no reservation may use all remaining capacity.
- Reservations equal to the limit produce a warning.
- Pausing a stage, a repository, or everything blocks dispatch and reports the
  right reason.
- Insertion order is preserved; a test asserts no starvation of the head task.

---

## Task 9 — Process supervision

**Goal.** Spawn a child, stream its stdout as lines, copy every raw line to a
task log, and report exit, all without blocking the event loop.

**Files.** `src/proc.rs`.

**Detail.**
- `RunSpec { task: String, cwd: PathBuf, program: String, args: Vec<String>,
  env: Vec<(String, String)>, log: PathBuf }`.
- `spawn(spec, tx: Sender<ProcEvent>) -> Result<ProcHandle>`. It creates the
  log's parent directory, opens the log for append, spawns the child with piped
  stdin, stdout, and stderr, and starts two reader threads. The stdout thread
  writes each raw line to the log and sends `ProcEvent::Line`. The stderr
  thread appends to the same log with a `stderr ` prefix and does not send
  events. A waiter thread sends `ProcEvent::Exit { code, ok }`.
- `ProcHandle` exposes `write_line(&str)`, `close_stdin()`, `interrupt_hook`
  (a closure the runner can install to send its own protocol interrupt),
  `terminate()`, and `kill()`. `terminate()` sends SIGTERM without a `libc`
  dependency: use `std::process::Child::kill` for SIGKILL and run
  `kill -TERM <pid>` through the `Exec` trait from `src/exec.rs` for SIGTERM.
- `stop_gracefully(handle, protocol_interrupt: bool)` runs the escalation:
  optional protocol interrupt, wait 10 s, SIGTERM, wait 5 s, SIGKILL. It must
  not block the caller; run it on its own thread and report the outcome as a
  `ProcEvent`.
- Writing to a dead child's stdin must return an error, never panic.

**Acceptance criteria.**
- Tests drive a fake child: a small shell script written into a temporary
  directory that prints known lines, echoes stdin, or sleeps.
- Every raw stdout line reaches the log file, byte for byte, including lines
  that are not valid JSON.
- Exit code and success flag are reported once.
- The escalation reaches SIGKILL for a script that ignores SIGTERM, and the
  test finishes well under its timeout.
- Writing after exit returns an error.

---

## Task 10 — opencode runner

**Goal.** Run implement and review tasks as one-shot opencode processes and map
their NDJSON to run events.

**Files.** `src/runner/mod.rs`, `src/runner/opencode.rs`.

**Detail.**
- Traits in `runner/mod.rs`:

```rust
pub trait Runner: Send {
    fn start(&mut self, job: &Job, tx: Sender<RunEvent>)
        -> anyhow::Result<Box<dyn Session>>;
}
pub trait Session: Send {
    fn send_user(&mut self, text: &str) -> anyhow::Result<()>;
    fn answer(&mut self, request_id: &str, answer: Answer) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;
}
```

  The two interactive methods have default bodies returning an error that says
  the runner does not support steering. `stop` has no default.
- `Answer { Allow { updated_input: Option<serde_json::Value> },
  Deny { message: String } }` is defined in this chunk, in `runner/mod.rs`,
  because the trait signature above names it. Chunk 12 uses it; it does not
  redefine it.
- `Job { task: String, stage: Stage, repo: String, model: String,
  variant: Option<String>, prompt: String, cwd: PathBuf, log: PathBuf,
  resume: Option<String>, yolo: bool }`.
- `RunEvent { Started { task, session_id: Option<String> },
  Text { task, text }, Tool { task, name, summary },
  Ask { task, request_id, tool, input: serde_json::Value,
        suggestions: serde_json::Value, needs_human: bool },
  TurnEnd { task, ok: bool, summary: String, cost_usd: Option<f64> },
  Exit { task, ok: bool, detail: String } }`.
- The opencode runner builds
  `opencode run --format json --auto --agent build -m <model>
   [--variant <v>] --dir <cwd> <prompt>` and parses each line: `step_start`
  yields `Started` with the `sessionID` on the first line only; `text` yields
  `Text` from `part.text`; a tool part yields `Tool`; `step_finish` yields
  `TurnEnd`. Unknown types are logged and ignored, never fatal.
- `Session::stop` for opencode kills the child; `send_user` and `answer` return
  the unsupported error.

**Acceptance criteria.**
- Fixture replay: a recorded NDJSON file drives the parser and the expected
  `RunEvent` sequence is asserted. Record the fixture from the shapes in this
  spec; do not call opencode in the test.
- The session id is captured from the first line that carries one.
- A malformed line does not abort the run and is preserved in the log.
- The built argument vector is asserted exactly, including `--auto` and the
  optional `--variant`.

---

## Task 11 — claude runner, part one

**Goal.** Spawn the claude session, complete the initialize handshake, send the
prompt, and map ordinary output.

**Files.** `src/runner/claude.rs`.

**Detail.**
- Build the argument vector exactly as in "Verified external protocol facts",
  including `--permission-prompt-tool stdio`. Mint the session id with
  `uuid::Uuid::new_v4` unless `job.resume` is set, in which case pass
  `--resume <id>` and do not pass `--session-id`.
- On start: write the `initialize` control request, wait for its
  `control_response` (with a timeout of 30 s, after which fail the job with a
  clear message), then write the prompt as a user message.
- Map output lines: `system`/`init` gives `Started` with the session id;
  `assistant` content blocks give `Text` for `text` blocks and `Tool` for
  `tool_use` blocks, with a summary derived from the input, using the command
  for `Bash` and the file path for `Write` and `Edit`, otherwise the first 120
  characters of the JSON; `result` gives `TurnEnd` with `ok` from `subtype ==
  "success"`, the `result` text as the summary, and `total_cost_usd`.
  `system`/`thinking_tokens` and `rate_limit_event` are ignored.
- `Session::send_user` writes another user message line. `Session::answer`
  keeps its default error body in this chunk.
- `Session::stop` in this chunk is a plain kill of the child, so the type
  compiles. Chunk 12 replaces it with the interrupt escalation. Do not build
  the escalation here.
- Persist the session id into the worktree marker as soon as it is known, so a
  restart can resume. The runner receives a callback for this; it does not
  import the worktree module directly.

**Acceptance criteria.**
- The argument vector is asserted exactly, for both the fresh and the resume
  case.
- Fixture replay maps a recorded happy-path session into the expected events.
- A missing `control_response` within the timeout fails the job with a message
  naming the handshake.
- Tool summaries for `Bash`, `Write`, and an unknown tool are asserted.
- The session id callback fires exactly once.

---

## Task 12 — claude runner, part two: asks, steering, and stopping

**Goal.** The permission channel, the yolo policy, mid-run steering, and
graceful abort.

**Files.** `src/runner/claude.rs`.

**Detail.**
- On a `control_request` whose `request.subtype` is `can_use_tool`:
  - Read `requires_user_interaction` from the request, defaulting to false.
  - When `job.yolo` is true and the flag is false, answer `allow` at once with
    `updatedInput` set to the request's own `input`, and emit no `Ask` event.
  - Otherwise emit `RunEvent::Ask` with `needs_human` set to the flag, and wait
    for `Session::answer`.
- `Answer { Allow { updated_input: Option<Value> }, Deny { message: String } }`.
  `answer` writes the `control_response` in the verified shape, echoing the
  request id. Answering an unknown request id is an error, not a panic.
- Track pending asks in the session so an abort can clear them.
- `Session::stop` writes `{"type":"control_request","request_id":"<new>",
  "request":{"subtype":"interrupt"}}`, then runs the escalation from chunk 9.
- Idle handling: the runner records the time of the last event and exposes
  `idle_for()`. It does not kill itself; the daemon decides, because only the
  daemon owns deadlines.

**Acceptance criteria.**
- With yolo on, a `can_use_tool` for `Write` is answered `allow` automatically
  and the exact response line is asserted against the verified shape.
- With yolo on, an `AskUserQuestion` request carrying
  `requires_user_interaction: true` is NOT auto-answered and produces an `Ask`
  event with `needs_human` true.
- With yolo off, an ordinary tool ask produces an `Ask` event.
- A deny answer writes `behavior: "deny"` with the message.
- `stop` writes the interrupt line before any signal.
- Answering an unknown request id returns an error.

---

## Task 13 — Decisions

**Goal.** One queue for every question that needs a human, and one answer path.

**Files.** `src/decisions.rs`.

**Detail.**

```rust
pub enum DecisionKind {
    Permission { task: String, request_id: String, tool: String,
                 input: serde_json::Value },
    Question   { task: String, request_id: String,
                 questions: serde_json::Value },
    Stuck      { task: String, reason: String },
    NeedsHuman { kind: ItemKind, number: u64, title: String },
    ReleaseGate{ prs: Vec<u64> },
}
pub struct Decision { pub id: String, pub repo: String,
                      pub stage: Option<Stage>, pub kind: DecisionKind,
                      pub opened_ms: u64 }
pub enum Response { Allow, Deny { message: String },
                    Answers { updated_input: serde_json::Value },
                    Text { text: String }, Retry, Cancel,
                    Go { prs: Vec<u64> } }
```

- `Decisions { open: Vec<Decision> }` with `open()`, `push()`, `take(id)`, and
  `drop_for_task(task)`.
- Ids are stable and derived, not random, so the same underlying condition does
  not open two rows: `perm:<task>:<request_id>`, `stuck:<task>:<attempt>`,
  `human:<repo>:<kind><number>`, `gate:<repo>`.
- `validate(&Decision, &Response) -> Result<()>` refuses a mismatched pair, for
  example `Go` against a `Permission`.
- Sources are wired by the daemon in chunk 15; this chunk owns the type, the
  queue, the id rules, and the validation.
- The `NeedsHuman` source reads the `needs-human` label. Opening it must not
  require a running task, because the label may appear after a task ends.

**Acceptance criteria.**
- Each id rule is asserted.
- Pushing the same underlying condition twice yields one open decision.
- Every legal kind and response pairing is accepted and every illegal one is
  refused, in a table test.
- `drop_for_task` removes only that task's rows and leaves `NeedsHuman` and
  `ReleaseGate` rows alone.

---

## Task 14 — Release trains

**Goal.** Collect ready pull requests, let the human stack them, and fire a
batch by policy or by hand.

**Files.** `src/trains.rs`.

**Detail.**
- `Train { repo: String, queue: Vec<u64>, stacked: Vec<u64>,
  in_flight: Option<String>, last_fire_ms: Option<u64> }`.
- Stacking is recorded on GitHub with the `release-stacked` label, so it
  survives a restart. `stacked` is rebuilt from labels on every poll;
  `stack(pr, on)` calls the label helpers from chunk 3 and updates the model
  optimistically.
- `enqueue(pr)` adds a ready pull request when it is absent. `dequeue(pr)`
  removes one that closed, merged, or went back to draft.
- `should_fire(&self, policy, now_ms) -> Option<Vec<u64>>`:
  - `Manual` never fires by itself.
  - `Interval { minutes }` fires when the queue is not empty and
    `now - last_fire >= minutes * 60_000`.
  - `Threshold { count }` fires when the queue length reaches count.
  - Never fires while `in_flight` is set.
  - The fired set is `stacked` when it is not empty, otherwise the whole queue.
- `next_deadline_ms(&self, policy, now_ms) -> Option<u64>` gives the event loop
  the exact wake time for an interval policy. Threshold and manual return none.
- `fire(prs) -> String` marks `in_flight` with the new task id.
  `finish(ok)` clears it and, on success, removes those pull requests from the
  queue and clears their labels; on failure it returns them to the queue.

**Acceptance criteria.**
- A threshold policy fires exactly once when the count is reached and does not
  fire again while a train is in flight.
- An interval policy fires only at or after its deadline, and
  `next_deadline_ms` matches the moment it fires.
- Manual never fires on its own but `fire` works when called.
- A stacked subset fires instead of the whole queue when stacking is present.
- A failed train returns its pull requests to the queue and a retry reuses the
  same set.
- A pull request that goes back to draft leaves the queue.

---

## Task 15 — Daemon event loop

**Goal.** Assemble every part into one thread that owns all state and sleeps
until the next real deadline.

**Files.** `src/daemon.rs`, `src/state.rs`.

**Detail.**
- `state.json` in the state directory holds only what GitHub cannot: stage
  limit overrides, lane overrides, release policy overrides, and
  `last_fire_ms` per repository. Write it with a temporary file and a rename,
  only when a value changes. A missing or corrupt file is not an error; log
  once and continue with the config defaults.
- `Daemon` owns `Config`, `Limits`, `Paused`, `Snapshot`, `GateTracker`,
  `TaskTable`, `Decisions`, `BTreeMap<String, Train>`, the live sessions, and
  the socket subscribers.
- The loop:

```
loop {
    let timeout = next_deadline() - now;   // None means block
    match rx.recv_timeout(timeout) { ... }
    // after every message: drive()
}
```

- `next_deadline()` is the minimum of each train's interval deadline and each
  idle claude session's reaper expiry. When there is none, block on `recv`.
- `drive()` runs after every message and does, in order: apply gate results to
  the task table and the trains; fire any due train; reap idle sessions;
  dispatch while `next_dispatch` yields a task; set a `dirty` flag when anything
  changed. It must be idempotent and must not recurse.
- This chunk does NOT define the wire state type and does not push anything.
  `StateView` belongs to chunk 16. Here the daemon only owns the `dirty` flag
  and an empty subscriber list, so chunk 16 can attach the socket without
  reshaping the loop.
- Two different limits, and they must not be confused. `counts_by_stage()` in
  chunk 6 counts RUNNING tasks only, which is what `can_start` uses: a queued
  task must not count against the limit it is waiting on, or a stage with limit
  1 and one queued task would deadlock against itself.
- The daemon owns the SECOND limit, on live processes. An interactive refine
  chat in `AwaitingUser` still holds a claude process between turns, and those
  processes are the real memory cost. So the daemon must also refuse to start a
  new session for a stage when the number of LIVE sessions for that stage has
  reached its limit, counting `AwaitingUser` tasks whose process is still
  alive. The idle reaper is what frees this capacity: when it kills a parked
  chat's process the task stays `AwaitingUser` and resumable, but its live-session
  slot is released at once. Without this, parked chats accumulate live
  processes without bound, which is exactly what the stage limit exists to stop.
- Dispatch: ensure the worktree, render the prompt, start the runner, move the
  task to Running. Refine tasks run in the repository checkout, not a
  worktree, and never create one.
- Run events map to state: `Started` stores the session id and writes the
  marker; `Ask` with `needs_human` opens a Question decision, otherwise a
  Permission decision; `TurnEnd` moves an interactive task to AwaitingUser and
  a one-shot task toward its exit; `Exit` with a failure and attempts left
  requeues, otherwise fails the task and opens a Stuck decision.
- Review success writes `.aif/reviewed-sha` only after the task reports
  success, never before.
- Restart derives everything: the first poll rebuilds gates, trains from the
  `release-stacked` labels, and `NeedsHuman` decisions from labels. Tasks that
  were running are gone with their processes; their worktrees remain and the
  gates re-open them, so work resumes in place.
- Prompt rendering lives here: read `prompts/<stage>.md` from the config
  directory, falling back to a built-in default compiled into the binary.
  Placeholders: `{repo}`, `{owner_repo}`, `{number}`, `{title}`, `{body}`,
  `{worktree}`, `{pr_list}`, `{pr_numbers}`, `{pr_count}`.
- The built-in prompts must instruct the implement and review agents to add the
  `needs-human` label when they need a human decision, and must forbid them
  from creating their own worktrees.

**Acceptance criteria.**
- A restart test: build a daemon over fake pollers, drive it to a known state,
  drop it, build a new one from the same directories, and assert that trains,
  `NeedsHuman` decisions, and worktree-based dedupe all come back.
- `next_deadline` returns the earliest of several pending deadlines and returns
  none when nothing is pending. A test asserts the loop does not wake without a
  reason.
- A run of `drive()` twice with no new messages produces no extra dispatches.
- A failing task with attempts left is requeued; the third failure opens a
  Stuck decision.
- `.aif/reviewed-sha` is absent after a failed review and present after a
  successful one.
- `state.json` survives a round trip and a corrupt file is tolerated.
- Prompt rendering fills every placeholder and reports an unknown one.

---

## Task 16 — Control socket

**Goal.** The two message kinds between daemon and UI.

**Files.** `src/sock.rs`, plus the `stop` path in `src/bin/aif.rs`.

**Detail.**
- Wire format is one JSON object per line over a Unix socket.
- Out: `Push::State(StateView)`. `StateView` carries the repositories, the
  stages with their limit, override flag, running count, and queued count, the
  lane reservations, the tasks with id, repo, stage, item, state, attempt, and
  log path, the open decisions, the trains with queue, stacked set, policy, and
  next fire time, and the paused flags.
- In: `Action` as a tagged enum with `Refine`, `Chat`, `Answer`, `Abort`,
  `Retry`, `Stack`, `Go`, `Policy`, `Limit`, `Lane`, `Pause`, `TicketCreate`,
  `Reconcile`, `Stop`.
- The server accepts connections on a thread, sends the current state at once,
  then forwards every later push. A slow or dead client is dropped, never
  blocks the daemon: use a bounded channel per subscriber and drop the
  subscriber when it fills.
- Pushes coalesce: the daemon marks state dirty and the socket thread sends at
  most one push every 50 ms.
- The socket file is created with mode 0600. A stale socket from a dead daemon
  is removed on bind after a connect attempt fails.
- The client half provides `connect()`, `send(Action)`, and an iterator of
  pushes, and is used by both the TUI and `aif stop`.

**Acceptance criteria.**
- A round trip test over a real socket in a temporary directory: connect,
  receive the initial state, send an action, and observe the effect in the
  next push.
- A subscriber that never reads is dropped and the daemon keeps running.
- Coalescing is asserted: many rapid changes produce far fewer pushes.
- The socket file mode is 0600.
- A stale socket file is replaced rather than causing a bind failure.

---

## Task 17 — Start, stop, and doctor

**Goal.** Run the daemon detached and report on the installation.

**Files.** `src/bin/aif.rs`, `src/bin/aifd.rs`, `src/doctor.rs`.

**Detail.**
- `aif` with no subcommand starts the TUI. If no daemon answers the socket, it
  starts one first with
  `systemd-run --user --collect --unit aif-daemon -- aifd run`, falling back to
  a plain detached spawn when `systemd-run` is missing, then waits up to 10 s
  for the socket.
- `aif stop` sends `Action::Stop` and waits for the socket to disappear.
- `aif doctor` reports, without changing anything: config path and validity,
  every repository with its resolved `owner/repo` and whether the path is a git
  repository, the versions of `gh`, `git`, `claude`, and `opencode`, whether
  `claude` meets the 2.1.223 floor, whether the daemon is running, the
  scheduler warnings from chunk 8, and the number of worktrees with their
  issue state.
- `aif doctor --clean` removes worktrees whose issue is closed or whose pull
  request is merged. It must pass `Cleanable::MergedOrClosed` and must never
  touch a worktree whose issue is open. It prints what it will remove and asks
  for confirmation unless `--yes` is given.

**Acceptance criteria.**
- `doctor` output is generated from an injected environment in tests: a fake
  `Exec` supplies versions and git answers, so the test needs no real tools.
- A claude version below the floor is reported as a failure with the floor
  named.
- `doctor --clean` on a fixture with one open and one closed issue removes only
  the closed one.
- `aif stop` against no daemon exits with a clear message and a non-zero code.

---

## Task 18 — TUI shell and pipeline view

**Goal.** Connect, hold the pushed state, and draw the home view.

**Files.** `src/tui/mod.rs`, `src/tui/theme.rs`, `src/tui/pipeline.rs`.

**Detail.**
- Three threads: a key reader using crossterm, a socket reader, and the main
  loop that owns the model and draws. The main loop blocks on one channel and
  redraws only when a message arrives. There is no periodic redraw.
- `App { state: Option<StateView>, connected: bool, view: View,
  selection: Selection, toast: Option<(String, Instant)> }`.
- Reconnect with backoff from 1 s to 10 s. While disconnected, keep drawing the
  last state with a clear banner.
- Theme is a constant palette struct in `theme.rs`. Do not read
  `ui/tokens/tokens.json`.
- The pipeline view groups tickets by stage, and inside a stage by repository.
  A stage header row shows `running/queued` against the limit and marks a
  limit that differs from the config file. The release group shows the queue,
  the stacked set, the policy, and the countdown.
- Keys in this chunk: `1`, `2`, `3` switch views, `j` and `k` move, `q` quits,
  `?` opens a help overlay. Interactions arrive in chunk 19.

**Acceptance criteria.**
- Render tests use `ratatui::backend::TestBackend` and assert visible content
  for: an empty state, a state with tasks in every stage, and the disconnected
  banner.
- The main loop performs no redraw without an input or socket message; a test
  asserts the draw count over a quiet interval.
- Reconnect backoff is unit-tested as a pure function.

---

## Task 19 — Pipeline interactions

**Goal.** Drive the factory from the home view.

**Files.** `src/tui/pipeline.rs`.

**Detail.**
- `+` and `-` change the selected stage limit and send `Action::Limit`.
  With a repository row selected they change the lane reservation and send
  `Action::Lane`.
- `p` pauses or resumes the selected scope, stage or repository, and `P` does
  the same globally.
- `r` on a ticket sends `Action::Refine` and switches to the session view.
- `n` sends `Action::TicketCreate` for the selected repository and switches to
  the session view.
- `x` aborts the selected task after a confirmation, `R` retries a failed one.
- Inside the release group: space sends `Action::Stack`, `g` sends
  `Action::Go` after a confirmation that lists the pull requests, and `s`
  cycles the policy through manual, interval, and threshold and sends
  `Action::Policy`.
- Every action shows a toast with what was sent.

**Acceptance criteria.**
- Each key produces exactly the expected `Action`, asserted through a fake
  sender, including the confirmation gate for `x` and `g`.
- A key pressed with nothing selected is a no-op and shows no toast.
- Limit changes clamp at 1 and never go below it.

---

## Task 20 — Session view

**Goal.** Read a running agent and steer it.

**Files.** `src/tui/session.rs`, `src/tui/transcript.rs`.

**Detail.**
- The view tails the task's log file itself, using the path from the state
  push. It reads new bytes on each redraw and on a file-change poll of at most
  once per 200 ms, keeping the last 2000 parsed items in a ring buffer.
- `transcript.rs` turns a raw log line into display lines. It handles both the
  claude and the opencode shapes and falls back to a dim raw line. Text wraps
  to the pane width. Tool lines are dim and prefixed. A failed tool result is
  marked. The renderer is a pure function of line and width so it is testable.
- An input bar sends `Action::Chat`. `ctrl-x` aborts. `PageUp` and `PageDown`
  scroll, `End` returns to following the tail.
- A pending ask for this task renders inline with its options.

**Acceptance criteria.**
- The renderer has unit tests for a claude assistant text line, a claude
  tool_use line, an opencode text line, a malformed line, and a very narrow
  width.
- The ring buffer never exceeds its bound, asserted with more input than the
  bound.
- Tail following resumes with `End` after scrolling up.
- Typing and pressing enter sends one `Action::Chat` with the typed text.

---

## Task 21 — Decisions inbox

**Goal.** One place to answer everything.

**Files.** `src/tui/inbox.rs`.

**Detail.**
- Rows list every open decision across repositories with age, repository,
  stage, and a one-line summary.
- `y` allows, `n` denies with a typed reason, `a` allows and applies the
  request's own suggestion when it carries one.
- A `Question` row expands to its options; digits pick, `enter` submits, and
  `i` types a free answer.
- A `ReleaseGate` row expands to the pull request list with checkboxes; space
  toggles and `g` fires.
- A `Stuck` row offers retry or cancel.
- `enter` on any row jumps to the session view for its task, when it has one.
- The badge with the open count appears in every view's status bar, and `!`
  jumps to the oldest decision from anywhere.

**Acceptance criteria.**
- Every row kind renders and every key sends the matching `Action::Answer`
  with the right response variant, asserted through a fake sender.
- A response that does not match the decision kind can never be produced by
  the UI; the test asserts the key map per kind.
- The badge count matches the pushed state in a render test.

---

## Task 22 — Remove v0.4 and ship

**Goal.** Delete the old factory and leave one clean tree.

**Files.** Deletions plus `install.sh`, `README.md`, `docs/v0.5/`.

**Detail.**
- Delete `ui/`, `zellij/`, `bin/clauded`, `bin/codexd`, `bin/opencoded`,
  `bin/ai-factory`, and the old `check.sh` behaviour that pointed at
  `ui/console`.
- `install.sh` builds the crate and installs `aif` and `aifd` into
  `~/.local/bin`, creates the config directory, and writes
  `factory.example.toml` and the default prompts when they are absent. It must
  not touch any zellij path.
- Rewrite `README.md` for v0.5: what it is, the four stages, install, the
  config file, the keys, and the known limits. Keep it in Simplified Technical
  English. Remove every zellij and v3 reference.
- Leave `docs/v0.5/SPEC.md` where it is. It is the delivery authority and
  moving it breaks the chunk briefs.

**Acceptance criteria.**
- `rg -i zellij` finds nothing outside `.git` and the changelog.
- `rg "ui/console"` finds nothing.
- `./check.sh` passes.
- `install.sh` runs in a temporary `HOME` and produces both binaries and a
  config directory, asserted by a test script.
- The README names no removed command.
