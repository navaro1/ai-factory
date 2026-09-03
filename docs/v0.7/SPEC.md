# The Theory Governor

Date: 2026-09-03 · Status: Ready · Scope: The theory loop, the trust loop, and the ladder, as PR-sized chunks on top of v0.6 · Predecessors: docs/superpowers/specs/2026-09-03-theory-governor-design.md, docs/v0.6/SPEC.md

---

## 1. Objective & Non-Goals

**Objective.** Make the operator the limiting step of the factory, by design, at the system level. The factory keeps a human-owned model of each repository, takes a prediction from the operator before every change, reports the delta after it, bounds throughput by the operator's open deltas, verifies every change at runtime with factory-run measurements, and turns every human intervention into a ticket, a measurer, or a rule. The design record names every decision; this spec grounds them in the code and cuts them into chunks.

**What NOT to build (non-goals):**
1. **No restyle of the existing views.** The Amber CRT theme applies to the Theory view and to the new inbox items. The pipeline, inbox feed, tickets, settings, and session keep `THEME`, and their helpers stay where they are.
2. **No several operators.** One daemon, one operator, one window. Every record still carries its GitHub author.
3. **No plugins.** The verification map names a measurer by command. A plugin is a later source of commands.
4. **No CI polling.** CI failures on the main branch are a later event source.
5. **No promotion step.** Moving a shadow model into the code repository is a later feature. The stable IDs keep the door open.
6. **No comment polling.** The 60-second poll fetches issues and PRs, as today. The factory reads the comments of one theory record on three paths only: at first sight of a theory label on that record, inside a task dispatch that needs them, and in the one daily sweep.
7. **No new GitHub timeline or checks calls.** Labels drive every gate. Comments carry content.
8. **No worktree replacement.** Cloud agents stay out of scope.

---

## 2. Context & Sources (grounding)

**Reality check (2026-09-03).**
- The repository is at `main` `2d15a37`, clean. The crate is `aif` 0.6.0, edition 2021, with bins `aifd` and `aif` (`Cargo.toml:1-13`).
- Dependencies: anyhow 1, clap 4, crossterm 0.28, pulldown-cmark 0.13.4, ratatui 0.29, serde 1, serde_json 1, toml 0.9, toml_edit 0.22, uuid 1 (`Cargo.toml:16-25`). No glob crate exists. `toml` parses only the configuration today (`src/config.rs:270`).
- `./check.sh` runs `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, then `tests/install.sh` (`check.sh:6-16`). A cold build and test loop takes about 30 s; an incremental one about 4 s.
- No `$EDITOR` use, no process spawn, and no suspend path exist under `src/tui/` (grep of `EDITOR` and `Command::new` over `src/tui/` returns nothing). The terminal-mode surface is `RawMode`, `enable_terminal_with`, and `restore_terminal_with`, which take closures over real crossterm calls (`src/tui/mod.rs:737-801`).
- No `git log`, `write-tree`, or command timeout helper exists in the crate. `Exec::run` blocks on `Command::output()` (`src/exec.rs:49-63`). Long-lived children go through `proc::spawn` with a stop escalation (`src/proc.rs:148-157`, `src/proc.rs:438-505`). With git 2.34.1, `git add -A --intent-to-add` then `git write-tree` on the live index misses new files and mutates the agent's index; a temporary `GIT_INDEX_FILE` does not.
- The factory fetches no comments anywhere. The only comment traffic is the outbound `gh api -X POST .../comments` in `Daemon::post_issue_comment` (`src/daemon.rs:2583-2605`).
- The worktree layer owns the name `.aif/` in every checkout and git-excludes it (`src/worktree.rs:18-21`, `src/worktree.rs:128-130`). So the theory files cannot live under `.aif/`.
- `HEAD` of `RepoConfig.path` is whatever branch the operator has checked out. The default branch is `origin/HEAD` through `default_base`, with a fallback to `HEAD` (`src/worktree.rs:437-452`).
- The old repository format under `~/Workplace/borsuk/.aif/` is inspiration only. Nothing there is read.

**Existing code / consumer contracts (verified by reading the cited lines).**
- Configuration: every raw table carries `deny_unknown_fields` (`RawConfig` `src/config.rs:428`, `RawRepo` `src/config.rs:546`). `RepoConfig` holds `alias`, `path`, `owner_repo`, `lanes`, `release`, `role_overrides` (`src/config.rs:191-199`), next to `ReleasePolicy` (`src/config.rs:159-170`). `ExecutionRole` lists the six roles with `ALL`, `table_name`, and `stage()`, which returns `Some(Stage)` for every `stage.*` table and `None` for the ticket tables (`src/config.rs:18-55`); `Config::parse` requires each (`src/config.rs:268-290`). `Config::resolved_role` merges globals and overrides (`src/config.rs:222-259`).
- Binding: `ResolvedRoleSettings` (`src/config.rs:127-133`) binds at `Daemon::bind_task_role` (`src/daemon.rs:3049-3066`), persists through `StateFile.role_bindings` (`src/state.rs:71-73`) next to `last_fire_ms` (`src/state.rs:65-67`), and is dropped on task end (`src/daemon.rs:862`, `src/daemon.rs:1671`).
- Settings view: `Settings::warnings` returns static warning strings (`src/tui/settings.rs:1022-1040`), rendered as `WARNING: …` (`src/tui/settings.rs:889-894`); tests at `src/tui/settings.rs:1697-1725`. `SettingsView::from_config` ships settings to the TUI (`src/sock.rs:117-158`).
- Doctor: `Check { label, status, detail }` (`src/doctor.rs:136-144`), assembled by `report` (`src/doctor.rs:167-200`), printed by `print_report` (`src/doctor.rs:203-207`). Flags `--clean` and `--yes` sit on `Command::Doctor` (`src/bin/aif.rs:59-70`). `tests/cli.rs:68-76` pins the doctor help.
- State directory: `config::state_dir()` (`src/config.rs:1499-1501`) holds `worktrees/`, `logs/`, `state.json`, and sometimes `daemon.sock`; no cache directory exists.
- Daemon loop: `Daemon::drive` runs the fixed pass (`src/daemon.rs:454-468`); `next_deadline` enumerates its two sources inline, train deadlines and idle reaps (`src/daemon.rs:475-505`); only `Interval` trains produce a deadline (`src/trains.rs:199-212`).
- Gates: `refine_ready`, `implement_ready`, `review_ready`, `release_ready` read `issue.labels` and `pr` fields (`src/gates.rs:36-58`); `parse_blocked_by` (`src/gates.rs:76-100`); `GateTracker::observe` is edge-triggered on a `GateKey` whose review trigger is `head_sha\0head_ref` (`src/gates.rs:157-251`); `Daemon::admit_ready` turns `ReadyWork` into tasks and spawns no process (`src/daemon.rs:800-812`), then pins the review ticket set (`src/daemon.rs:813-843`).
- Dispatch: `next_eligible` skips held tasks (`src/daemon.rs:1149-1184`); `dispatch_one` picks the cwd by `task.stage`, ensures it, then renders the prompt (`src/daemon.rs:1230-1299`); `launch_task` binds the role first, then builds the `Job` and calls `runner_factory.build(&role)` (`src/daemon.rs:1306-1339`); `task_cwd` mirrors the cwd choice (`src/daemon.rs:2994-3006`); `execution_role` maps stage and purpose to a role (`src/daemon.rs:3022`). `sched::can_start` checks pause then capacity and returns a `Verdict` with a `Reason` (`src/sched.rs:159-196`); `Limits` is keyed by `Stage` (`src/sched.rs:22-53`). `Paused` has no repository scope, and a test rejects `{"scope":"repo"}` (`src/sched.rs:91-141`, `src/sock.rs:1927-1938`).
- `Action::Refine` today adds `to-refine` through `GhClient` and lets the poll gate queue the task (`src/daemon.rs:1652-1679`).
- Run events: `on_exit_event` routes `ok == false` into `fail_run`, then `fail_task`, then a requeue up to `MAX_ATTEMPTS` (`src/daemon.rs:1558-1588`, `src/daemon.rs:1346-1371`, `src/tasks.rs:21`).
- Tasks: `Task` (`src/tasks.rs:75-103`), `TaskPurpose { Pipeline, TicketCreate, TicketChat }` (`src/tasks.rs:63-71`), `scoped_id` (`src/tasks.rs:175-177`), `ticket_chat_id` (`src/tasks.rs:180-182`), `upsert_with_id` (`src/tasks.rs:240-266`). The ticket conversation state is keyed by ticket (`TicketConversationState`, `src/state.rs:34-35`).
- Prompts: six consts (`src/prompts.rs:15-115`); `render_prompt` supplies the placeholder sets (`src/daemon.rs:3106-3208`); `fill_template` rejects an unknown placeholder (`src/daemon.rs:3357-3383`); `prompt_template(stage)` reads `<prompts_dir>/<stage>.md` and `ticket_chat_prompt_template` is a near copy (`src/daemon.rs:3211-3230`); the docs copies are pinned byte for byte (`src/prompts.rs:196-221`).
- Agent output: `parse_ticket_proposal` is the one marker-block parser: single block, must end the text, no backticks, one JSON line, `deny_unknown_fields` (`src/ticket.rs:619-658`). The daemon buffers `RunEvent::Text` per task id in `TicketTurnText`, gated by `is_ticket_chat` (`src/daemon.rs:101-106`, `src/daemon.rs:1474-1488`), and consumes it in `finish_ticket_proposal_turn` (`src/daemon.rs:2838-2887`).
- GitHub: `GhClient` (`src/gh.rs:90`) paginates with ETags through `fetch_list` (`src/gh.rs:236-305`); writes are `add_label` (`src/gh.rs:311-328`), `remove_label` (`src/gh.rs:357-366`), `create_label` (`src/gh.rs:173-199`), `update_issue` (`src/gh.rs:202-228`), and `create_issue`, which has no production caller (`src/gh.rs:396-420`). `TicketController::create_label` already holds a create-then-422-then-fetch label flow (`src/ticket.rs:417-560`, the step at `:498-540`). `Issue` and `Pr` carry no comments (`src/model.rs:103-151`). `RepoSnapshot` (`src/model.rs:154-160`); `Snapshot.repos` is keyed by alias and every per-repository derivation iterates it. One poller thread per configured repository (`src/poll.rs:90-115`); `DaemonMsg::Polled` (`src/poll.rs:37-58`).
- Links: `Links::derive` rebuilds ticket-PR pairs every poll from bodies and branch names, persisted nowhere (`src/links.rs:33-45`). This is the precedent for every derived table in this spec.
- Decisions: `DecisionKind { Permission, Question, Stuck, NeedsHuman, ReleaseGate }` (`src/decisions.rs:20-67`); `Response` (`src/decisions.rs:228-259`); `validate` pins the legal pairs (`src/decisions.rs:274-300`). `derive_needs_human` rebuilds rows from labels each poll (`src/daemon.rs:760-793`); `resolve_needs_human` posts a comment and removes the label (`src/daemon.rs:2549-2581`).
- Trains: `Train::fire` (`src/trains.rs:223-247`); `Daemon::fire_train` (`src/daemon.rs:2915-2952`); `finish_train` (`src/daemon.rs:1377-1399`); `ensure_train` resets the train worktree to the base (`src/worktree.rs:248-285`).
- Worktrees: `WORKTREE_KINDS` with `issue-` and `pr-` (`src/worktree.rs:44-63`); `ensure_on` takes a branch and cuts it from the default base (`src/worktree.rs:201-239`); `ensure_issue` (`src/worktree.rs:167-174`); `ensure_pr` fetches `pull/<n>/head` (`src/worktree.rs:182-191`); the `git` helper (`src/worktree.rs:509-515`); the doctor scans by `WORKTREE_KINDS` (`src/doctor.rs:1135-1169`).
- Socket: `StateView` with `#[serde(default)]` on later fields (`src/sock.rs:84-114`); `TaskView` has no reason field (`src/sock.rs:895-921`); `StateInput::build` destructures every field (`src/sock.rs:354-539`); `Action` (`src/sock.rs:1025-1135`) nests ticket commands as `Action::Ticket(TicketAction)`; the round-trip test list `every_action()` (`src/sock.rs:1713`); `Push` (`src/sock.rs:1005-1019`); `WIRE_PROTOCOL_REVISION = 1` and its rule (`src/sock.rs:44-52`).
- Runners: `Runner` and `Session` traits (`src/runner/mod.rs:182-214`); `RunnerFactory::build(&role)` is the one seam that selects a runner (`src/runner/mod.rs:48-66`); `Capabilities` per harness (`src/runner/mod.rs:25-45`).
- TUI shell: `View { Pipeline, Session, Inbox, Tickets, Settings }` (`src/tui/mod.rs:60-72`); `App` (`src/tui/mod.rs:215-246`); `handle_key` with one digit arm per view (`src/tui/mod.rs:370-523`); `render_with_clock` dispatches on the view (`src/tui/mod.rs:1039-1107`); `draw_header` and `tab_span` (`src/tui/mod.rs:1110-1156`); the help array (`src/tui/mod.rs:1253-1284`); the footer hint (`src/tui/mod.rs:1184-1187`).
- Theme: `THEME` with true colors (`src/tui/theme.rs:34-44`), locked by a test (`src/tui/theme.rs:76-84`). The hand-drawn boxes of the release lane stay in `src/tui/pipeline.rs:1279-1301`.
- Inbox: `Inbox` state with `picks` and `checks` accumulators (`src/tui/inbox.rs:64-82`); `row_key` pairs kind and key (`src/tui/inbox.rs:426-511`); `submit_question` is the multi-step precedent (`src/tui/inbox.rs:823-872`); five exhaustive matches over `DecisionKind`: `kind_label` (`src/tui/inbox.rs:1155-1163`), `feed_message` (`:1083-1103`), `choice_lines` (`:1364-1432`), `feed_actions` (`:1435-1449`), `footer_text` (`:1740-1806`); one silent match in `App::inbox_row_owns` (`src/tui/mod.rs:546-563`).
- Ticket chat: `c` starts or attaches (`src/tui/tickets.rs:373-394`); the proposal offer and `a` apply (`src/tui/tickets.rs:886-898`, `:395-416`).
- TUI tests: `TestBackend` snapshots through `render_to_string` (`src/tui/pipeline.rs:1839-1870`) and `sample_view` (`src/tui/pipeline.rs:1688-1833`); inbox render helpers (`src/tui/inbox.rs:1956-1986`).

