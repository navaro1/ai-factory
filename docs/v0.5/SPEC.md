# AI Factory v0.5 — implementation spec

Authority for the v0.5 rework. Each chunk below is one dispatch, one commit.
The design rationale lives in the approved plan; this file is what implementers
and reviewers read.

## What v0.5 is

A daemon plus a terminal UI drive AI coding agents against GitHub issues in
several repositories. The Tickets view supports review and ticket shaping.
An issue then flows through four fixed stages.

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
4. No domain polling loops and no tick thread. The event loop blocks on
   `recv_timeout(next_deadline)`. Deadlines are computed from pending work
   (train fires, idle reaper expiries). The only periodic clock in the whole
   system is each repository's 20 s ETag poll thread.
5. GitHub is the source of truth. Do not build a journal, an event log, or a
   task database. `state.json` holds runtime overrides, train times, and active
   ticket conversation metadata. Task logs hold full transcripts.
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
`rate_limit_event`, `control_request`, `control_response`, `result`.

An `assistant` line, probed on this machine, carries `message`,
`parent_tool_use_id`, `request_id`, `session_id`, `timestamp`, `type`, `uuid`.
Its `message` carries `content`, `id`, `model`, `role`, `stop_reason`,
`usage`. Walk `message.content[]`; the block types seen are `thinking`, `text`,
and `tool_use`. A `tool_use` block carries `id`, `name` and `input`. Skip
blocks whose line has a non-null `parent_tool_use_id`, which are subagent
output.

A `result` line carries `subtype`, `result`, `is_error`, `session_id`,
`num_turns`, `total_cost_usd`, `usage`, `duration_ms`, `permission_denials`
and more. Treat `subtype == "success"` as success and read the human-readable
outcome from `result`. The
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
`step_start`, `text`, `step_finish`, and `tool_use`. Every line carries
`sessionID` and a `part` object. A `text` line holds the assistant text at
`part.text`.

A `tool_use` line, probed on this machine, looks like this:

```json
{"type":"tool_use","timestamp":1788091541442,"sessionID":"ses_...",
 "part":{"type":"tool","tool":"read","callID":"call_...",
   "state":{"status":"completed","input":{"filePath":"..."},
            "output":"...","metadata":{...},"time":{...},"title":"..."}}}
```

So the line type is `tool_use`, the part type is `tool`, the tool name is at
`part.tool`, and the call's status, input and output live under `part.state`.
Note the mismatch: the LINE says `tool_use` while the PART says `tool`. Match
on the part, not only on the line type.

One run emits SEVERAL `step_finish` lines, one per step, and exits once. So for
opencode a step ending is not the task ending: task completion is the process
`Exit`, never a `TurnEnd`. This differs from claude, where a `result` line ends
a turn and an interactive task then waits for the human.

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
src/ticket.rs       ticket review actions and GitHub changes
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
src/tui/tickets.rs
```

## Naming rules

- Stage directory and label names are lowercase: `refine`, `implement`,
  `review`, `release`.
- Labels the factory reads: `to-refine`, `refined`, `needs-human`,
  `release-stacked`.
- The Tickets view gives `to-refine` priority when both workflow labels exist.
- Task id format: `<repo>/<stage>-<kind><number>` where kind is `i` or `p`,
  for example `borsuk/implement-i142`. The attempt is a field, not part of
  the id.
- Ticket chat uses `<repo>/ticket-i<number>` as its task identifier.
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
- `spawn_pollers(cfg: &Config, tx: Sender<DaemonMsg>) -> Pollers` where
  `Pollers { handles: Vec<JoinHandle<()>>, wake: BTreeMap<String, Sender<()>> }`.
  Create each repository's wake channel INSIDE spawn_pollers before spawning,
  hand the receiver to the thread, and return the sender in the map. Do not
  deliver the senders to the daemon as a message: that would add a startup
  handshake the daemon must wait on before it can force a reconcile, and it
  would put a `Sender` inside `DaemonMsg`, which costs `PartialEq` on the
  daemon's whole message enum and so costs chunk 15 its message assertions.
  Each thread loops: fetch, send `DaemonMsg::Polled { repo, snapshot }`, then
  wait 20 s on a per-repo wake channel so `Reconcile` can force an early pass.
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
- `can_start(&Limits, &Paused, &TaskTable, stage, repo, task) -> Verdict` where
  `Verdict` is `Yes`, or `No(Reason)` with `Reason` naming `StageFull`,
  `LaneBlocked`, or `Paused`. There is exactly ONE such predicate. Do not also
  expose a capacity-only variant that ignores pause: two near-identical
  predicates invite a caller to reach for the wrong one, and the wrong one
  would dispatch a paused task silently.
- `next_dispatch(&Limits, &TaskTable, &Paused) -> Option<TaskId>` walks queued
  tasks in insertion order, skips those blocked, and returns the first that may
  start. It never reorders and never starves an earlier task by preferring a
  later one from another repository.
- `Paused` contains the global state and explicit `bool` maps for stages,
  repository lanes, and tasks. A task state overrides its lane state. A lane
  state overrides its stage state. A stage state overrides the global state.
  A global change removes all narrower states.
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
- A task, repository lane, stage, or global pause blocks the applicable task.
  The scheduler reports the right reason.
- A resumed task can start below a paused lane, stage, or factory. Sibling
  tasks keep their inherited pause state.
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
  thread appends to the same log with a `stderr ` prefix and sends
  `ProcEvent::StderrLine` for each line, so a harness can read what the child
  prints on stderr. A waiter thread sends `ProcEvent::Exit { code, ok }`.
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
  resume: Option<String>, yolo: bool, allowed_tools: Option<Vec<String>>,
  allowed_permissions: Vec<AllowedPermission> }` with one
  `AllowedPermission { permission, patterns }` per rule.
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
- The opencode runner also reads the stderr lines, because opencode without
  `--auto` cannot emit an ask on stdout: it prints
  `permission requested: <name> (<patterns>); auto-rejecting` and rejects the
  ask. The runner strips the ANSI escapes, matches that shape, and emits one
  `RunEvent::Ask` per match: `tool` is the permission name, `input` is
  `{"patterns": [...]}`, `needs_human` is true only for the `question`
  permission, and `request_id` is `rej-<n>`, counted in stream order, so a
  retry refreshes the same row.
- The job's `allowed_permissions` reach the opencode child as the
  `OPENCODE_PERMISSION` environment value,
  `{"<permission>": {"<pattern>": "allow"}}`, the inline permissions config
  that opencode merges over its own config, so the inline allow wins.
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
- A `Question` decision's `questions` field holds the `AskUserQuestion` tool
  input VERBATIM, so the question list sits under its `questions` key. Every
  reader of the field must accept that object and a bare array, because a
  reader that accepts only one shape shows a question the human cannot answer.
- A `Question` answer's `updated_input` has the shape
  `{"answers": {header: label}}`, matching the runner fixture. The daemon must
  pass it through to the runner VERBATIM; reshaping it breaks AskUserQuestion.
- Deferred, not a defect: the inbox has no "allow always". `Response::Allow`
  carries no field for the request's `permission_suggestions`, so applying a
  suggestion would need a wire change through sock.rs, decisions.rs and the
  daemon. With yolo on by default, ordinary tool asks are auto-allowed anyway,
  so the value is low. The inbox therefore offers only allow and deny, rather
  than offering a key that silently does not do what it says.
- `NeedsHuman` accepts only `Text` and `Cancel`. `Text` posts the text as a
  comment on the item and removes the `needs-human` label; `Cancel` removes the
  label without comment. It must NOT accept `Retry`: a `needs-human` label can
  outlive its task, so there may be nothing to retry, and an inbox that offers
  an action which quietly does nothing is worse than one that offers fewer.
- `Permission` and `Question` share one id namespace (`perm:<task>:<request_id>`)
  because they are the same underlying `can_use_tool` request from the claude
  control channel, told apart only by `requires_user_interaction`. One namespace
  is what stops a single request opening two rows.
- The daemon keeps the ask rows of a one-shot harness over a failure: a task
  whose runner has no `permission_responses` capability, such as opencode,
  cannot answer a live ask, so its `Permission` and `Question` rows stay open
  while the task retries or fails. A steerable claude task still loses its rows
  on failure, and `Stuck` rows always drop. Success and cancel still close every
  row of the task. The kept rows and the granted permission rules persist in
  `state.json` (`runtime.asks` and `runtime.allowed_permissions`), each
  validated against the known task ids like the other runtime maps.
- A one-shot answer routes by the row kind. `Permission` + `Allow` records
  `AllowedPermission { permission, patterns }` for the task and requeues it from
  attempt 1; the next dispatch builds the job with the rule, and the rules clear
  when the task completes or is cancelled. `Permission` + `Deny` closes the row
  and leaves the task state alone. `Question` + `Text` queues the text in
  `pending_chats`, reopens the terminal task, and lets the pending-chats resume
  carry it to the recorded session. The text obeys the same `input_mode` policy
  as a chat message, so a run with no session marker, a task whose worktree a
  sibling holds, and a queued task that never ran all re-push the row with the
  reason; a queued task must refuse, because the resume uses the queued text as
  the whole prompt. An accepted text writes one user line into the task log,
  like every other queue-path message. `Question` + `Answers` is refused and
  re-pushes the row, because a one-shot row carries no option list.
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
  requests an immediate repository poll, while a one-shot task moves toward its
  exit; `Exit` with a failure and attempts left requeues, otherwise fails the
  task and opens a Stuck decision.