**External references (durable copies, if any).**
- `docs/superpowers/specs/2026-09-03-theory-governor-design.md` — the design record: 27 decisions, 12 rules, vocabulary, and the build order. Every chunk below cites its section.
- `research/2026-09-02-github-closing-keywords.md` — the `pull/<n>/head` fetch and the closing keyword rules that the link parser follows.

---

## 3. Requirements & Acceptance Criteria

Functional requirements (EARS). The design record section is in brackets.

Stance and configuration:
- **R1** — The repository shall carry `docs/STANCE.md`, linked from the first section of the README, whose vocabulary table names every `TheoryConfig` field, every label, and every Theory view panel of this spec. [§1]
- **R2** — WHEN `factory.toml` names a repository, the parser shall accept `governor` (`"on"` or `"off"`, default `"on"`), `window` (default 3), `theory` (`{ repo, path }`, optional), `sweep` (`{ days, after_train }`, defaults 7 and true), `cards` (`{ per_day, stale_days }`, defaults 3 and 30), and `interview` (`{ weekday, minutes }`, defaults `"monday"` and 20); `path` is required whenever `theory` is present; unknown fields stay rejected; zero values are rejected. The parser shall accept two new role tables, `[theory.audit]` and `[theory.chat]`, optional until the chunk that first dispatches each role makes it required. [§8]
- **R3** — WHILE a repository has `governor = "off"`, the Settings view shall show `WARNING: the theory governor is off`, the doctor shall print one `Warn` line per such repository, and every gate, label, event, card, and sweep of this spec shall be inert for it. [§1 rule 11]
- **R4** — WHEN the polled default branch of a repository with the governor on moves, the daemon shall read `theory/model.toml` at that commit through `git show <default_base>:theory/model.toml`, ship the parsed entries, or the first error, or `theory/model.toml: missing`, in the state view, and never stop on a parse error. [§4.1]
- **R5** — The model file shall hold `[[entry]]` tables, one internally tagged enum on `kind`: `invariant { constrains }`, `state`, `transition { from, to }`, `boundary { sides, paths }`, `failure { crosses }`, each with `id`, `title`, and `statement`. `paths` are `globset` patterns. A duplicate `id`, a dangling reference, a boundary without `paths`, an invalid pattern, or an unknown kind is a parse error that names the entry. [§4.1, §10]
- **R6** — WHEN a PR carries the label `model-pr` or a head branch `aif/<alias>/model-<n>`, the daemon shall skip both prediction gates for it, dispatch its review under the `theory.audit` role with the model-PR audit prompt, and let the release train merge it. [§4.1]
- **R7** — WHEN a pipeline task starts, the daemon shall bind the model commit next to the role in one `TaskBinding`; WHEN a later poll shows the model changed in an entry of the task's predicted areas before the review ends, the daemon shall open one theory event, take no automatic restart, and leave the task `Running`. [§4.1]
- **R8** — WHEN the daemon renders a stage prompt, it shall fill `{model}` with the slice: for refine the entries of the short prediction's areas plus one hop; for implement the entries of the full prediction's areas plus one hop; for review the implement slice plus the areas whose `paths` match the diff. One hop means every invariant and failure that names an area or its states, plus every boundary that touches it. [§4.1]

Predictions:
- **R9** — WHEN the operator presses refine on a governed ticket, the TUI shall take one line of short prediction first; the daemon shall validate its areas against the model, post it as an `<aif-prediction-v1>` comment, add `theory-short`, then add `to-refine`, and let the poll gate queue the task; a named area with no entries, or a model in error, posts nothing and shows the reason; WHILE a `to-refine` ticket lacks `theory-short`, refine shall not dispatch and the pipeline row shall show `awaits short prediction`. [§4.3]
- **R10** — WHEN a ticket gains `refined`, the TUI shall offer the full prediction: a template file with five slots, the candidate entry IDs of the areas named in the short prediction as shipped in the state view, and a `sure`/`unsure` tag per slot, opened in `$EDITOR`; on save the TUI shall reject a malformed file at once; the daemon shall validate the areas, post it as an `<aif-prediction-v1>` comment, and add `theory-full`; WHILE a `refined` ticket lacks `theory-full`, implement shall not dispatch. [§4.3]
- **R11** — IF a prediction names an area with no entries, THEN the daemon shall refuse the post, the gate shall hold, and the Theory view shall show `area <id> has no entries` with the bootstrap action. [§4.2]
- **R12** — WHILE a ticket awaits its short prediction, or awaits its full prediction, the ticket chat action shall be disabled with the reason on screen; the system shall NOT post a prediction from any agent output. [§3 rule 4]

Delta and window:
- **R13** — WHEN the daemon dispatches a review of a PR whose ticket carries `theory-full`, it shall fill `{prediction}` with both predictions and require the agent to end its report with one `<aif-delta-v1>` block: per-slot outcome (`hit`/`miss`), the entry IDs the diff touched, `violations` of `{ entry, finding }`, and one question; the daemon shall parse the block with the ticket-proposal strictness, post it as a comment, and add `delta-open`. A review of a PR without `theory-full` renders `{prediction}` as `none`, requires no block, and adds no label. [§4.4]
- **R14** — WHILE the count of records labeled `delta-open` or `event-open` in a repository is at or above `window`, `sched::can_start` shall refuse that repository's implement tasks with `Reason::WindowFull`, the task view shall carry `hold = "window full"`, refine and review shall continue, and the Theory view shall show the gauge and the pause. [§4.5]
- **R15** — WHEN a delta has only hits and no violation, the inbox shall show a `DELTA` item that closes on one confirmation: the daemon removes `delta-open` and posts the confirmation comment; WHEN a delta has a miss, the inbox shall show one `THEORY` item per miss with a three-step answer: the cause (`model`, `pr`, `recall`), the entry ID, and the rung (1, 2, 3); `pr` posts the finding as a comment, cancels the review task, and re-queues the implement task of every linked ticket; `recall` closes on the answer; the label `delta-open` leaves only when every miss and violation of the record has an answer and no `model` answer stays open; `model` closes only when `git log` of the theory repository's default branch shows a commit after the answer whose diff of `theory/model.toml` mentions the entry ID. [§4.6]
- **R16** — The daemon shall open a theory event, with the same answer shape, through one `open_event` function that takes a record key, an item or the repository record `<alias>/theory`, for: a model violation reported by review, a `needs-human` label, a release train failure with the areas of the batch diff, a new ticket labeled `bug` with the entries the audit role maps to it, and a mid-flight model change. [§4.6]

Bootstrap and edits:
- **R17** — WHEN the operator presses the bootstrap action, the TUI shall start a `theory.chat` task with the bootstrap prompt in the theory repository checkout; the agent shall end a turn with one `<aif-model-proposal-v1>` block of entries; the daemon shall write the entries to the open model branch, open or update the model PR labeled `model-pr`, and the audit review shall follow. The prompt shall forbid claims the operator did not state. [§4.2]
- **R18** — WHEN the operator presses the edit-model action, the TUI shall restore the terminal, open `$EDITOR` on `theory/model.toml` in the model worktree, re-enter raw mode on return, validate the file, and on success ask the daemon to commit, push `aif/<alias>/model-<n>`, and open or update the model PR; the open model branch is derived from the snapshot, never stored; a validation error shall show the entry and the reason; a repository without `theory/` gets its first model PR this way. [§9]

Shadow mode:
- **R19** — WHILE `theory.repo` is set, every theory record shall go to a shadow issue in that repository titled `<alias>#<n>`, created on first use; the labels of this spec and the routed agent blocks shall land on the shadow issue; the code repository shall receive no comment and no label from this spec; the shadow issues shall be polled by one more poller whose snapshot lives in a separate map, never in `Snapshot.repos`; the doctor shall print one line per shadow repository. [§4.9]