- Review success writes `.aif/reviewed-sha` only after the task reports
  success, never before.
- Completion differs per runner and you must not treat them alike. For a
  one-shot runner (opencode) a `TurnEnd` is only a step boundary, several
  arrive per run, and the task completes on `Exit`. For an interactive runner
  (claude) a `TurnEnd` ends a turn: an interactive task moves to `AwaitingUser`
  and waits for the human, while a one-shot claude task completes.
- YOLO POLICY. Set `job.yolo` from the stage's config, which defaults to true.
  With yolo on, the claude runner auto-answers `allow` to ordinary tool asks
  and they never reach you; only asks carrying `requires_user_interaction`
  arrive, and those open a `Question` decision. With yolo off for a stage,
  every ask arrives and opens a `Permission` decision. This is deliberately NOT
  `--dangerously-skip-permissions`: that flag would close the control channel
  and take AskUserQuestion with it, so the agents could no longer ask the human
  anything. Never pass it.
- Answering routes to four sinks and nowhere else: a `Permission` or `Question`
  answer goes to the runner's `answer`; a chat message goes to the runner's
  `send_user`; a `Stuck` answer retries or cancels the task; a `ReleaseGate`
  answer fires the train. `NeedsHuman` is the one that touches GitHub rather
  than a process: `Text` posts a comment and removes the label, `Cancel`
  removes the label.
- `Reconcile` sends on that repository's wake sender from chunk 4's `Pollers`,
  which forces an early poll. Dropping a wake sender stops its poller, which is
  the shutdown path.
- `Refine` and `TicketCreate` from the UI create work directly rather than
  waiting for a gate: `Refine` queues a refine task for the named issue, and
  `TicketCreate` starts an interactive claude session whose prompt tells it to
  create a ticket with `gh`.
- Contracts the socket module imposes on you, reported by its author:
  - `Server::bind` returns `(Server, Receiver<Action>)`. Take the receiver into
    your event loop as another message source.
  - `publish(StateView)` never blocks and coalesces to at most one push every
    50 ms. Call it whenever your `dirty` flag is set; do not throttle it
    yourself.
  - Drop the `Server` before the process exits, so clients see EOF and the
    socket file disappears. A daemon that exits without dropping it leaves a
    stale socket that the next start has to clean up.
- Contracts the decisions module imposes on you, reported by its author:
  - A `ReleaseGate` row snapshots its pull request list when it opens. If the
    stacked set changes while that row is open, you must take the row and push
    a fresh one, or the human will approve a list that no longer matches what
    would actually ship.
  - Build a stuck row with `Decision::stuck(&task, ..)`, not `Decision::new`;
    only the former carries the attempt number that makes the id unique per
    attempt.
- Contracts the release-train module imposes on you, reported by its author:
  - Build a train with `Train::new(repo)`, never a struct literal. It carries
    private state that `finish` needs to reconstruct the exact fired batch.
  - `fire` stamps `last_fire_ms` on EVERY fire, manual included. You must
    restore `last_fire_ms` from `state.json` BEFORE the first `drive()`, or an
    interval policy will see no previous fire and release immediately on every
    daemon start. That is a release to production caused purely by a restart.
  - `finish(false)` deliberately leaves the batch queued and still labelled so
    a retry reuses the identical set. Do not dequeue it yourself; a human who
    stacked five pull requests must not silently get a different five.
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
- The wire protocol is the JSON message format between the daemon and client.
- Every state push carries `protocol_revision`. Revision `1` is current.
- A missing revision decodes as legacy revision `0`.
- The client rejects a different revision. The TUI restores the terminal and prints the complete recovery command.
- The client does not restart the daemon automatically. The operator runs `aif stop`, then starts `aif` again.
- Wire format is one JSON object per line over a Unix socket.
- Out: `Push::State(StateView)`. `StateView` carries the repositories, the
  stages with their limit, override flag, running count, and queued count, the
  lane reservations, the tasks with id, repo, stage, item, state, attempt, and
  log path, the open decisions, the trains with queue, stacked set, policy, and
  next fire time, and the paused flags.
- In: `Action` as a tagged enum with `Refine`, `Chat`, `Answer`, `Abort`,
  `Retry`, `Stack`, `Go`, `Policy`, `Limit`, `Lane`, `Pause`, `TicketCreate`,
  `Reconcile`, `Stop`.
- A pause scope is `global`, `stage`, `lane`, or `task`. A lane contains one
  stage and one repository. The protocol has no repository-wide pause scope.
- `PausedView` contains `global` and an `overrides` list. Each override contains
  one pause scope and one `paused` value. The old `stages` and `repos` fields do
  not exist.
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
  for the socket. A `systemd-run` failure that names an existing unit gets one
  `systemctl --user reset-failed aif-daemon` and one retry, because a daemon
  that just stopped leaves its unit loaded for a moment.
- `aif stop` sends `Action::Stop` and waits for the socket to disappear. A
  successful stop also unloads the transient unit with `systemctl --user stop
  aif-daemon` and `systemctl --user reset-failed aif-daemon`; both ignore
  every error, so the stop works without systemd too.
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

## Contracts the daemon imposes on the TUI chunks

Reported by the author of chunk 15 after building the loop. These bind
chunks 18 to 21.

- `Limits::from_config` seeds the map with the config defaults, so a key being
  PRESENT does not mean it was overridden at run time. To show that a limit
  differs from the config file, compare the effective value against the
  configured one; do not test for map presence.
- A release gate row refreshes only on the poll AFTER a stack label call, because
  stacking is recorded on GitHub. So the trains view must not assume a stack
  edit is visible immediately; show the optimistic local change and let the next
  push correct it.
- `Decisions::push` preserves `opened_ms` for a row that already exists. The
  inbox age column depends on that, so an existing row's age must not reset when
  the daemon re-pushes it.
- `write_marker` appends a newline. Trim marker values when you display them.
- clippy 1.92 under `-D warnings` now demands `is_none_or` and vec literals.
  Expect it and write to it rather than fighting the gate.

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

## Task 19 — Wire the views, then add the pipeline interactions

Chunk 19 now covers BOTH the TUI integration and the pipeline key actions,
because they touch the same files and no other chunk owns the wiring.

**Part A, integration.** Chunk 18 shipped `render()` with placeholder arms for
the session and inbox views, since it could not see them. Wire them now:
- Drive the session view with `show`, `on_redraw`, `poll`, `handle_key`, `draw`.
- Drive the inbox with `observe`, `draw`, `handle_key`.
- Render `inbox::badge` in the status bar of EVERY view, not only the inbox.
- Bind `!` globally to enter the inbox and select its oldest row.
- Map `InboxOutcome::OpenSession` to switching to the session view for that task.
- Rebind the session view's six local style functions onto `theme.rs`.
- The help overlay must list PageUp, PageDown and End.

**The `q` hazard, and it is a real one.** Chunk 18 binds `q` to quit GLOBALLY.
The session view has a text input bar and the inbox has a deny-reason input, so
as it stands, typing the letter q into a message to an agent QUITS THE WHOLE
UI. Gate `q` so it never reaches the global handler while any view holds text
input focus. Write a test that types `q` into the session input and asserts the
app does not quit and the character lands in the buffer.

**Part B, the pipeline interactions.** As below.



**Goal.** Drive the factory from the home view.

**Files.** `src/tui/mod.rs` and `src/tui/pipeline.rs`. You may ALSO edit
`src/tui/session.rs` and `src/tui/inbox.rs` where the wiring genuinely requires
it — this chunk is the integrator, and the seams it must close run through
those files. Say in the report which of them you touched and why. No other file.

**Detail.**
- `+` and `-` change the selected stage limit and send `Action::Limit`.
  With a repository row selected they change the lane reservation and send
  `Action::Lane`.
- `p` pauses or resumes the exact selected stage, repository lane, or task.
  A release row selects its repository release lane. `P` changes the global
  state and removes all narrower states.
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

**Files.** `src/tui/inbox.rs`, `src/tui/mod.rs`, `src/sock.rs`,
`src/daemon.rs`, `src/tui/transcript.rs`.

**Detail.**
- The inbox shows an oldest-first feed. Each item leads with the decision
  message. Metadata shows the age, repository, stage, and decision type.
- Only the selected feed item shows its choices and quick actions.
- `y` allows and `n` denies with a typed reason. There is deliberately NO
  "allow always" key: `Response::Allow` carries no field for the request's
  `permission_suggestions`, so such a key would send exactly the same message
  as `y` while claiming to do more. Offering a key that does not do what it
  says is worse than offering one fewer key, in a view whose whole purpose is
  to be trusted. Recorded as deferred, not a defect.
- A `Question` item expands to its options. Digits pick, `s` submits, and `i`
  types a free answer.
- A `ReleaseGate` row expands to the pull request list with checkboxes; space
  toggles and `g` fires.
- A `Stuck` row offers retry or cancel.
- `enter` opens a focused source detail for every decision type.
- A release detail shows the pull request title and GitHub description. Left
  and Right move through a release batch. Space changes the current choice.
- A `NeedsHuman` detail shows the issue or pull request title and description.
- A permission, question, or stuck detail shows recent visible transcript
  entries from the exact task log. It never shows hidden thought blocks.
- `o` opens the full task session from a task detail. `esc` returns to the
  same selected feed item.
- Page Up and Page Down scroll within a selected feed item when its choices
  exceed the viewport. The scroll offset stays inside that item.
- The daemon sends item content only for open decisions that reference it.
- The badge with the open count appears in every view's status bar, and `!`
  jumps to the oldest decision from anywhere.