Cards, interview, audit:
- **R20** — Once per day, one persisted cadence shall run one sweep that reads the theory records updated in the last `stale_days` days and derives: the cards of the day, at most `cards.per_day` per repository, from a PR merged since the last sweep with no prediction by the operator and from an entry that no prediction touched for `stale_days`; the calibration share; the rung counts; and the events per day. Each card is an inbox item; the audit role grades the answer against the diff or the code; a miss opens a theory event. [§4.7]
- **R21** — Once per week on `interview.weekday`, a persisted cadence shall queue one `theory.chat` task with the interview prompt, whose agent asks the operator about the model and the merged PRs of the week and ends with one `<aif-event-v1>` block per gap; the Theory view shall show the waiting interview with a join key. [§4.7]
- **R22** — The `theory.audit` role shall run a weekly sweep per repository from a persisted cadence, and a scoped sweep after each release train when `sweep.after_train` is on; `aif doctor --audit <alias>` shall queue one full sweep; every event posts to its record at once, and the inbox shows at most `cards.per_day` open sweep events per day with the remainder count on screen. The model PR review and the card grading are in R6 and R20. [§4.8]

Trust loop:
- **R23** — The daemon shall read `theory/verify.toml`: `[[area]]` tables with `id`, `boundary`, `statement`, `[[area.property]]` (`id`, `rule` of `hold` or `not_worsen`, `policy` of `error`, `ratchet`, `observe`, optional `threshold`), `[[area.measurer]]` (`id`, `command`, `mode` of `fast`, `pr`, `full`, `timeout_s`), and `skills`; an area whose `boundary` is not a boundary entry is a parse error. [§5.1]
- **R24** — The daemon shall run a `measure` task without a harness: it runs one measurer command in the item's worktree through the script runner with the timeout, treats every exit as complete, parses one JSON object per stdout line as `{ id, value, unit, direction }`, stores the records under `<state_dir>/measure/<alias>/<tree>/<area>-<measurer_hash>.json` keyed by the tree hash of a temporary index and a hash of the measurer's command, mode, and timeout, and honours `[measure] limit`. [§5.2]
- **R25** — WHEN a PR enters review, the daemon shall have "before" records at the merge base and "after" records at the head for every area whose `paths` match the diff, compare them into `new`, `resolved`, `worsened`, `improved`, `unchanged`, `incomparable`, apply the policy, and post one `<aif-measure-v1>` comment on the theory record of the PR; a new head sha triggers a new "after". [§5.2]
- **R26** — WHEN an agent runs `aif measure [--area <id>]` inside a worktree, the command shall send `TheoryAction::Measure` over the socket, the daemon shall run the touched areas at the worktree's tree hash, and the command shall print the comparison against the merge base in agent-text form. [§5.2]
- **R27** — WHEN a governed review dispatches, the daemon shall check the PR in `dispatch_one` after the cwd exists and before the prompt: a `## Why` section is required; a `## How` section, or a heading that starts with `Implementation`, is forbidden; a diff that lists `theory/model.toml`, `theory/verify.toml`, or `theory/rules.md` off a model branch is forbidden; a failure posts one finding comment and re-queues the implement task. [§5.3, §1 refusal 2]
- **R28** — Implement shall run measurers in `fast` mode, review in `pr` mode, and the release train in `full` mode on the train worktree before the merge; a stage runs every measurer whose mode is at or below its tier, `fast` < `pr` < `full`; a `worsened` result under an `error` policy shall stop the train and open a theory event on the record of the PR whose diff touches that area, else on the repository record. [§5.2]
- **R29** — WHEN the operator presses the area action, the TUI shall start a `theory.chat` task with the area prompt that turns a stream of consciousness into one `<aif-area-proposal-v1>` block; the daemon shall write it to `theory/verify.toml` on the model branch and open or update the model PR; the first measurement of a new measurer shall wait for one inbox approval, persisted in `state.json`. [§5.1]
- **R30** — Every stage prompt shall carry the complete placeholder set from the chunk that introduces `{model}`: `{model}`, `{prediction}`, `{comparison}`, `{skills}`, `{rules}`, `{why_rule}`, each with an empty rendering until its chunk fills it; the prompts are rewritten and their docs copies pinned once. [§5.1, §5.2]

Ladder:
- **R31** — WHEN a theory event answer carries rung 1, the daemon shall create a ticket labeled `to-refine` and `ladder-1` with the event text; rung 2 creates a ticket labeled `to-refine` and `ladder-2` for a measurer of the named area; rung 3 adds the label `ladder-3` to the record, appends one rule to `theory/rules.md` on the model branch, and opens or updates the model PR; the rung-2 area comes from the answer, pre-filled when the entry maps to one area; `{rules}` renders the file. [§6]
- **R32** — The Theory view shall show events per rung over the last 30 days from the `ladder-*` labels, the events per day, and the calibration share: sure slots that hit, over all sure slots, all from the daily sweep. [§6, §9]
- **R33** — The TUI shall carry a Theory view, tab `6`, in the Amber CRT look: a header strip with the governor state, the window gauge, and the calibration; a MAP panel per area with `h`/`l` to cycle areas and `j`/`k` to move the entry cursor; a DELTAS panel; a LADDER panel; an AREAS panel; and the keys `p` predict, `e` edit model, `b` bootstrap, `s` area, `i` join interview. [§9]

Non-functional rules. Each has an ID and a tracing chunk.
- **N1** — The daemon shall run every new derivation per poll and persist none of it, like `Links` (`src/links.rs:1-10`). The persisted exceptions are `state.json` entries: the task binding, the cadence list, and the measurer approvals.
- **N2** — The factory shall fetch comments on three paths only: at first sight of a theory label on a record, inside a dispatch that needs them, and in the daily sweep; at most one page of 100 per record, with an ETag.
- **N3** — The wire revision stays 1: every new `StateView` field carries `#[serde(default)]`, every new `Action`, `TheoryAction`, `Push`, and `Response` variant joins the round-trip test, and an old client ignores the new fields (`src/sock.rs:44-52`).
- **N4** — A measurer that exceeds `timeout_s` produces `incomparable` records with the reason, and the measure task ends `Done` at attempt 1.
- **N5** — The `$EDITOR` path restores raw mode on every exit path, including a non-zero editor status and a missing editor.
- **N6** — `./check.sh` passes after every chunk.

Acceptance (Given/When/Then, representative):
- *Given* `governor = "off"` on `borsuk`, *when* the settings view shows `borsuk`, *then* it prints `WARNING: the theory governor is off`, and *when* `aif doctor` runs, *then* one `Warn` line names `borsuk`.
- *Given* a model with a transition whose `to` names no state, *when* the daemon polls, *then* the state view carries `theory/model.toml: T-3: to names unknown state S-9` and the daemon keeps running.
- *Given* a `to-refine` ticket without `theory-short`, *when* `drive` runs, *then* no refine task exists and the row shows `awaits short prediction`.
- *Given* three records labeled `delta-open` and `window = 3`, *when* a refined ticket with `theory-full` waits, *then* `can_start` returns `WindowFull` for implement and the gauge reads `3/3 · IMPLEMENT PAUSED`.
- *Given* a miss answered `model` on `INV-3`, *when* the default branch gains a commit whose diff of `theory/model.toml` contains `INV-3`, *then* the next poll removes `delta-open`.
- *Given* a measurer that prints `{"id":"poll_p95","value":12,"unit":"ms","direction":"lower"}` at the base and `14` at the head under `ratchet`, *when* the comparison runs, *then* the record is `worsened` and the comment shows `poll_p95 12 → 14 ms worsened`.

---

## 4. Design (HOW)

**Architecture** — the daemon derives every theory table per poll, ships it in the state view, gates dispatch on labels, and posts records as comments. The TUI renders and takes the operator's input. Agents read slices and return marker blocks. Scripts measure; the daemon runs them.

```
src/config.rs            TheoryConfig beside ReleasePolicy; RawRepo gains the six fields;
                         RawTheory { audit, chat } as optional role tables; ExecutionRole gains
                         TheoryAudit and TheoryChat, both with stage() == None
src/theory/model.rs      new: Model, Entry enum, parse, validate, slice, areas_for_paths
src/theory/verify.rs     new: VerifyMap, Area, Property, Measurer, parse, validate
src/theory/records.rs    new: label and block constants, TheoryRecords::derive, labels_of,
                         open_count, comment builders and parsers, check_pr
src/theory/blocks.rs     new: parse_block(tag, text) -> Result<String, BlockError>
src/theory/measure.rs    new: Record, parse_lines, cache, tree_hash, compare, apply_policy, agent_text
src/theory/cadence.rs    new: Schedule, ScheduleKind, due, fire
src/gh.rs                fetch_comments with ETag; post_comment; create_label_if_missing (moved
                         from ticket.rs); create_issue gains labels
src/gates.rs             refine and implement predicates take &TheoryRecords
src/sched.rs             Reason::WindowFull; can_start takes the window map
src/daemon.rs            theory cache per repo; PurposeSpec table; open_event; derive_label_rows;
                         wants_final_block; first-sight fetch; theory_record; measure and audit
                         dispatch; shadow routing; cadence firing
src/tasks.rs             TaskPurpose gains Measure, Audit(AuditJob), Interview, Bootstrap, Area
src/decisions.rs         DecisionKind gains DeltaHit, TheoryEvent, Card, FirstRun; Response gains
                         Confirm, Theory
src/state.rs             TaskBinding replaces the bare role binding; schedules; measurer_approvals
src/prompts.rs           AUDIT_MODEL_PR_PROMPT, AUDIT_BUG_PROMPT, AUDIT_CARD_PROMPT, AUDIT_SWEEP_PROMPT,
                         BOOTSTRAP_PROMPT, AREA_PROMPT, INTERVIEW_PROMPT; the stage prompts reworded once
src/runner/script.rs     new: ScriptRunner over proc::spawn; RunnerFactory::build(role, purpose)
src/worktree.rs          WorktreeKind gains Base and Model; ensure_detached
src/sock.rs              StateView.theory (serde default); TaskView.hold; Action::Theory(TheoryAction)
                         with Predict, EditModel, CommitModel, Chat, Detail, Sweep, Measure;
                         Push::ModelPath, Push::MeasureResult
src/tui/theory.rs        new: the Theory view: strip, map, deltas, ladder, areas
src/tui/crt.rs           new: the Amber CRT palette and frame(title) on BorderType::Double
src/tui/editor.rs        new: edit_file_with(path, editor, restore, enable) and edit_file(path)
src/tui/inbox.rs         presentation(&DecisionKind) replaces the five matches; four new kinds
src/bin/aif.rs           `aif measure`, `aif doctor --audit`
docs/STANCE.md           the stance
docs/v0.7/MIGRATION.md   the upgrade note, written in the chunk that first requires a new table
```