**Acceptance criteria.**
- Every decision kind renders and every key sends the matching `Action::Answer`
  with the right response variant, asserted through a fake sender.
- A response that does not match the decision kind can never be produced by
  the UI; the test asserts the key map per kind.
- The badge count matches the pushed state in a render test.
- Feed tests prove the oldest-first order and selected-item actions.
- Detail tests cover pull request batches, issue descriptions, exact task
  context, missing sources, narrow terminals, and shell navigation.

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
- `rg -i zellij` and `rg "ui/console"` find nothing outside `.git` and
  `docs/v0.5/`. The spec and the chunk reviews under `docs/v0.5/reviews/`
  are the HISTORICAL RECORD of how this was built and legitimately name the
  old tree. Never edit a review document to satisfy this check: those files
  are other reviewers' accounts of their own work, and rewriting them
  falsifies the record. Exclude the path instead:
      rg -i zellij --glob '!docs/v0.5/**'
- `./check.sh` passes.
- `install.sh` runs in a temporary `HOME` and produces both binaries and a
  config directory, asserted by a test script.
- The README names no removed command.

---

# v0.5.1 — Open a task session and converse with the agent

## Verified external protocol facts (v0.5.1 additions)

### opencode multi-turn (CLI 1.18.25, probed 2026-08-31)

`opencode run` accepts `-s, --session <id>` and `-c, --continue`. A second run
that names the session id of the first continues that conversation with its full
context.

The probe: turn one was told a secret code and told to write no file. Turn two ran
with `--session <id>` and asked for the code. It answered correctly. The code
existed only in the conversation, never on disk. So the continuation is real.

The session id arrives in the NDJSON stream as `sessionID`. The runner already
captures it and the daemon already writes it to a marker file.

**Mid-turn steering does not work.** The opencode server accepts a prompt with
`delivery: "steer"` and records it. A run that is already in flight finishes its
original task and never reads it. A steer needs a turn boundary. A one-shot run
has none. So the factory never steers a running turn. It queues the message and
sends it as the next turn.

There is no `opencode serve` in this design. There is no HTTP client. Multi-turn
uses the same one-shot child process shape as every other run.

## Task 23 — The opencode follow-up turn

**Goal.** Let a human send a message to an implement or review task. The message
becomes a new turn in the same opencode session.

**Files.** `src/runner/opencode.rs`, `src/daemon.rs`, `src/tasks.rs`.

**Detail.**

- `build_args` appends `--session <id>` when `job.resume` is `Some`. The doc
  comment above `build_args` states the opposite today. Correct it.
- `TaskTable::reopen(&mut self, id: &str, now_ms: u64) -> Result<()>`. It is a new
  method, not a new edge in `transition`. It accepts a task in `Done` or
  `Failed` only. It sets `Queued`. It does NOT raise the attempt count and it
  ignores `MAX_ATTEMPTS`. A human asked for this turn, so the automatic retry
  budget must stay untouched. It refuses `Queued`, `Running`, and `AwaitingUser`
  with a clear error.
- `Daemon::chat` changes. Today it refuses a terminal task, then calls
  `send_user` on any live session, then queues for an `AwaitingUser` claude task.
  The new order:
  1. Find the task. No task is an error, as today.
  2. If a live session exists AND `task_uses_claude(&task)`, call `send_user`.
     This is the unchanged claude path.
  3. Otherwise apply the sibling guard, then push the text into `pending_chats`.
     If the task is terminal, also call `reopen`.
  4. If the task has no session id and no session marker, refuse with a reason.
- **Never call `send_user` on an opencode session.** Today `chat` calls it for any
  live session and prints the resulting "does not support steering" error to
  stderr. Use `task_uses_claude` at `daemon.rs:1992` to gate it, because that
  helper also treats a ticket task as claude.
- **The sibling guard.** Refuse a follow-up when another task for the same
  repository, kind, and number is not terminal. Two agents must never run in one
  worktree. The refusal names the blocking task.
- **The session marker survives.** `remove_task_session_marker` runs when a task
  reaches `Done`. Keep the marker for an opencode task, so a human can converse
  after a daemon restart. The worktree cleanup still removes it.
- `resume_pending_chats` changes. It accepts a task in `Queued` as well as
  `AwaitingUser`. It skips a task in `Running` and waits for the exit. It calls
  `sched::can_start` in place of the raw live-session limit check, so stage
  limits, lane reservations, and pauses all apply to a follow-up turn.
- **One owner per queued task.** A failed opencode task requeues itself, so a
  `Queued` task can also hold a typed message. Two dispatchers would then race.
  The rule: a task with entries in `pending_chats` belongs to
  `resume_pending_chats`, and `dispatch_one` skips it. Do not depend on the order
  of the calls inside `drive`.