**Data flow and contracts.**
- Labels are the state machine. `theory-short`, `theory-full`, `delta-open`, `event-open`, `model-pr`, `ladder-1`, `ladder-2`, `ladder-3`, `bug`, and the v0.6 labels. The factory creates the theory labels on first use through `create_label_if_missing`.
- `TheoryRecords` is the one read model. `records::derive(config, snapshots, theory_snapshots) -> TheoryRecords` runs per poll like `Links`. `labels_of(alias, kind, number)` returns the labels of the record, from the code snapshot or the shadow snapshot. `open_count(alias)` counts `delta-open` and `event-open`. Every gate, the window, and every row derivation take `&TheoryRecords` and never `issue.labels`.
- `RecordKey::{ Item { kind, number }, Repo }` names a theory record. `Daemon::theory_record(alias, key) -> Result<(owner_repo, number)>` is the one write target. For an item it returns the item itself, or its shadow issue; for `Repo` it returns the repository record, one issue titled `<alias>/theory` in the theory repository. Both are created on first use. Every label and comment write of this spec goes through it. An event with no item, from a stale entry, an interview, a sweep, or a train, lands on the repository record.
- First sight. When a poll first shows a theory label on a record, including at daemon start, the daemon fetches that record's comments once, parses the blocks, and ships them in `TheoryView` as `records: BTreeMap<key, RecordView { short, full, delta, events, answers }>`. The TUI never reads comments. A dispatch that needs a block reads the shipped one or fetches again with the ETag.
- Comments are the content. Every factory-posted comment carries one marker block: `<aif-prediction-v1>`, `<aif-delta-v1>`, `<aif-measure-v1>`, `<aif-event-v1>`, `<aif-answer-v1>`. `parse_block` returns `Result<String, BlockError>` with `Absent` and `Malformed`; `parse_ticket_proposal` maps it to `Option`.
- Agent blocks. `wants_final_block(task) -> Option<&'static str>` names the block a task must end with: the ticket proposal for ticket chat, the delta for a governed review, the model proposal for bootstrap, the area proposal for the area chat, the event for interview and audit. The existing per-task text buffer serves every one of them.
- Purposes. `PurposeSpec { stage: Option<Stage>, role: ExecutionRole, prompt: fn(&Daemon, &Task) -> Result<String>, cwd: fn(&Daemon, &Task) -> Result<PathBuf>, limit: LimitKey, block: Option<&'static str> }`, one const per `TaskPurpose`. `dispatch_one`, `task_cwd`, `execution_role`, `Limits`, and the text buffer consult the spec. A new purpose is one entry.
- Chat keys. `ChatKey::Ticket { repo, number }` and `ChatKey::Theory { repo, purpose, key }` key the conversation state that was keyed by ticket. The three theory chats share the ticket-chat machinery through it.
- Events. `Daemon::open_event(alias, key: RecordKey, TheoryEvent) -> Result<()>` is the only writer of `<aif-event-v1>` and `event-open`. Every source is one function that calls it. `derive_label_rows(repo, records, label, make_row)` rebuilds the inbox rows of one label each poll and serves NeedsHuman, DeltaHit, and TheoryEvent.
- Binding. `TaskBinding { role: ResolvedRoleSettings, model_commit: Option<String> }` replaces the bare role value in the existing map, with serde defaults for the old shape.
- Cadences. `Schedule { kind: ScheduleKind, repo, last_ms: Option<u64> }` is a list in `state.json`. Day boundaries are UTC midnight. `cadence::due(schedules, config, now) -> Option<deadline>` feeds `next_deadline`; `Daemon::fire_cadence(kind, repo)` runs one. The daily sweep, the weekly interview, and the weekly audit are three kinds. "Since the last sweep" means since `last_ms`.
- Model reads. The theory checkout is `TheoryConfig::checkout(repo_path)`: `theory.path` when set, else the repository path. The daemon reads `theory/model.toml`, `theory/verify.toml`, and `theory/rules.md` at `default_base` of that checkout, only when `git rev-parse <default_base>` moved, so an uncommitted local edit is never live.
- Slices. `Model::slice(areas) -> Model` returns the areas' entries plus one hop. `Model::areas_for_paths(paths) -> Vec<&str>` maps diff paths through the boundary `paths` with `globset`. The diff paths come from `git diff --name-only <base>...<head>` in the item's worktree.
- The window. `sched::can_start` takes a `window: &BTreeMap<String, (usize, usize)>` next to `Limits` and returns `Verdict::No(Reason::WindowFull)` for an implement task at or above the cap. The reason lands in `TaskView.hold`.
- Measurements. A record is `{ id, value: f64 | null, unit, direction: "lower" | "higher" }`; a malformed stdout line yields one `incomparable` record with the reason, and a run never fails. `compare` yields one state per id, and `MeasureVerdict` is the policy result. Modes are tiers: a stage runs every measurer whose mode is at or below its own, `fast` < `pr` < `full`. Policy: `error` fails the verdict on `worsened` or `new` under `hold`; `ratchet` fails on `worsened`; `observe` never fails. The measure task runs through `ScriptRunner`, which `RunnerFactory::build(role, purpose)` returns for `Measure` with a synthetic role; its `Job.prompt` carries the command; every exit is complete.
- The editor bridge. `edit_file_with(path, editor, restore, enable) -> Result<EditorOutcome>` takes the two terminal hooks as closures so tests record their order; `edit_file(path)` wraps it with `restore_terminal_with`, `enable_terminal_with`, and `$EDITOR`.
- Model worktrees. `WorktreeKind::Model` at `worktrees/<alias>/model` on the open model branch, derived from the snapshot as the open PR whose head starts with `aif/<alias>/model-`; `WorktreeKind::Base` at `worktrees/<alias>/base-<sha8>` through `ensure_detached`. Both join `WORKTREE_KINDS` with a cleanable rule, so the doctor scans them.
- Inbox presentation. `presentation(&DecisionKind) -> ItemPresentation { label, message, choice_lines, actions, footer, owns_digits }` replaces the five matches and the silent one. A new kind is one arm.

**Extension axes.**

| Axis | State | One variant touches |
|---|---|---|
| Task purposes | open | one `PurposeSpec` const in `src/daemon.rs` |
| Theory event sources | open | one function that calls `open_event` |
| Marker block kinds | open | one tag constant and one wire struct in `src/theory/records.rs` |
| Inbox item kinds | open | one `DecisionKind` arm and one `presentation` arm |
| Cadences | open | one `ScheduleKind` arm in `src/theory/cadence.rs` |
| Measurer sources | open | one command in `theory/verify.toml`; no code |
| Harnesses | closed | none; the script runner selects by purpose |
| Prompt placeholders | closed after R30 | none |

**Error handling / graceful degradation.** A model or map parse error, or a missing model file, ships in the state view, blocks the gates of that repository, and shows `model error` on every held row. A missing theory checkout is a doctor `Fail` and blocks the gates. A failed comment fetch fails that dispatch with the reason, like a worktree error today. A measurer timeout or non-zero exit yields `incomparable` records and a `Done` task. A missing `$EDITOR` shows a toast and posts nothing. A missing label is created on first use; a failed label creation fails that action with the `gh` error.

---

## 5. Boundaries

- ✅ **Always:** GitHub holds every theory record; every derivation rebuilds per poll; only the operator's TUI actions post predictions and answers; only the daemon runs measurers; every new view field carries a serde default; every chunk keeps `./check.sh` green.
- ⚠️ **Ask-first:** worktree removal keeps the doctor preview flow; a model PR merge goes through the release train and its policy; the first run of a new measurer waits for the inbox approval.
- 🚫 **Never:** an agent edits `theory/model.toml`, `theory/verify.toml`, or `theory/rules.md`; the factory posts a prediction from agent text; the factory polls comments; a human reads a diff to catch a defect; the code repository receives a theory label or comment in shadow mode; the existing views change their look.

---

## 6. Open Questions

All questions are resolved. None waits for clarification.

1. **Look** — Amber CRT: amber on near-black, double-line frames, uppercase block titles, block gauges, one white accent for the selected row. (User decision, 2026-09-03.)
2. **Paths** — `theory/model.toml`, `theory/verify.toml`, `theory/rules.md`. Reason: `.aif/` is the git-excluded worktree marker directory (`src/worktree.rs:18-21`).
3. **Defaults** — window 3, cards 3 per day, stale 30 days, sweep 7 days, interview Monday. The fields are configurable.
4. **Spec location** — `docs/v0.7/SPEC.md`, the repository convention that `implement-chunk` reads.
5. **Model version binding** — the default-branch commit of the theory checkout at dispatch, in `TaskBinding` next to the role. Reason: it survives a restart like the role binding (`src/state.rs:71-73`).
6. **Diff paths** — `git diff --name-only` in the worktree, never a GitHub files call. Reason: non-goal 7.
7. **Labels on the code repository in shadow mode** — none. The shadow issue carries everything. Reason: R19.
8. **How to detect a model-only PR** — the label `model-pr` set by the factory, or the branch rule as fallback. Reason: PR files are not polled.
9. **Table names** — `[theory.audit]` and `[theory.chat]`, not `[stage.audit]`. Reason: `ExecutionRole::stage()` returns `Some` for every `stage.*` table (`src/config.rs:46-54`), and the audit role has no pipeline stage.
10. **Glob grammar** — `globset` patterns. Reason: no hand-rolled matcher; one new dependency.
11. **Tree hash** — a temporary index: `GIT_INDEX_FILE=<tmp> git read-tree HEAD`, `git add -A`, `git write-tree`. Reason: the live index misses new files and belongs to the agent.

---

## 7. Chunks and Acceptance Criteria

### C0 — Configuration fields and the model parser
**Status:** `[ ]` pending
**Build:** Add `TheoryConfig { governor, window, theory, sweep, cards, interview }` in `src/config.rs` beside `ReleasePolicy` (`src/config.rs:159-199`), the `RawRepo` fields with defaults (`src/config.rs:547-557`), `path` required whenever `theory` is present, zero-value rejection in the style of `validate_release` (`src/config.rs:987-997`), and `TheoryConfig::checkout(&self, repo_path) -> PathBuf`: `theory.path` when set, else `repo_path`. Add `RawTheory { audit, chat }` as an optional top-level table, `ExecutionRole::TheoryAudit` (`theory.audit`) and `ExecutionRole::TheoryChat` (`theory.chat`) to the enum, `ALL`, `table_name`, with `stage() == None` (`src/config.rs:18-55`); `Config::parse` accepts a file without them; `theory.chat` and `theory.audit` reject `limit` like the ticket roles. Add `globset` to `Cargo.toml`. Add `src/theory/model.rs`: `Model`, `Entry` as an internally tagged enum on `kind`, `parse(text) -> Result<Model, ModelError>` with the R5 checks, and `ModelError` that names the entry.
**AC:**
- A config test parses `[repo.borsuk] governor = "off"` and `window = 5`, defaults the rest, and rejects `window = 0` and `governor = "maybe"` with messages that name the field.
- A config test parses `docs/v0.5/factory.example.toml` unchanged, and one with `[theory.audit]` and `[theory.chat]`; `theory = { repo = "o/r" }` without `path` is rejected; `checkout` returns `path` when set and the repository path otherwise; `theory.chat.limit` is rejected; `ExecutionRole::TheoryAudit.stage()` is `None`.
- A model test parses a four-entry file into the four enum arms and rejects a duplicate id, a transition whose `to` names no state, a boundary without `paths`, a boundary with the pattern `[`, and an unknown kind, each error naming the entry id.
**Depends on:** — · **Traces to:** R2, R5, N6
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

### C1 — Walking skeleton: the model read, the state view, and the Theory view
**Status:** `[ ]` pending
**Build:** In the daemon, on every `apply_poll` (`src/daemon.rs:601-631`) run `git rev-parse <default_base>` on the theory checkout, `TheoryConfig::checkout`, through the `git` helper (`src/worktree.rs:509-515`, `:437-452`); when the commit moved, read `git show <commit>:theory/model.toml` and cache `(commit, Result<Model, String>)` per alias; a missing file caches `theory/model.toml: missing`. Add `StateView.theory: BTreeMap<String, TheoryView>` with `#[serde(default)]` (`src/sock.rs:84-114`) carrying `governor`, `entries: Vec<EntryView>` with the relation arrays, `error`. Add `src/tui/crt.rs` with the Amber CRT palette (`amber #FFB000`, `dim #9A6A00`, `bright #FFD866`, `frame #C98A00`, `white #FFF7E0`, `background #0B0A06`) locked by a test like `src/tui/theme.rs:76-84`, plus `frame(title)` that returns a `Block` with `BorderType::Double` and an uppercase title. Add `View::Theory` as tab `6` (`src/tui/mod.rs:60-72`, `:370-523`, `:1039-1107`, `:1110-1156`) and `src/tui/theory.rs` that draws the header strip `GOVERNOR ON · ENTRIES n · AREAS n` or the error. Add the help line and the footer digit (`src/tui/mod.rs:1184-1187`, `:1253-1284`).
**AC:**
- A daemon test with a `ScriptExec` that answers `rev-parse` and `git show` ships four entries in the state view; a second poll with the same commit runs no `git show`; a moved commit runs one.
- A `ScriptExec` that fails `git show` with `does not exist` ships `error = "theory/model.toml: missing"` and zero entries; a bad file ships its error; the daemon's next `drive` still runs in both cases.
- A TUI test renders `6` and asserts the strip `GOVERNOR ON · ENTRIES 4 · AREAS 1` inside a double-line frame; a palette test asserts the six CRT colors.
- `a_state_view_round_trips_through_json` carries the entries and a view without `theory` parses with an empty map.
**Depends on:** C0 · **Traces to:** R4, R33, N3

### C2 — The governor switch: settings warning and doctor lines
**Status:** `[ ]` pending
**Build:** Ship `TheoryConfig` per repository in `SettingsView::from_config` (`src/sock.rs:117-158`). Add the warning `the theory governor is off` to `Settings::warnings` when the selected repository has the governor off (`src/tui/settings.rs:1022-1040`). Add `theory_checks` to the doctor `report` (`src/doctor.rs:167-200`): one `Warn` per repository with the governor off, one `Fail` per theory checkout that is not a git repository.
**AC:**
- A settings render test with `governor = "off"` on the selected repository asserts `WARNING: the theory governor is off`; the same view with `"on"` shows no warning.
- A doctor test with two repositories, one off, asserts exactly one `Warn` line that names the alias; a test with a missing theory checkout asserts one `Fail` line.
- `tests/cli.rs` still pins the doctor help.
**Depends on:** C1 · **Traces to:** R3

### C3 — The stance document
**Status:** `[ ]` pending
**Build:** Write `docs/STANCE.md` from the design record §1, §2, §3, §6: the thesis, the loops, the governor, the ladder, the twelve rules, the three refusals, and the vocabulary table. Link it from the first section of `README.md`.
**AC:**
- A test over `include_str!("../docs/STANCE.md")` asserts that the vocabulary table names each `TheoryConfig` field (`governor`, `window`, `theory`, `sweep`, `cards`, `interview`), each label constant of `src/theory/records.rs` once it exists, and each panel title (`MAP`, `DELTAS`, `LADDER`, `AREAS`); until C4 lands, the label list is the literal set of this spec.
- The README's first section contains a link to `docs/STANCE.md`.
**Depends on:** — · **Traces to:** R1

### C4 — Prep: records, blocks, comments, labels
**Status:** `[ ]` pending
**Build:** Add `src/theory/blocks.rs` with `parse_block(tag, text) -> Result<String, BlockError>` (`Absent`, `Malformed(&'static str)`) that holds the ticket-proposal rules, and make `parse_ticket_proposal` map it to `Option` (`src/ticket.rs:619-658`). Add `GhClient::post_comment` and replace the hand-rolled call in `Daemon::post_issue_comment` (`src/daemon.rs:2583-2605`). Extract `GhClient::create_label_if_missing(owner_repo, name, color)` from `TicketController::create_label` (`src/ticket.rs:498-540`) and make the controller call it. Add `GhClient::fetch_comments(owner_repo, number, etag)` with one page of 100 and a 304 path. Add `src/theory/records.rs`: the label and block constants, `RecordKey`, `TheoryRecords::derive(config, snapshots, theory_snapshots: &BTreeMap<String, RepoSnapshot>)` where the map is keyed by alias and stays empty until C20, `labels_of`, `open_count`, and the `<aif-prediction-v1>` builder and parser for `{ kind, text, areas }` and `{ kind, slots }`.
**AC:**
- `parse_block` returns `Absent` for text without a block and `Malformed` with a reason for a fenced block, a second block, or a block not at the end; `proposal_parser_accepts_only_one_final_complete_unquoted_block` still passes unchanged.
- A `ScriptExec` test of `post_comment` asserts the exact `gh api` argv and that `resolve_needs_human` still posts through it; the ticket controller's label tests still pass through `create_label_if_missing`.
- A `fetch_comments` test asserts `per_page=100` in the argv, `If-None-Match` on the second call, and the cached page on a 304.
- `TheoryRecords::derive` over a code snapshot and a shadow snapshot returns the shadow labels for a shadowed alias and the code labels otherwise; `open_count` counts `delta-open` and `event-open` and ignores an alias with the governor off.
**Depends on:** C0 · **Traces to:** R9, R13, R14, N2

### C5 — Prep: the daemon seams
**Status:** `[ ]` pending
**Build:** Replace the `is_ticket_chat` gate on the text buffer (`src/daemon.rs:1474-1488`) with `wants_final_block(task) -> Option<&'static str>`, which returns the ticket-proposal tag for ticket chat today. Replace `prompt_template(stage)` and `ticket_chat_prompt_template` (`src/daemon.rs:3211-3230`) with one `prompt_template(name, builtin)`. Add `derive_label_rows(repo, records, label, make_row)` and make `derive_needs_human` (`src/daemon.rs:760-793`) call it. Add `Daemon::theory_record(alias, key)` for code-repository mode: an item returns itself; `Repo` finds or creates the issue `<alias>/theory` in the code repository. Add `TaskBinding { role, model_commit: Option<String> }` in place of the bare role value in `role_bindings` and `StateFile` with serde defaults (`src/state.rs:71-73`, `src/daemon.rs:3049-3066`).
**AC:**
- The ticket proposal tests still pass through `wants_final_block`; a pipeline task with no expected block buffers nothing.
- A prompt override test reads `<prompts_dir>/ticket-chat.md` through the one loader; the docs copies test still passes.
- The needs-human row tests still pass through `derive_label_rows`.
- `theory_record("borsuk", Item { Issue, 142 })` returns `(owner_repo, 142)`; `theory_record("borsuk", Repo)` creates the issue `borsuk/theory` once and returns its number on the second call without a create.
- A `state.json` written by v0.6 with bare role bindings loads into `TaskBinding` with `model_commit = None`; a restart keeps a stored `model_commit`.
**Depends on:** C4 · **Traces to:** R7, R13, R16

### C6 — The editor bridge
**Status:** `[ ]` pending
**Build:** Add `src/tui/editor.rs`: `edit_file_with(path, editor: &[String], restore: impl FnOnce() -> Result<()>, enable: impl FnOnce() -> Result<()>) -> Result<EditorOutcome>` that calls `restore`, runs the editor with inherited stdio, and calls `enable` in a guard that runs on every path; `EditorOutcome::{Saved, Unchanged, Failed(String)}`; and `edit_file(path)` that wraps it with `restore_terminal_with`, `enable_terminal_with` (`src/tui/mod.rs:760-801`), and `$EDITOR` with the fallback `vi`. Add the scratch directory `<state_dir>/edit/`.
**AC:**
- A test passes closures that push `restore` and `enable` into a shared `Vec` and a fake editor script that writes the file; it asserts `Saved` and the order `[restore, enable]`; the same with a script that exits 1 asserts `Failed` and the same order; an unchanged file asserts `Unchanged`.
- A test with an editor command that does not exist returns `Failed` with the reason and the order `[restore, enable]`.
**Depends on:** C1 · **Traces to:** R18, N5

### C7 — The edit-model flow
**Status:** `[ ]` pending
**Build:** Add `WorktreeKind::Model` to `WORKTREE_KINDS` with the path `worktrees/<alias>/model` and a cleanable rule (`src/worktree.rs:44-63`). Derive the open model branch per poll: the open PR whose `head_ref` starts with `aif/<alias>/model-`, else a new `aif/<alias>/model-<uuid8>`. Add `e` in the Theory view: `TheoryAction::EditModel { request, repo }` asks the daemon to ensure the model worktree on that branch through `ensure_on` (`src/worktree.rs:201-239`) and reply with `Push::ModelPath { request, repo, path }`; the TUI runs the editor bridge on `theory/model.toml` there, validates with `Model::parse`, and sends `TheoryAction::CommitModel { repo }`; the daemon commits with the message `Update the model`, pushes, opens the PR through `gh pr create --label model-pr` when none is open, and reports the number on a toast. A repository without `theory/` starts from a template with one comment line.
**AC:**
- A TUI test with a fake editor that edits the file sends `CommitModel`; a fake editor that breaks the TOML shows the error toast and sends nothing.
- A daemon test asserts the git and `gh` call order: `commit`, `push`, `pr create --label model-pr`; a second edit while that PR is open pushes to the same branch and opens no PR.
- A repository with no `theory/` directory and no model PR gets `theory/model.toml` created from the template and a first model PR.
- The doctor lists the model worktree and keeps it while the PR is open.
- `every_action()` gains `EditModel` and `CommitModel`, `Push::ModelPath` round-trips, and the tests pass.
**Depends on:** C4, C6 · **Traces to:** R18, N3

### C8 — The short prediction and the refine gate
**Status:** `[ ]` pending
**Build:** In the pipeline view, `r` (`src/tui/pipeline.rs:685`) on a governed ticket opens a one-line inbox-style input for the short prediction before it sends `Action::Refine`; the action gains `prediction: Option<ShortPrediction>`, the `{ kind, text, areas }` wire struct of C4. In the daemon, `Action::Refine` on a governed repository validates the named areas against the cached model, posts the comment through `post_comment` on the theory record, adds `theory-short` through `create_label_if_missing` plus `add_label`, then adds `to-refine` through `add_label`, and does not call `upsert_queued`; the poll gate queues the task. On a repository with the governor off the v0.6 path stays (`src/daemon.rs:1652-1679`); a missing area or a model in error refuses with the reason on a toast, writes nothing, and records the hold in `TheoryView.holds: Vec<HoldView>` (serde default, daemon memory until a prediction posts). Make `refine_ready` take `&TheoryRecords` and require `theory-short` on a governed ticket (`src/gates.rs:36-38`); add the row hints `awaits short prediction` and `model error`. Add the first-sight fetch: when a poll first shows a theory label on a record, fetch its comments once and ship `RecordView` in `TheoryView.records`. Add the chat lock: `c` on a ticket that awaits a prediction shows the reason (`src/tui/tickets.rs:373-394`).
**AC:**
- A daemon test asserts the `gh` call order: comment, `theory-short`, `to-refine`; a prediction that names area `gh` with no boundary `gh` posts nothing, adds nothing, and the toast reads `area gh has no entries`.
- A gate test with `to-refine` and no `theory-short` yields no refine work; with both it yields work; with the governor off `to-refine` alone yields work; with the model in error, both labels yield no work and the row shows `model error`.
- A first poll that shows `theory-short` on a record fetches its comments once and ships `records[key].short.areas`; the next poll fetches nothing.
- A pipeline render test shows `awaits short prediction`; a tickets test shows `chat waits for the short prediction` on `c`.
- `every_action()` gains the `prediction` field and the round-trip test passes.
**Depends on:** C5, C7 · **Traces to:** R3, R9, R11, R12, N2, N3