- **The reopen hook.** `on_exit_event` handles an opencode exit and calls
  `complete_task` or `fail_run`. After that terminal state is set, check
  `pending_chats`. When it holds a message for this task, call `reopen`.
- **A retry still starts fresh.** `dispatch_one` computes `resume` only for a
  claude task. Do not change that. Only `resume_pending_chats` passes a session
  id for an opencode task.

**Acceptance criteria.**
- `build_args` emits `--session <id>` with a resume id, and omits the flag
  without one.
- A chat on a `Done` opencode task queues the text and reopens the task to
  `Queued`.
- The relaunch calls the runner with the captured session id and the typed
  message as the prompt, not the stage prompt.
- A chat on a `Running` opencode task queues the text, and the relaunch happens
  after the exit event, never before it.
- A chat on a live opencode session never reaches `send_user`. A test asserts
  that the unsupported-steering error never appears.
- A `Queued` task that holds a pending chat gets exactly one launch, and that
  launch carries the typed message.
- A retry after a failure passes `resume = None`.
- `reopen` refuses a `Queued`, `Running`, or `AwaitingUser` task.
- `reopen` succeeds on a task at `MAX_ATTEMPTS` and leaves the attempt count
  unchanged.
- The sibling guard refuses the follow-up and its message names the other task.
- The task session marker still exists after an opencode task reaches `Done`.
- A paused stage holds a queued follow-up, and the follow-up does not start.

## Task 24 — Enter opens a task session

**Goal.** Press `Enter` on a ticket row. The UI opens that task's session view.

**Files.** `src/tui/pipeline.rs`, `src/tui/mod.rs`.

**Detail.**

- Bind `KeyCode::Enter` in `pipeline::handle_key`. On `Row::Ticket { index }`,
  resolve `state.tasks[index]`. Then set the task id and open `View::Session`.
  Clear `app.wanted`, and call `app.show_session_task()`.
- `Row::Stage`, `Row::Repo`, and `Row::Train` do nothing on `Enter`.
- `Enter` works for a task in any state. A `Done` or `Failed` task still has its
  log file, so its transcript is readable.
- Add the row `("enter", "open the selected task session")` to the help rows at
  `mod.rs:1040`. Raise the array length annotation and the overlay height.
- `Esc` already returns to the pipeline view. Do not change it.

**Acceptance criteria.**
- `Enter` on a ticket row sets `View::Session` and the correct task id.
- `Enter` opens a `done` task and a `failed` task.
- `Enter` on a stage, repository, or train row changes nothing.
- A shell test drives the keys through `run_messages`.
- The help overlay test asserts the new key row.

## Task 25 — The input mode on the wire

**Goal.** Tell the UI what the input bar will do for each task.

**Files.** `src/sock.rs`, `src/daemon.rs`.

**Detail.**

```rust
/// What the session view's input bar does for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum InputMode {
    Live,
    Resume,
    NextTurn,
    Follow,
    Closed { reason: String },
}
```

- `TaskView` gains `input: InputMode` and `queued_messages: usize`.
- `Daemon::input_mode(&Task) -> InputMode` is the one place that decides:
  - a live session and a claude task: `Live`
  - a claude task in `AwaitingUser` with a session id and no live process:
    `Resume`
  - an opencode task in `Running`: `NextTurn`
  - an opencode task in a terminal state with a session id or marker: `Follow`
  - any task blocked by the sibling guard: `Closed`, and the reason names the
    other task
  - any task with no session to continue: `Closed`, and the reason says so
- Use `task_uses_claude`, not the raw config string, so a ticket task counts as
  claude although it runs in the refine stage.
- `StateInput` gains one field: a map from task id to `InputMode`. `push_state`
  fills it. `sock.rs` stays a plain serializer and holds no policy.
- `queued_messages` is the count of `pending_chats` entries for that task.

**Acceptance criteria.**
- Each of the six cases above returns the stated mode.
- A `Closed` reason is a sentence a human can act on, not a code.
- `queued_messages` rises after a chat and falls after the relaunch.
- A `StateView` round trip through JSON keeps both new fields.

## Task 26 — The input bar tells the truth

**Goal.** The session view states what your message will do, and never claims to
send a message it dropped.

**Files.** `src/tui/session.rs`, `src/tui/pipeline.rs`, `src/tui/mod.rs`.

**Detail.**

- The input bar's bottom hint comes from `TaskView::input`:

| Mode | Hint |
|---|---|
| `Live` | `enter send · ctrl-x abort · end tail` |
| `Resume` | `enter send · resumes the parked chat` |
| `NextTurn` | `enter queue · lands after this turn · ctrl-x sends it now` |
| `Follow` | `enter send · starts a follow-up turn` |
| `Closed` | the reason text, and the bar is dim |

- In `Closed` mode `handle_key` swallows typed letters and returns `None` for
  `Enter`. The shell must not show the `sent chat` toast for a message the daemon
  will drop.
- The session header shows the queued count when `queued_messages` is above zero.
- A pipeline ticket row shows a short badge when `queued_messages` is above zero,
  so a waiting message is visible from the board.

**Acceptance criteria.**
- A `TestBackend` frame test covers each of the five modes.
- `Enter` in `Closed` mode emits no action and shows no toast.
- `Enter` in `NextTurn` mode emits `Action::Chat`.
- The session header shows the queued count.
- The pipeline ticket row shows the badge.

## Task 27 — The abort delivers the queued message

**Goal.** Make `ctrl-x` do what the session view promises. A human who queued a
message and does not want to wait aborts the running turn, and the message starts
the next turn at once.

**Files.** `src/daemon.rs`.

**Why this task exists.** The `NextTurn` hint reads
`enter queue · lands after this turn · ctrl-x sends it now`. Today `cancel_task`
does the opposite: it calls `pending_chats.remove(id)` and
`remove_task_session_marker(task)`, so the abort DISCARDS the queued message and
the session with it. The promise came from the approved plan. No earlier task
carried it. This task carries it.

**Detail.**

- `cancel_task` keeps its present behaviour for a task that holds NO pending
  chats. Do not change that path.
- When a task holds pending chats:
  - keep `pending_chats` for that task,
  - keep the task session marker,
  - stop the running session as it does today,
  - after the cancel, call `TaskTable::reopen` so the task returns to `Queued`.
- `resume_pending_chats` then launches the queued message as the next turn, with
  the saved session id. It already refuses a `Running` task and already applies
  `sched::can_start`, so limits, lanes, and pauses still hold.
- The sibling guard still applies at request time. An abort does not bypass it.
- A cancel that cannot reopen must not lose the message silently. Log the error
  and leave the task terminal.

**Acceptance criteria.**
- An abort of a task with no pending chats behaves exactly as before: the marker
  goes, the decisions drop, the state is `Failed("cancelled")`.
- An abort of a running opencode task that holds one queued message keeps the
  message, keeps the marker, and returns the task to `Queued`.
- After that abort the next drive pass launches the queued message as the prompt,
  with the saved session id.
- A paused stage still holds the reopened task. The abort does not start work a
  pause forbids.
- The sibling guard still refuses a follow-up after an abort when another task
  holds the same worktree.

## Task 28 — A running task with no session yet takes no message

**Goal.** Close the one window where the wire still promises what the daemon
refuses.

**Files.** `src/daemon.rs`.

**Why this task exists.** A live end-to-end run found it; no unit test did.
`Daemon::input_mode` returns `InputMode::NextTurn` for ANY opencode task in
`Running`:

```rust
} else if task.state == TaskState::Running {
    return InputMode::NextTurn;
```

It never checks that a session exists. `Daemon::chat` does check, and refuses
with "no session id and no session marker; there is no agent session to
continue". An opencode run records its session id only when it prints its first
NDJSON line, one to three seconds after it starts. In that window the session
view shows `enter queue · lands after this turn · ctrl-x sends it now`, the
human types, and the daemon drops the message while the UI reports success.

`closed_reason` already carries the correct sentence for this state,
`"Wait until the task records a session."`. Nothing routes to it today.

**Detail.**

- `input_mode` returns `NextTurn` only when the task is `Running` AND a session
  id or marker exists. Otherwise it falls through to the `Closed` arm, which
  already builds the right sentence.
- Do not change `chat`. The daemon's refusal is correct; the wire is what lies.
- Do not widen `chat` to accept a message with no session. A run that dies
  before it records a session would strand that message.

**Acceptance criteria.**
- A `Running` opencode task with no session id and no marker returns `Closed`,
  and the reason tells the human to wait until the task records a session.
- A `Running` opencode task WITH a session id still returns `NextTurn`.
- A `Running` opencode task with a session marker but no in-memory id still
  returns `NextTurn`.
- The existing input-mode tests still pass unchanged.

## Task 29 — Close stdin for the one-shot runner

**Goal.** Let the daemon hear an opencode agent. Today it hears nothing.

**Files.** `src/runner/opencode.rs`.

**Why this task exists.** This is a v0.5 defect, not a v0.5.1 one. A live run
found it; no unit test could.

`proc::spawn` pipes stdin and holds it open, because the claude runner steers a
session by writing lines to it. `opencode run` writes NOTHING to stdout while its
stdin is an open pipe that never delivers and never closes. Measured on this
machine on 2026-08-31, same command and same worktree each time:

| stdin | opencode output |
|---|---|
| inherited | NDJSON at once, `sessionID` on the first line |
| pipe, held open, no data | nothing at all in 100 seconds |
| pipe, closed at once | NDJSON at once, `sessionID` on the first line |