### C9 — The full prediction and the implement gate
**Status:** `[ ]` pending
**Build:** Add the template writer: five `[slot]` tables with `entries = []` and `tag = "unsure"`, a comment line per slot listing the candidate IDs of the areas from `records[key].short.areas` in the state view. Add `p` in the pipeline view on a `refined` row: write the template under `<state_dir>/edit/<alias>-<n>-prediction.toml`, run the editor bridge, parse, reject with the entry and reason on a toast, and send `TheoryAction::Predict { repo, number, prediction }`. The daemon validates the areas, posts the `<aif-prediction-v1>` block `{ kind: "full", slots }` on the theory record, and adds `theory-full`. Make `implement_ready` take `&TheoryRecords` and require `theory-full` on a governed ticket (`src/gates.rs:41-49`). Extend the chat lock to the `refined` window. Show `area <id> has no entries · b bootstrap` in the Theory view for a held prediction.
**AC:**
- A template test writes five slots with the candidate IDs of area `daemon`; a parse test rejects an unknown entry id, an unknown tag, and a missing slot, naming each.
- A daemon test asserts the posted block carries `kind = "full"` and five slots with tags, then the label; a full prediction that names an empty area is refused before any post and the implement gate yields no work.
- A gate test with `refined` and no `theory-full` yields no implement work; with both, work; with the governor off, `refined` alone yields work.
- A Theory view test shows `area gh has no entries · b bootstrap`.
- `every_action()` gains `TheoryAction::Predict` and the round-trip test passes.
**Depends on:** C8 · **Traces to:** R3, R10, R11, R12, N3

### C10 — The placeholders, the slices, and the binding
**Status:** `[ ]` pending
**Build:** Add `Model::slice(areas)` and `Model::areas_for_paths(paths)` with `globset`. Introduce the complete placeholder set in `render_prompt` (`src/daemon.rs:3193-3207`) for every stage prompt: `{model}`, `{prediction}`, `{comparison}`, `{skills}`, `{rules}`, `{why_rule}`, each rendered empty except `{model}`: refine gets the short prediction's areas, implement the full prediction's areas, review the implement slice plus the areas of `git diff --name-only <base>...<head>` run in the worktree. Rewrite the four stage prompts once: a `{model}` section, `{prediction}` in review, `## Why` and `## Evidence` and no `## How` in implement with `{why_rule}`, `{skills}` and `{rules}` everywhere, and save the `docs/v0.7/prompts/*.md` copies pinned by the byte-for-byte test (`src/prompts.rs:196-221`). Bind `model_commit` in `TaskBinding` at dispatch.
**AC:**
- A slice test on a model with two areas and one cross-area invariant returns the predicted area, the invariant, and the boundary it touches, and not the other area's states.
- `areas_for_paths(["src/tui/theory.rs"])` returns `tui` for a boundary with `paths = ["src/tui/*.rs"]`.
- A dispatch test asserts the review prompt contains the diff-touched area's entries and the `ScriptExec` saw `git diff --name-only`; every other placeholder renders empty and `fill_template` accepts the prompts.
- A dispatch stores `model_commit` in the binding; a task end drops it.
- The docs copies test covers the four reworded prompts and the vocabulary ban test passes.
**Depends on:** C9 · **Traces to:** R7, R8, R30

### C11 — The PR check
**Status:** `[ ]` pending
**Build:** Add `records::check_pr(body, paths, branch) -> Result<(), Finding>`: require `## Why`; forbid `## How` and any heading that starts with `Implementation`; forbid `theory/model.toml`, `theory/verify.toml`, and `theory/rules.md` in the path list off a model branch. In `dispatch_one` (`src/daemon.rs:1230-1299`), for a governed review after the cwd exists and before the prompt, run the check on the polled body and on `git diff --name-only` in the worktree; a finding posts one comment `PR: <finding>` on the theory record, cancels the review task with the finding, and re-queues implement through `upsert_queued(repo, Implement, Issue, ticket)` for every ticket in `Links::tickets_of(pr)`.
**AC:**
- A check test accepts a body with `## Why` and `## Evidence`, and rejects a body without `## Why`, a body with `## How`, a body with `## Implementation notes`, and a diff that lists `theory/model.toml` off a model branch, each with the finding text; the same diff on `aif/borsuk/model-1` passes.
- A dispatch test with a failing body posts the finding, cancels the review task, queues one implement task per linked ticket, and dispatches no review; with the governor off the check does not run.
**Depends on:** C10 · **Traces to:** R3, R27

### C12 — The delta in review
**Status:** `[ ]` pending
**Build:** Make `wants_final_block` return the delta tag for a governed review whose ticket carries `theory-full`. Instruct the reviewer to end with one `<aif-delta-v1>` block `{ slots: [{ id, outcome }], touched: [ids], violations: [{ entry, finding }], question }`. On a successful `TurnEnd`, parse the buffered text; post the block on the theory record, add `delta-open`, and compute the four outcomes from the slot tags. A governed review that ends without the block is a failed attempt with the finding `no delta block`. A review of a PR without `theory-full` renders `{prediction}` as `none` and expects no block. Ship `TheoryView.deltas: Vec<DeltaView>` and draw the DELTAS panel: `#n ● SURE-MISS  <entry>` for a miss row, `#n ○ OPEN  <hits> hit <unsure> unsure` for a hit-only record, `#n ✓ CLOSED` once closed.
**AC:**
- A daemon test feeds a review report with a valid block and asserts the comment, the label, and the state view row with `sure-miss` for a slot tagged `sure` and marked `miss`.
- A report without a block fails the attempt with `no delta block` and requeues while attempts remain; a review of a PR with no linked ticket completes without a block and adds no label.
- A block with an unknown slot id fails the parse and the attempt; a block with one violation ships it in `DeltaView.violations`.
- A Theory view test renders `#142 ● SURE-MISS  INV-3` and `#150 ○ OPEN  4 hit 1 unsure`.
**Depends on:** C11 · **Traces to:** R13, R33

### C13 — The window
**Status:** `[ ]` pending
**Build:** Add `Reason::WindowFull` to `sched` and a `window: &BTreeMap<String, (usize, usize)>` parameter to `can_start` (`src/sched.rs:159-196`); the daemon builds the map from `open_count` and `TheoryConfig.window`. Add `#[serde(default)] hold: Option<String>` to `TaskView` (`src/sock.rs:895-921`) and set it to `window full` on a refused implement task. Ship `TheoryView.window: (open, cap)` and draw the gauge `WINDOW ▮▮▯ 2/3` in the strip, with `IMPLEMENT ▸ PAUSED` when full. Add the `window full` hint on the pipeline row.
**AC:**
- A `can_start` test with `(3, 3)` for `borsuk` returns `No(WindowFull)` for an implement task and `Yes` for a review task; `(2, 3)` returns `Yes`.
- A daemon test with three `delta-open` records dispatches no implement task and its view carries `hold = "window full"`; a repository with the governor off is never in the map.
- A Theory view test renders `WINDOW ▮▮▮ 3/3 · IMPLEMENT ▸ PAUSED`.
**Depends on:** C12 · **Traces to:** R3, R14, R33

### C14 — Closing a delta: hits, misses, violations, causes, rungs
**Status:** `[ ]` pending
**Build:** Add `DecisionKind::DeltaHit { kind, number, hits }`, `DecisionKind::TheoryEvent { kind, number, slot, entry, tag, question, source }`, `DecisionKind::Card { source, prompt }`, `DecisionKind::FirstRun { area, measurer, sample }` (`src/decisions.rs:20-67`), `Response::Confirm` and `Response::Theory { cause, entry, rung, area, note }`, and the `validate` pairs: `DeltaHit` takes `Confirm`; `TheoryEvent` takes `Theory`; `Card` takes `Text`; `FirstRun` takes `Confirm` and `Cancel` (`src/decisions.rs:274-300`). Replace the five inbox matches and `inbox_row_owns` with `presentation(&DecisionKind)`. Add `Daemon::open_event(alias, key, event)` as the only writer of `<aif-event-v1>` and `event-open`. Derive the rows each poll through `derive_label_rows` from `delta-open` and `event-open` records and the shipped `RecordView`; one `THEORY` row per miss and one per violation. Inbox: `DELTA` rows close on `y`; `THEORY` rows take `m`/`p`/`r`, a typed entry id validated against the model, `1`/`2`/`3`, an area pre-filled when the entry maps to one area and asked when it maps to several, then `s`, using the `picks` pattern (`src/tui/inbox.rs:785-872`). Daemon: post `<aif-answer-v1>`, then: `recall` needs nothing more; `pr` posts the finding as a comment, cancels a live review task, and re-queues implement through `upsert_queued` for every ticket in `Links::tickets_of(pr)`; `model` records the answer time in the block. The label `delta-open` leaves only when every miss and violation of the record has an answer in `RecordView.answers` and no `model` answer stays open; each poll runs `git log --since=<answer> -p -- theory/model.toml` on the theory checkout's default branch and removes the label when a diff hunk contains the entry id.
**AC:**
- `validate` accepts exactly the five new pairs and rejects `Confirm` for `TheoryEvent`.
- A table test asserts `presentation` for every `DecisionKind`, including label, actions, footer, and digit ownership for the four new kinds; the v0.6 inbox render tests still pass.
- An inbox test drives `m`, `INV-3`, `2`, `s` and asserts one `Action::Answer` with the three fields; `s` before the rung shows `pick a rung`.
- A daemon test with two misses on one record keeps `delta-open` after the first `recall` answer and removes it after the second; for `pr` asserts the comment, the cancelled review, and one queued implement task per linked ticket; for `model` keeps the label until a scripted `git log` output contains `+statement = "…INV-3…"`, then removes it; a delta with one violation yields one `THEORY` row with source `violation`.
- `every_action()` gains the two responses and the round-trip test passes.
**Depends on:** C13 · **Traces to:** R15, R16, N3

### C15 — The map panel
**Status:** `[ ]` pending
**Build:** Add `src/tui/theory/map.rs`: a layered layout for one area at a time (states as boxes on rows by transition depth, transitions as `──<title>──▶` arrows labeled with the transition title truncated to 8 characters, the boundary as the outer double-line frame, failures as `⚠ <id> CROSSES <boundary>` lines under the frame), sized to the pane and truncated with `…` when the area does not fit. Add `h`/`l` to cycle areas and `j`/`k` to move the entry cursor; the cursor shows the entry statement in a bottom strip.
**AC:**
- A render test with two states and one transition asserts `[IDLE]──poll──▶[BUSY]` on one line inside a frame titled with the area id in uppercase.
- A test with a failure that crosses the boundary asserts `⚠ FM-2 CROSSES` under the frame.
- A test at 40 columns asserts the panel truncates with `…` and never panics.
- Pressing `j` moves the cursor and the bottom strip shows the next entry's statement.
**Depends on:** C14 · **Traces to:** R33