Every daemon child gets the middle case. So the daemon never receives a
`sessionID`, never writes a transcript line, never sees a `TurnEnd`, and learns
of the task only from the process exit. The agent still does its work; the
daemon is deaf to it. Every runner test feeds the parser from a recorded
fixture, so the parser is right and the process plumbing was never exercised.

**Detail.**

- `OpenCodeRunner::start` calls `handle.close_stdin()` directly after
  `proc::spawn` returns, and before it returns the session.
- `proc::ProcHandle::close_stdin` already exists at `src/proc.rs:378`.
- Do NOT change the claude runner. It needs stdin open: the prompt goes in as
  the first line and steering uses the same channel.
- Do NOT change `proc.rs`. The shared spawn keeps piping stdin; the one-shot
  runner is what knows it has no steering channel.

**Acceptance criteria.**
- `OpenCodeRunner::start` closes the child's stdin.
- An offline test proves it. Use a fake program that reads its stdin to the end
  and only then prints one NDJSON line. With stdin left open the test would
  block; with stdin closed the runner reports the line. Give the test a bounded
  wait so a regression fails instead of hanging the suite.
- The claude runner still writes its prompt on stdin, and its existing tests
  pass unchanged.

## Task 30 — Tickets view

**Goal.** Add complete issue review and ticket shaping inside the terminal UI.

**Files.** `src/model.rs`, `src/gh.rs`, `src/poll.rs`, `src/ticket.rs`,
`src/sock.rs`, `src/state.rs`, `src/tasks.rs`, `src/config.rs`,
`src/runner/`, `src/daemon.rs`, `src/tui/`, and `docs/v0.5/`.

**Issue list.**

- Key `4` opens the Tickets view.
- The list includes every open issue from every configured repository.
- The list excludes GitHub pull request objects.
- The list groups untouched, `to-refine`, and `refined` issues in that order.
- An untouched issue has neither workflow label.
- The `to-refine` label wins when both workflow labels exist.
- Each group sorts by update time, repository alias, and issue number.
- Key `/` searches the repository, number, title, and label text.
- Key `Enter` opens the selected issue.

**Issue focus.**

- The focus shows the title, description, labels, author, assignees, update
  time, and GitHub reference.
- The GitHub URL is a reference only.
- Key `e` opens the title and description editor.
- `Ctrl+S` saves the direct content edit.
- AIF fetches current GitHub content before each content update.
- A remote content change opens a comparison view.
- Key `g` keeps the GitHub version.
- Key `p` fetches GitHub again before it reapplies the pending version.
- Key `Esc` returns one level and keeps a pending local version.

**Labels.**

- Key `l` opens the repository label picker.
- Key `Space` applies one label toggle immediately.
- AIF treats removal of an absent label as success.
- Key `n` opens the new-label form.
- The form accepts an optional `#` before exactly six hexadecimal digits.
- AIF creates the repository label before AIF attaches it.
- A partial failure states that creation succeeded and attachment failed.
- A catalog failure retains the last valid catalog.
- The picker uses each color from the GitHub label catalog.

**Ticket chat.**

- Key `c` starts or resumes one Claude chat for the focused issue.
- Chat can start before a workflow label exists.
- Ticket chat allows only the `Read`, `Glob`, and `Grep` tools.
- `[ticket_chat] model` selects the Claude model.
- A missing setting uses the refine model only when Claude runs refinement.
- An invalid chat setting does not disable ticket review.
- `prompts/ticket-chat.md` customizes the ticket prompt.
- A new `to-refine` transition sends one refinement message to the same
  session.
- A later transition sends a new message after label removal.
- The conversation survives a daemon restart.
- The conversation ends when `refined` appears or the issue closes.

**Proposals.**

- Claude returns one final `aif-ticket-proposal-v1` data block.
- AIF rejects a malformed, quoted, duplicated, split, or incomplete block.
- AIF stores the proposal with its original title and description.
- The state file does not store the full transcript.
- Key `a` applies the latest shown proposal through the content update service.
- Proposal application never changes labels.
- A stale proposal opens the standard content comparison.
- A successful application notifies the same Claude session.
- A notice failure does not undo the GitHub update.

**Wire and display.**

- `StateView` contains compact ticket summaries.
- The daemon sends separate details, label, and result messages.
- Every user change shows pending, success, conflict, partial failure, or
  failure with a word and a symbol.
- AIF ignores a poll that starts before a confirmed mutation.
- The focus splits at 104 columns and stacks below that width.
- The terminal uses the approved true-color Tickets palette.
- A list without an open chat has no transcript poll deadline.

**Acceptance criteria.**

- Offline tests cover all list, search, detail, edit, label, chat, restart, and
  proposal paths.
- Render tests cover 120-column and 80-column terminals.
- Old configuration files and old state files load without changes.
- Formatting, Clippy, all tests, and the installer test pass.