### C16 — Purposes, the audit role, and model-only PRs
**Status:** `[ ]` pending
**Build:** Add the `PurposeSpec` table with one const per `TaskPurpose`, and route `dispatch_one`, `task_cwd`, `execution_role`, `Limits`, and `wants_final_block` through it (`src/daemon.rs:1230-1299`, `:2994-3006`, `:3022`, `src/sched.rs:22-53`). Add `TaskPurpose::Audit(AuditJob)` with `AuditJob::ModelPr` and `AUDIT_MODEL_PR_PROMPT` (`src/prompts.rs`): read the changed entries of `theory/model.toml` in the PR, check each against the code, approve or request changes, and end with one `<aif-event-v1>` block per contradiction. Detect a model PR by the label `model-pr` or the branch `aif/<alias>/model-<n>` in `admit_ready` (`src/daemon.rs:800-843`): skip both gates and dispatch its review under `theory.audit`; the daemon diffs the cached model against `git show <head>:theory/model.toml` in the PR worktree and fills `{model}` with the slice of the changed ids; `wants_final_block` returns the event tag for every audit task, and each block opens an event through `open_event` on the PR record. Make `[theory.audit]` required and write `docs/v0.7/MIGRATION.md` with the governor default, the escape, the table, and the labels; point the config error at it.
**AC:**
- A config test rejects a file without `[theory.audit]` with `theory.audit is required; see docs/v0.7/MIGRATION.md`.
- A gate test with a `model-pr` PR yields review work with no `theory-full` on any ticket, the dispatched role is `TheoryAudit`, and the prompt fills `{model}` with only the changed entries; a branch `aif/borsuk/model-a1b2c3d4` without the label is detected too.
- Every v0.6 dispatch test still passes through the `PurposeSpec` table; a purpose with a stage keeps its stage limit and one without uses its own limit key.
- An audit report with one contradiction block opens one event on the PR record.
**Depends on:** C10, C14 · **Traces to:** R2, R6

### C17 — The bootstrap chat
**Status:** `[ ]` pending
**Build:** Add `ChatKey::{Ticket, Theory}` and key the conversation state by it (`src/state.rs:20-36`). Add `BOOTSTRAP_PROMPT`: take the operator's stream about area `{area}`, ask short questions, add no claim the operator did not state, and end a turn with one `<aif-model-proposal-v1>` block `{ entries: [...] }` when the operator says done. Add `TaskPurpose::Bootstrap` with the id `<alias>/bootstrap-<area>` under `theory.chat`, and make `[theory.chat]` required with its migration line. Add `TheoryAction::Chat { request, repo, purpose, key }`. Add `b` in the Theory view on an `area X has no entries` line: send it with purpose `Bootstrap` and the area as key, and open the session view like the ticket chat (`src/tui/tickets.rs:373-394`). On a proposal block, the daemon validates the merged model, writes it to the model worktree, and runs the C7 commit-push-PR path. The Theory view shows `◇ proposal · a apply` like the ticket proposal offer (`src/tui/tickets.rs:886-898`).
**AC:**
- A config test rejects a file without `[theory.chat]` with `theory.chat is required; see docs/v0.7/MIGRATION.md`.
- The ticket-chat tests still pass under `ChatKey::Ticket`; a bootstrap chat resumes after a restart under `ChatKey::Theory`.
- A daemon test feeds a proposal block with two entries and asserts the merged model validates, the commit, and the PR with `model-pr`; a proposal that duplicates an existing id is refused with the id in the chat and no commit.
- A TUI test presses `b` on the hold line and asserts the task id `borsuk/bootstrap-gh` and the role `TheoryChat`; `every_action()` gains `TheoryAction::Chat`.
**Depends on:** C16 · **Traces to:** R2, R11, R17, N3

### C18 — Events from the label and the train
**Status:** `[ ]` pending
**Build:** Add two source functions that call `open_event`: one reads the `needs-human` label on a governed record and opens an event, only when `RecordView.events` holds no open event with source `needs-human`, with the slice of the areas of `records[key].full`, else `.short`, else `areas_for_paths` of the PR diff, leaving `derive_needs_human` and `resolve_needs_human` unchanged; one runs in `finish_train` on failure (`src/daemon.rs:1377-1399`) and opens an event with the failed PR and `areas_for_paths` of the batch diff. The answer of a `needs-human` event also removes `needs-human`.
**AC:**
- A daemon test with a `needs-human` ticket shows one `THEORY` row whose source is `needs-human` after three polls, not three rows, and its answer posts the block and removes both labels; with the governor off no row appears and the v0.6 row still works.
- A failed train opens one event that names the PR and the batch areas.
- The window count rises by one per open event.
**Depends on:** C14 · **Traces to:** R3, R16

### C19 — Events from bug tickets and mid-flight changes
**Status:** `[ ]` pending
**Build:** Add `AuditJob::Bug` with `AUDIT_BUG_PROMPT` (`map this bug to entries`): a new ticket labeled `bug` on a governed repository queues one audit task with the id `<alias>/audit-bug-<n>` through `upsert_with_id`, skipped when the record already holds an event with source `bug`, whose `<aif-event-v1>` block opens the event with the mapped entries. Add the mid-flight source: on `apply_poll`, when the model commit moved, for every active task diff `git show <binding.model_commit>:theory/model.toml` against the new model, and open one event when a changed entry lies in the task's predicted areas; the task stays `Running`.
**AC:**
- A `bug` ticket queues one audit task and its block opens one event with the mapped entries; with the governor off nothing queues.
- A poll that changes an entry in a task's predicted areas opens exactly one event for the task, issues no cancel, and the task stays `Running`; a change outside the predicted areas opens nothing.
**Depends on:** C16, C18 · **Traces to:** R7, R16

### C20 — Shadow mode
**Status:** `[ ]` pending
**Build:** Honour `theory = { repo, path }`: `theory_record` returns the shadow issue of `repo` titled `<alias>#<n>`, found in the shadow snapshot or created through `create_issue` with the body `Theory record of <owner_repo>#<n>`. Spawn one more poller per shadow repository (`src/poll.rs:90-115`) that sends `DaemonMsg::TheoryPolled` into `Daemon.theory_snapshots`, never into `Snapshot.repos`. `TheoryRecords::derive` reads the shadow labels for shadowed aliases, so the gates, the window, and the rows follow. Fill `{why_rule}` with `name behaviours in words, not entry IDs` in shadow mode. Add a doctor line per shadow repository.
**AC:**
- A table-driven daemon test runs every write site of this spec so far in shadow mode, the short and full predictions, the delta, the answer, the events, and asserts that every `gh` call to the code `owner_repo` is a v0.6 label call, and that `borsuk#142` is created once and reused.
- A gate test reads `theory-full` from the shadow snapshot; `Snapshot.repos.len()` equals the count of configured code repositories; the v0.6 derivations see one repository.
- The doctor prints one line per shadow repository.
**Depends on:** C14 · **Traces to:** R19

### C21 — Cadences and the daily sweep
**Status:** `[ ]` pending
**Build:** Add `src/theory/cadence.rs`: `Schedule { kind, repo, last_ms }`, `ScheduleKind::{Daily, Interview, Audit}`, `due(schedules, config, now) -> Option<u64>`, persisted in `StateFile` next to `last_fire_ms` (`src/state.rs:65-67`); `next_deadline` (`src/daemon.rs:475-505`) adds the one `due` value. Add `Daemon::fire_cadence(kind, repo)`. The `Daily` kind runs the sweep: fetch the comments of the theory records updated in the last `stale_days` days, one page each, and derive the calibration share from the delta blocks, the rung counts from the `ladder-*` labels, the events per day, and the stale entries; ship `TheoryView.calibration`, `rungs`, `events_per_day`, `stale_entries`.
**AC:**
- A `due` test with no `last_ms` is due at once; with `last_ms` at noon UTC it is due at the first poll after UTC midnight; a restart keeps `last_ms`.
- A sweep test runs at most one comment page per record and ships `calibration = 0.7` from seven sure hits over ten sure slots, `rungs = [2, 5, 3]` from the labels, and `stale_entries = ["INV-9"]` for an entry untouched for 31 days.
- With the governor off no cadence exists for the repository.
**Depends on:** C19 · **Traces to:** R3, R20, R32, N1

### C22 — Cards
**Status:** `[ ]` pending
**Build:** In the daily sweep, build at most `cards.per_day` cards per repository: source one, a PR merged since `last_ms` none of whose linked tickets, from `Links::tickets_of`, carries an `<aif-prediction-v1>` comment authored by the operator (the `gh` login from `gh api user`, cached per daemon run); source two, the stale entries. The day's cards live in daemon memory; a restart drops them until the next sweep. A miss on a source-two card opens its event on the repository record. Add the `CARD` inbox rows through `presentation`. The answer, a `Text`, queues one `AuditJob::Card` task with `AUDIT_CARD_PROMPT`, the card, the answer, and the diff or the entry, whose `<aif-event-v1>` block, empty for a pass, opens an event on a miss.
**AC:**
- A merged PR whose ticket carries no operator prediction yields a source-one card, and one whose ticket carries an operator prediction yields none; an entry untouched for 31 days yields a source-two card; an entry touched 5 days ago yields none; a fourth card in one day is not shown.
- Answering a card queues one audit task with the card text; a block with one gap opens one event; an empty block opens none.
**Depends on:** C21 · **Traces to:** R20

### C23 — The interview
**Status:** `[ ]` pending
**Build:** Add `INTERVIEW_PROMPT`: lead a session for at most `{minutes}` minutes, from `interview.minutes`, about the model slice `{model}` and the PRs merged this week `{prs}`, ask one question at a time, and end with one `<aif-event-v1>` block per gap. Add `TaskPurpose::Interview` under `theory.chat` with the id `<alias>/interview-<date>` and `ScheduleKind::Interview` on `interview.weekday`. The Theory view shows `INTERVIEW WAITS · i join`; `i` opens the session view on the task. Each block opens an event on the repository record.
**AC:**
- A cadence test on the configured weekday queues one interview task per governed repository, and none on another day or with the governor off.
- A dispatch test asserts the interview prompt renders `{prs}` as the week's merged PR list and `{model}` as the full model.
- A Theory view test shows the waiting line and `i` opens the session on `borsuk/interview-2026-09-07`.
- Two blocks in the final turn open two events.
**Depends on:** C22 · **Traces to:** R21, R33

### C24 — Audit sweeps and the doctor audit
**Status:** `[ ]` pending
**Build:** Add `ScheduleKind::Audit` on `sweep.days` that queues one `AuditJob::Sweep` task with `AUDIT_SWEEP_PROMPT` (`check every entry against the code`); add the train sweep in `finish_train` on success when `sweep.after_train` is on, scoped to `areas_for_paths` of the batch diff. Add `aif doctor --audit <alias>` (`src/bin/aif.rs:59-70`) that sends `TheoryAction::Sweep { repo }`. Every event posts at once through `open_event`, on the item the block names as `{ kind, number }` when the entry maps to one, else on the repository record; the inbox derivation shows the oldest `cards.per_day` open sweep events of the day and ships the remainder count for the strip `n events wait`.
**AC:**
- A cadence test queues one sweep after `sweep.days` days and not before.
- A successful train with `after_train = true` queues one scoped audit whose prompt lists only the batch areas.
- `aif doctor --audit borsuk` sends the action; the help text lists `--audit`; `every_action()` gains `Sweep`.
- Five events from one sweep post five comments, show three inbox rows, and render `2 events wait`; a daemon restart shows the same three rows.
**Depends on:** C21 · **Traces to:** R22, N3

### C25 — The verification map
**Status:** `[ ]` pending
**Build:** Add `src/theory/verify.rs`: `VerifyMap`, `Area`, `Property`, `Measurer`, `parse`, and the R23 checks against the model's boundary ids. Read `git show <commit>:theory/verify.toml` next to the model when the commit moves; a missing file is an empty map, not an error. Ship `TheoryView.areas` and draw the AREAS panel: `daemon · 2 props · 1 measurer · ratchet`, where the policy shown is the strictest of the area's properties, `error` over `ratchet` over `observe`.
**AC:**
- A parse test accepts one area with one property and one measurer, and rejects an area whose boundary is not in the model, a property with an unknown rule, and a measurer with `timeout_s = 0`, each naming the area.
- A daemon test ships `areas` with the counts; a missing file ships an empty list and no error.
- A Theory view test renders the AREAS row.
**Depends on:** C1 · **Traces to:** R23, R33

### C26 — One measurer run
**Status:** `[ ]` pending
**Build:** Add `ScriptRunner` in `src/runner/script.rs` that implements `Runner` over `proc::spawn` with a timeout thread that stops the child. Change `RunnerFactory::build(role, purpose)` (`src/runner/mod.rs:48-66`) so `DefaultRunnerFactory` returns `ScriptRunner` for `TaskPurpose::Measure` with a synthetic role; the fake factory in the daemon tests follows. Add `src/theory/measure.rs` with `Record { id, value, unit, direction }` and `parse_lines`: a malformed line, including a missing `direction`, yields one `incomparable` record with the reason, and a run never fails. Add `TaskPurpose::Measure` with its `PurposeSpec` (no stage, limit key `measure`) and the id `<alias>/measure-<tree8>-<area>-<measurer>`, where `<tree8>` is `git rev-parse HEAD^{tree}` of the worktree until C27 replaces it with `tree_hash`; add `[measure] limit` to the config and `Limits`. Add `Daemon::queue_measure(alias, worktree, areas, mode) -> Vec<String>`. On `Exit`, regardless of `ok`, parse the log into records, or one `incomparable` record with the reason on a timeout or a non-zero exit, and post them as an `<aif-measure-v1>` comment on the theory record. On review admission of a governed PR, `queue_measure` one task per area of `areas_for_paths` of the diff at the head in `pr` mode.
**AC:**
- A runner test with a script that sleeps past `timeout_s = 1` ends within 3 s, the task ends `Done` at attempt 1, no `Stuck` decision appears, and the comment carries `incomparable: timeout`.
- A `parse_lines` test reads three lines and turns a malformed one, and one without `direction`, into `incomparable` records with the reason; the other records survive.
- A config test accepts `[measure] limit = 2` and the scheduler caps two concurrent measure tasks; `harness = "script"` in a role table stays rejected.
- A review admission of a PR that touches `daemon` queues one measure task and its records land as a comment on the theory record.
**Depends on:** C25, C16, C10 · **Traces to:** R24, N4

### C27 — The cache, the comparison, and the policy
**Status:** `[ ]` pending
**Build:** Add to `src/theory/measure.rs`: `tree_hash(exec, worktree)` through a temporary index (`GIT_INDEX_FILE=<state_dir>/measure/tmp-index-<pid> git read-tree HEAD`, `git add -A`, `git write-tree`); the cache at `<state_dir>/measure/<alias>/<tree>/<area>-<measurer_hash>.json` with `read_cache` and `write_cache`; `compare(before, after) -> Vec<Comparison>` with the six states; `apply_policy(area, comparisons) -> MeasureVerdict`; `agent_text(comparisons)` that renders `id  before → after  unit  state` under `AREA <id>`. `queue_measure` skips a cached tree and uses `tree_hash` for the task id.
**AC:**
- A real-git test in a temporary repository with one modified tracked file and one new file returns a hash that differs from `HEAD^{tree}` and leaves `git status` of the live index unchanged.
- A cache test writes and reads back the same records, yields `None` for an unknown tree, and misses when the measurer command changes.
- A comparison test yields `worsened` for `12 → 14` with `direction = lower`, `improved` for `14 → 12`, `new` for a missing before, `resolved` for a missing after, `unchanged` for equal values, and `incomparable` for a unit change.
- A policy test fails the verdict on `worsened` under `ratchet`, on `new` under `error` with `hold`, and never under `observe`; `agent_text` renders `poll_p95 12 → 14 ms worsened` under `AREA daemon`.
**Depends on:** C26 · **Traces to:** R24, R25

### C28 — Before and after on every PR
**Status:** `[ ]` pending
**Build:** Add `WorktreeKind::Base` at `worktrees/<alias>/base-<sha8>` with `ensure_detached(path, sha)` and a cleanable rule (`src/worktree.rs:44-63`). On review admission of a governed PR, compute the merge base (`git merge-base <default_base> <head>` in the item's worktree), queue measure tasks for the base tree in the base worktree and the head tree in `pr` mode, and hold the review task in `prior_stage_active` (`src/daemon.rs:1191-1220`) until both finish; replace the raw-records comment of C26 with one `<aif-measure-v1>` comment of the comparison on the theory record. Fill `{comparison}` with the agent text and `{skills}` with the touched areas' skill names in the review prompt; in the implement prompt `{comparison}` renders `none` and `{skills}` fills from the full prediction's areas. A new head sha re-queues the head measurement, like the review supersede (`src/daemon.rs:821-843`).
**AC:**
- A daemon test asserts two measure tasks, the held review, then the comment with `poll_p95 12 → 14 ms worsened`, and a review prompt that contains the table and `skills: control-app`.
- A head sha change queues one more head measurement and no base measurement.
- A PR whose diff touches no area renders `comparison: none` and holds nothing.
- The cleanable rule marks the base worktree removable once the PR is merged or closed, and `--clean` removes it after the preview.
**Depends on:** C27 · **Traces to:** R25, R30

### C29 — The lever: `aif measure`
**Status:** `[ ]` pending
**Build:** Add `Command::Measure { area: Vec<String> }` to `aif` (`src/bin/aif.rs:50-71`) and `TheoryAction::Measure { path, areas, request }` with a `Push::MeasureResult { request, text, pass }` reply. The daemon resolves the repository by the worktree path, takes the touched areas from `git diff --name-only <merge-base>` against the working tree, runs them at the worktree tree hash in `fast` mode, compares against the cached merge base, queued and awaited when absent, and replies with the agent text. `aif measure` prints it and exits 0 on pass, 1 on a failed verdict, 3 on an error.
**AC:**
- `tests/cli.rs` pins `aif measure --help`.
- A daemon test resolves `/state/worktrees/borsuk/issue-142` to `borsuk`, runs one measure task, and replies with a table that starts with `AREA daemon`; `every_action()` gains `Measure` and `Push::MeasureResult` round-trips.
- A second call on an unchanged tree replies from the cache with no new task.
- A failed verdict makes the command exit 1.
**Depends on:** C28 · **Traces to:** R26, N3

### C30 — Modes per stage and the train measurement
**Status:** `[ ]` pending
**Build:** Apply the tier rule, a stage runs every measurer whose mode is at or below its own: the lever runs `fast`, the review runs `fast` and `pr`, the train runs all three. Add the train path: before the release task dispatches, the daemon runs `git merge --no-commit` of the batch in the train worktree, queues `full` measure tasks for every area, and resets the worktree with `git reset --hard` before the release task starts. A `worsened` under `error` stops the train: the release task does not dispatch, the daemon calls `Train::finish(false, …)` so the batch returns to the queue with its retry set and `last_fire_ms` stays, and one event opens on the record of the PR whose diff touches the worsened area, else on the repository record.
**AC:**
- A measurer with `mode = "full"` does not run for the lever or the review, and runs for the train; a `fast` measurer runs for all three.
- A train test with a `worsened` error record does not dispatch the release task and opens one event; a train with only `observe` records dispatches.
- The `git reset --hard` call is the last git call before the release task starts.
**Depends on:** C29 · **Traces to:** R28

### C31 — The area statement chat and the first-run approval
**Status:** `[ ]` pending
**Build:** Add `AREA_PROMPT`: turn the operator's stream about area `{area}` into one `<aif-area-proposal-v1>` block `{ area, statement, properties }`, ask short questions, add no property the operator did not name. Add `TaskPurpose::Area` under `theory.chat` with the id `<alias>/area-<id>`, started by `s` in the Theory view on an AREAS row or on a boundary without an area, through `TheoryAction::Chat` with purpose `Area`; the daemon writes the block into `theory/verify.toml` on the model branch through the C7 path, replacing the `[[area]]` with the same id, else appending. Add the `FIRSTRUN` inbox rows: the first cached records of a new measurer wait for `Confirm` before they count as a "before"; `Cancel` skips the measurer; both decisions persist in `StateFile.measurer_approvals`; while an approval is pending the comparison renders `incomparable: awaits first-run approval` and the review proceeds.
**AC:**
- A daemon test feeds a proposal block and asserts the merged map validates and the PR opens with `model-pr`.
- A first measurement of a new measurer opens one `FIRSTRUN` row; `y` lets the next comparison use it; `n` skips the measurer in the next comparison; a restart keeps both decisions.
- A TUI test presses `s` on an AREAS row and asserts the task id `borsuk/area-daemon` and the role `TheoryChat`.
**Depends on:** C28, C17 · **Traces to:** R29, N1

### C32 — The ladder: rungs into tickets and rules
**Status:** `[ ]` pending
**Build:** On a theory event answer: rung 1 creates a ticket through `GhClient::create_issue` (gain `labels`, `src/gh.rs:396-420`) titled `Eliminate: <entry> — <question>` with `to-refine` and `ladder-1`; rung 2 creates `Measure: <area> — <entry>` with `to-refine` and `ladder-2`, where the area is the `area` of the answer; rung 3 adds `ladder-3` to the record and appends `- <date> <entry>: <note>`, with the date as `YYYY-MM-DD` in UTC, to `theory/rules.md` on the model branch through the C7 path. Fill `{rules}` from `git show <commit>:theory/rules.md`, empty when absent.
**AC:**
- A daemon test for rung 1 asserts `create_issue` with both labels and the title; rung 2 likewise; rung 3 asserts the label, the commit, and the PR, and that the next rendered prompt contains the rule line.
- A ticket created by rung 1 waits for its short prediction like any ticket.
**Depends on:** C14, C7 · **Traces to:** R31

### C33 — The ladder panel
**Status:** `[ ]` pending
**Build:** Draw the LADDER panel from `TheoryView.rungs`, `events_per_day`, and `calibration`: `R1 ▮▮  R2 ▮▮▮▮▮  R3 ▮▮▮`, `EVENTS/30D ▁▂▃▅▃▂▁`, and `CAL 71%` in the header strip.
**AC:**
- A Theory view test with counts `[2, 5, 3]` renders the three bars with those lengths.
- A calibration test with `0.7` renders `CAL 70%`; with `None` renders `CAL —`; seven days of counts render seven sparkline glyphs.
**Depends on:** C21, C32 · **Traces to:** R32, R33

---

## Definition of Done

- `python3 ~/.claude/skills/writing-specs/scripts/validate_spec.py docs/v0.7/SPEC.md` exits 0 with zero open `[NEEDS CLARIFICATION]` markers.
- Every requirement R1 to R33 and every rule N1 to N6 traces to at least one chunk, and every chunk traces to at least one requirement.
- Every chunk ships with its own tests green under `./check.sh` before the next chunk begins.
- Every docs copy under `docs/v0.7/prompts/` matches its const byte for byte.
- The wire revision stays 1.
