# Ticket-PR Links and One Task Vocabulary

Date: 2026-09-02 · Status: Ready · Scope: One item vocabulary for every stage, many-to-many ticket-PR links, link badges in the TUI · Predecessors: docs/v0.5/SPEC.md

---

## 1. Objective & Non-Goals

**Objective.** The pipeline names its work with one vocabulary: "ticket" for an issue and "PR" for a pull request. The daemon derives many-to-many ticket-PR links from the PR bodies it already polls, and the TUI shows the links as badges on the existing rows.

**What NOT to build (non-goals):**
1. **No GitHub timeline calls.** The PR bodies that the poll already carries hold the closing keywords. No new `gh` call exists.
2. **No plain mentions.** Only closing keywords count. A bare `#5` without a keyword creates no link.
3. **No cross-repository links.** Only same-repository `#n` references count. `owner/repo#n` is skipped.
4. **No new TUI view or pane.** The links appear as badges on the existing pipeline rows and in the existing inbox detail.
5. **No Tickets view badges.** Ticket shaping happens before any PR exists, so the Tickets view shows no links.
6. **No exact GitHub closing semantics for non-default bases.** The parser counts keywords on every PR. GitHub ignores keywords on a PR that targets a non-default branch. The factory accepts this divergence.

---

## 2. Context & Sources (grounding)

**Reality check (2026-09-02).**
- The worktree is a git repository, clean, at HEAD `00c5f3a` ("Merge pull request #3").
- `./check.sh` passes after one repair this run: the test helper at `src/tui/inbox.rs:3366` missed the `protocol_revision` field. The fix adds the field with `WIRE_PROTOCOL_REVISION`.
- Stack: Rust 2021, crate `aif` 0.5.0, deps anyhow 1, clap 4, crossterm 0.28, pulldown-cmark 0.13.4, ratatui 0.29, serde 1, serde_json 1, toml 0.9, uuid 1 (`Cargo.toml`).

**Existing code / consumer contracts (verified by reading the cited lines).**
- Task ids follow `<repo>/<stage>-<kind><number>`, built by `id_for` (`src/tasks.rs:152`). Ticket chat overrides its id to `<repo>/ticket-i<number>` after construction (`src/tasks.rs:136-148`). `upsert_queued` and `upsert_ticket_chat` duplicate the order-maintenance block (`src/tasks.rs:184-247`), so a third special id needs one shared helper, not a third copy.
- The ticket-PR link is one-to-one today. `review_issue_numbers: BTreeMap<String, u64>` maps one review task to one issue (`src/daemon.rs:142`). Five consumers read it: dispatch (`src/daemon.rs:1218-1232`), the supersede check (`src/daemon.rs:788-832`), `worktree_item` (`src/daemon.rs:2002-2013`), `prior_stage_active` (`src/daemon.rs:1167-1172`), and `task_cwd` (`src/daemon.rs:2710-2721`).
- Review dispatch refuses a PR whose branch is not `aif/<repo>/issue-<n>` (`src/daemon.rs:1218-1232`). A PR that closes several tickets, or that a human opened on another branch, cannot start a review today.
- Two agents never share one worktree. `worktree_item` maps a review task to its source issue item, and `sibling_blocker` holds a second task on the same item (`src/daemon.rs:2002-2031`).
- Worktree paths and branches: `issue_path`, `train_path`, `issue_branch` (`src/worktree.rs:63-87`). `ensure_issue` cuts the branch from the default base, `origin/HEAD` or `HEAD` (`src/worktree.rs:129-159`). `ensure_train` shows the reset discipline (`src/worktree.rs:172-209`). Removal goes through `remove_issue`, which deletes the issue branch too (`src/doctor.rs:315`, `src/worktree.rs:219-244`).
- The doctor scans the `issue-<n>` worktree directories by hard-coded prefix (`src/doctor.rs:1030-1073`), keeps its own copy of `issue_path` (`src/doctor.rs:1068-1074`), and treats only `IssueClosed` or `PullMerged` as cleanable (`src/doctor.rs:1093-1095`).
- The poll already carries PR bodies and head shas: `Pr.body` and `Pr.head_sha` (`src/model.rs:120-131`), the pulls fetch (`src/gh.rs:122-130`), and the poll JSON (`src/poll.rs:318`). No new fetch is needed for the link parser.
- The state view builds through `StateInput::build` (`src/sock.rs:318`) from `push_state` (`src/daemon.rs:519`). Later view fields use `#[serde(default)]` for wire compatibility (`src/sock.rs:96-100`), and `StateView` rejects no unknown field. The revision rule says: bump only when an older peer cannot safely cope (`src/sock.rs:44-48`).
- Board rows render the bare item label `i142` / `p7` in `ticket_spans` and `task_label` (`src/tui/pipeline.rs:1455-1507`). Release rows render bare PR numbers (`src/tui/pipeline.rs:1536-1573`), and `release_batch_task_id` rebuilds the old id for a saved retry batch (`src/tui/pipeline.rs:194-203`). A ticket title comes from the tickets list (`src/tui/pipeline.rs:1438-1445`).
- The inbox holds three parallel kind-to-noun tables: `item_label` says "Issue" / "Pull request", `item_title_label` says "Issue" / "PR" (`src/tui/inbox.rs:1126-1139`), and a local match adds a third (`src/tui/inbox.rs:1481`). The upsert error also names "issue" and "pull request" (`src/tasks.rs:196-200`). The help text says "toggle the selected pull request" (`src/tui/mod.rs:1187`).
- Prompt templates live as six consts, `REFINE_PROMPT` through `TICKET_CHAT_PROMPT`, inside the daemon module (`src/daemon.rs:3101-3199`). A file `prompts/<stage>.md` in the config directory overrides the default (`src/daemon.rs:2768-2773`). `fill_template` rejects an unknown placeholder (`src/daemon.rs:3002-3010`). `docs/v0.5/prompts/` holds reference copies of five of them; no copy exists for the ticket-creation prompt.
- The release train builds its task id `<repo>/release-p<first>` in `fire` (`src/trains.rs:220-247`). The daemon queues the release task with the lowest PR number as its item (`src/daemon.rs:2640-2668`) and maps id to batch in `release_batches` (`src/daemon.rs:140`). Tests pin the id format (`src/trains.rs:363`, `src/trains.rs:433`, `src/trains.rs:800`).

**External references (durable copies, if any).**
- `research/2026-09-02-github-closing-keywords.md` — the closing keyword list, the same-repository syntax, the default-branch rule, and the `pull/<n>/head` fetch, with the aif decisions on top.

---

## 3. Requirements & Acceptance Criteria

Functional requirements (EARS):

- **R1** — WHEN a user-visible string names a repository item, the system shall take the noun from one vocabulary table: "ticket" for an issue, "PR" for a pull request. The covered surfaces are: the prompt templates, every TUI screen (pipeline, inbox, tickets), the TUI confirmations, the decision titles, the doctor report lines, the daemon error lines, and the upsert error. Backtick command text is exempt.
- **R2** — WHEN a poll stores PR bodies, the daemon shall derive ticket-PR links from closing keywords in those bodies. The keywords are `close`, `closes`, `closed`, `fix`, `fixes`, `fixed`, `resolve`, `resolves`, `resolved`. Case does not matter. A colon may follow. Only same-repository `#<number>` references count.
- **R3** — The daemon shall also derive a link for each PR whose head branch is `aif/<repo>/issue-<n>`: it links ticket `<n>`.
- **R4** — IF a review task's PR links exactly one ticket, THEN the daemon shall run that review in that ticket's worktree.
- **R5** — IF a review task's PR links zero or several tickets, THEN the daemon shall run that review in the PR worktree `<state_dir>/worktrees/<repo>/pr-<n>` on branch `aif/<repo>/pr-<n>`. The daemon shall fetch the GitHub pull ref `pull/<n>/head` and reset that branch to the polled head sha, so the worktree holds the PR content. A PR worktree is cleanable when its PR is merged or closed.
- **R6** — The system shall NOT refuse a review task for the PR branch name or for the number of linked tickets. The current refusal at `src/daemon.rs:1218-1232` goes away.
- **R7** — The release task id shall be `<repo>/release`, and a retry of the same batch shall reuse the id. The release log file shall keep the batch number: `<state_dir>/logs/<repo>__release-p<lowest>.jsonl`.
- **R8** — WHEN the daemon builds a state view, it shall ship the ticket-PR links. The pipeline board shall show a link badge on every task row and every release PR row: ticket rows show `→ #7 #9`, PR rows show `← #142 #150`. A row with zero links shows no badge. The release task row shows no badge; its PR rows carry the tickets. The inbox item detail shall show the "Closes" list of a PR.
- **R9** — One test shall ban stray item nouns: outside backtick spans, after lowercasing, the prompt templates shall contain no "issue" and no "pull request"; the rendered screens shall say "Ticket" and "PR"; the noun test shall match over every `ItemKind` arm, so a new arm fails until it gains a noun. The prompt docs copies shall match the consts byte for byte.
- **R10** — IF two active tasks map to one worktree item, THEN the daemon shall hold the second task until the first is terminal. The existing rule at `src/daemon.rs:2002-2031` stays, under the new mapping.
- **R11** — WHEN a poll changes the head sha or the linked ticket set of a PR that has an active review task, THEN the daemon shall supersede that task with a fresh queued task. The daemon shall pin the ticket set of each review task at admit time.
- **R12** — WHEN the daemon renders a review prompt, it shall fill the `{tickets}` placeholder with the linked ticket list as `#4, #9`, or with `none` when no ticket links.

Non-functional / edge cases:
- No new GitHub API call. The derivation reads only the polled snapshot. The pr worktree fetch uses git only.
- The link table rebuilds on every poll of a repository and persists in no file. GitHub stays the only source.
- A body that parses poorly yields the links that were found; it never fails the poll.
- An old daemon view without the links field still parses in the TUI, through `serde` defaults, and an old client ignores the new field. The wire revision stays 1, per the rule at `src/sock.rs:44-48`.
- `./check.sh` passes: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

Acceptance (Given/When/Then, representative):
- *Given* a PR body `Fixes #10, resolves octo-org/other#22`, *when* the daemon applies the poll, *then* the links hold ticket 10 only, and the view carries the pair.
- *Given* a PR on branch `aif/borsuk/issue-5` with an empty body, *when* links derive, *then* ticket 5 links to that PR.
- *Given* a PR that closes tickets 4 and 9, *when* its review dispatches, *then* the session runs in `worktrees/borsuk/pr-<n>` and the worktree holds the PR head.
- *Given* a PR on a branch the factory did not create and with no keywords, *when* its review dispatches, *then* the task starts in the PR worktree; no refusal appears.
- *Given* an active review of a PR pinned to ticket set `{4}`, *when* a repoll reports the same head sha and a body that now closes `{4, 9}`, *then* the daemon supersedes the task and queues a fresh one.
- *Given* a fired train of PRs 7 and 9, *when* the state view arrives, *then* the release task id reads `borsuk/release` and the batch rows show their tickets.

---

## 4. Design (HOW)

**Architecture** — the daemon derives links per poll, ships them in the state view, and uses them at review dispatch. The TUI only renders what the view carries.

```
src/links.rs        new: keyword parser, branch-rule union, Links queries
src/prompts.rs      new: the six prompt consts move here from daemon.rs
src/model.rs        ItemKind::noun() -> "ticket" | "PR"
src/tasks.rs        scoped-id constructor; one upsert helper for all paths
src/daemon.rs       links cache; review_item helper; pinned ticket sets;
                    dispatch fallback; fire_train queues <repo>/release
src/worktree.rs     ensure_on helper; ensure_pr fetches pull/<n>/head and
                    resets to the head sha; remove_pr; owns directory names
src/doctor.rs       asks the worktree manager for owned directories
src/trains.rs       fire returns <repo>/release
src/sock.rs         LinkView, StateView.links (serde default), revision stays 1
src/tui/pipeline.rs badge spans on task rows and release rows
src/tui/inbox.rs    noun labels; item detail shows the Closes list
src/tui/mod.rs      help text wording
docs/v0.6/prompts/  reference copies of the reworded defaults
```

**Data flow and contracts.**
- The daemon holds one live links cache, `BTreeMap<String, Links>`, and calls `Links::derive(&RepoSnapshot)` once per poll of a repository. Every current or future source is an internal step of that function.
- The daemon pins the ticket set of each review task at admit time, `BTreeMap<String, BTreeSet<u64>>`. The supersede check compares the pinned set and the head sha against the fresh poll (`src/daemon.rs:788-832` today).
- One helper, `review_item(task) -> (ItemKind, u64)`, maps a review task through its links: one link gives the ticket item, otherwise the PR item. `dispatch_one`, `task_cwd`, `prior_stage_active`, and `worktree_item` (hence `sibling_blocker`) all consume it, so no site drifts.
- The prior-stage rule: a review waits while any linked ticket has an active implement task; a review with zero links never waits on a prior stage.

**Worktree mechanics.** `ensure_issue` and `ensure_pr` share one private helper, `ensure_on(exec, repo, path, branch)`, extracted from `src/worktree.rs:129-159`. `ensure_pr` wraps it, then fetches `pull/<n>/head` and runs `git reset --hard FETCH_HEAD` in the worktree, per the research file. `remove_pr` sits beside it and deletes branch `aif/<repo>/pr-<n>`. The doctor asks the worktree manager for the directory names it owns, so the manager is the single source and the local `issue_path` copy goes away.

**Naming rules that this spec sets.** They supersede the matching lines of `docs/v0.5/SPEC.md:221-240`:
- Item nouns in every user-visible string: "ticket" and "PR".
- Task ids keep the form `<repo>/<stage>-<kind><number>`, with one exception: the release task id is `<repo>/release`, built by one scoped-id constructor.
- PR worktree path: `<state_dir>/worktrees/<repo>/pr-<n>`, branch `aif/<repo>/pr-<n>`.
- Release task log: `<state_dir>/logs/<repo>__release-p<lowest>.jsonl`.

**Extension axis (OCP).** `src/links.rs` stays plain functions plus one table; no step abstraction exists until a second real source arrives. A new source joins as a new function inside `derive`. New item kinds add one `noun()` arm. New worktree kinds add a path pair registered with the manager; the doctor scan needs no edit. Badge rendering reads only the `LinkView` list, so a new source needs no TUI edit.

**Error handling / graceful degradation.** The parser returns what it finds and never fails a poll. A failed fetch or reset in `ensure_pr` fails that dispatch with a clear reason, like a worktree error today. A view from an older daemon parses with an empty links list. `fill_template` still rejects an unknown placeholder, so a template that uses `{tickets}` before the daemon fills it fails loudly.

---

## 5. Boundaries

- ✅ **Always:** links derive from polled data only; every poll of a repository refreshes them; badges render from the state view only; two agents never share one worktree; one helper maps a review task to its worktree item.
- ⚠️ **Ask-first:** worktree removal keeps the existing doctor preview flow; nothing new asks first.
- 🚫 **Never:** no new GitHub API call for links; no cross-repository links; no plain mentions; no parallel agents in one worktree; no edit of the shipped `docs/v0.5/` history.

---

## 6. Open Questions

All questions are resolved. None waits for clarification.

1. **Vocabulary** — "ticket" and "PR"; task ids keep their form. (User decision, 2026-09-02.)
2. **Link source** — closing keywords in PR bodies, plus the branch rule as fallback. (User decision, 2026-09-02.)
3. **TUI display** — link badges on existing rows; no new view. (User decision, 2026-09-02.)
4. **Review worktree** — ticket worktree for the single-link case; per-PR worktree for zero or several links. (User decision, 2026-09-02.)
5. **Release task id** — `<repo>/release`, built by one scoped-id constructor; the log name keeps the batch number. (User decision, 2026-09-02; log rule from review.)
6. **Keywords on non-default bases** — parse them; accept the divergence from GitHub. Reason: no base-branch data is polled, and the case is rare. See `research/2026-09-02-github-closing-keywords.md`.
7. **Supersede check** — requirement R11. The daemon pins the ticket set at admit time and compares sets plus the head sha.
8. **Prompt docs copies** — `docs/v0.6/prompts/` holds six files; the ticket-creation copy is new. Each file matches its const byte for byte, pinned by an `include_str!` test.
9. **PR content in the pr worktree** — the daemon fetches `pull/<n>/head` and resets the branch to the polled head sha. (User decision, 2026-09-02, from review.)
10. **Wire revision** — stays 1; the `serde` default protects both directions, per the rule at `src/sock.rs:44-48`.
11. **Prompt module** — the six consts move to `src/prompts.rs`; each prompt has exactly one writer chunk.
12. **Stable-id mechanics** — one scoped-id constructor and one `TaskTable` upsert helper serve the ticket chat and release paths.

---

## 7. Chunks and Acceptance Criteria

### C0 — One item vocabulary everywhere
**Status:** `[x]` DONE
**Build:** Add `ItemKind::noun()` returning "ticket" and "PR", and make every kind-to-noun site consume it: delete or delegate `item_label`, `item_title_label`, and the local match in the inbox. Move the six prompt consts to new `src/prompts.rs`. Reword five prompts (refine, implement, release, ticket creation, ticket chat) to the vocabulary; chunk C5 owns the review prompt. Reword the tickets view strings, the inbox confirmations and footers, the decision titles, the doctor report lines, the help text, and the upsert error. Write the five docs copies to `docs/v0.6/prompts/`. Add the ban test and the docs-coupling test.
**AC:**
- The noun test matches over every `ItemKind` arm and asserts "ticket" and "PR"; a new arm fails the test until it gains a noun.
- The ban test strips backtick spans per line, lowercases the line, and fails on any "issue" or "pull request" in the five reworded consts; it passes after the rewording.
- An `include_str!` test asserts each of the five `docs/v0.6/prompts/*.md` files equals its const byte for byte.
- Screen tests show the vocabulary: the tickets view, a release gate confirmation reading `Release PR #7?`, a decision title reading `Ticket #142 needs a decision`, a doctor report line, and the help line `toggle the selected PR`.
- A test asserts the upsert error names "ticket" and "PR".
**Depends on:** — · **Traces to:** R1, R9
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->
✅ IMPLEMENTED
- `ItemKind::noun()` and `title_noun()` live in `src/model.rs`; the inbox label functions and the local noun match are gone.
- The six prompts moved to `src/prompts.rs`; the daemon imports them; five carry the vocabulary.
- `docs/v0.6/prompts/` holds the five copies, pinned by the coupling test.
- The commit also repairs the train-state test helper at `src/tui/inbox.rs`, which missed `protocol_revision` before this work started.
Last updated: 2026-09-02

### C1 — Derive ticket-PR links
**Status:** `[ ]` pending
**Build:** Add `src/links.rs`. Parse the closing keywords per the research file. Union the branch rule. Move `issue_number_from_head` there and reuse it from the daemon. Expose `prs_of(ticket)` and `tickets_of(pr)` queries.
**AC:**
- Parser tests: `Fixes #10, resolves #22` yields 10 and 22; `closes: #7` yields 7; `Fixes octo-org/other#9` yields nothing; `mentioned #5` yields nothing; `CLOSES #3` yields 3.
- Union test: PR 7 on branch `aif/borsuk/issue-5` with body `Closes #9` yields `tickets_of(7) == [5, 9]`, `prs_of(5) == [7]`, and `prs_of(9) == [7]`.
- Dedup test: a PR on `aif/borsuk/issue-5` with body `Closes #5` yields exactly one link for ticket 5.
- Malformed test: a body `Fixed ###, closes #abc` yields no links and returns no error; a partly readable body yields the links that parse.
- Exact query asserts, for example `prs_of(5) == [7]` and `tickets_of(7) == [5]`.
**Depends on:** — · **Traces to:** R2, R3
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

### C2 — Ship links and show badges
**Status:** `[ ]` pending
**Build:** Add the per-repository links cache to the daemon, rebuilt by one `Links::derive` call per poll. Add `LinkView { repo, ticket, pr }` and `StateView.links` with `#[serde(default)]`; the wire revision stays 1. Extend the existing number-list helper for the badge text and the Closes line, and state the cap for long lists. Add badge spans to the board task rows and release PR rows, in dim style after the title; plain-string rows in the nested release border append the badge text.
**AC:**
- A daemon rig test polls a PR body `Closes #142` and asserts the pushed view carries the pair `(borsuk, 142, 7)`.
- A pipeline test renders a ticket row with `→ #7 #9` and a PR row with `← #142 #150`, in dim style after the title; a zero-link row shows no badge.
- A pipeline test renders a release PR row with its ticket list.
- An inbox test renders a PR detail with a `Closes #142 #150` line.
- A serde test parses a state view JSON without the `links` field into an empty list.
**Depends on:** C1 · **Traces to:** R8
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

### C3 — Review dispatch by links
**Status:** `[ ]` pending
**Build:** Add the `review_item` helper and route `dispatch_one`, `task_cwd`, `prior_stage_active`, and `worktree_item` through it. Replace `review_issue_numbers` with the pinned ticket set filled at admit time. Supersede an active review when the head sha or the ticket set changes. Remove the branch refusal. Extract `ensure_on` in the worktree manager; add `pr_path`, `ensure_pr` (fetch `pull/<n>/head`, reset to the head sha), and `remove_pr`. Let the doctor ask the manager for owned directories; a pr worktree is cleanable when its PR is merged or closed. Make `implementation_transitioned` query the links cache, so one branch-rule derivation stays.
**AC:**
- A rig test dispatches a review of a PR that closes tickets 4 and 9, and asserts the session runs in `worktrees/<repo>/pr-<n>`.
- A worktree test asserts the exact git arguments: the `worktree add` call for `pr-<n>`, then `fetch origin pull/<n>/head`, then `reset --hard FETCH_HEAD`.
- A rig test dispatches a review of a PR on a foreign branch with no keywords; the task starts and no refusal message appears.
- A rig test resumes a chat for a review task in a pr worktree, and asserts the resume reads the session marker in that worktree.
- A rig test shows a review waits while a linked ticket has an active implement task, and a review with zero links never waits.
- Supersede rig tests: a pinned set `{4}` that changes to `{4, 9}` supersedes the active task and queues a fresh one; an unchanged set with the same head sha supersedes nothing.
- Regression: a single-link review dispatches into the ticket worktree, unchanged from today.
- Regression: two active reviews of two PRs of one ticket stay serialized through the sibling blocker.
- A doctor test lists a stale `pr-3` worktree for removal when PR 3 closes without a merge.
**Depends on:** C1, C2 · **Traces to:** R4, R5, R6, R10, R11
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

### C4 — Stable release task id
**Status:** `[ ]` pending
**Build:** `Train::fire` returns `<repo>/release`. Add the scoped-id constructor next to `id_for` in `src/tasks.rs` and one `TaskTable` upsert helper; route the ticket chat and release paths through both. `fire_train` queues the task with the overridden id and keeps the log name `logs/<repo>__release-p<lowest>.jsonl`. Replace the id that `release_batch_task_id` builds at `src/tui/pipeline.rs:194-203`. The naming rules in section 4 stay as written; no `docs/v0.5/` edit happens.
**AC:**
- A trains test asserts `fire` returns `borsuk/release`, and a retry of the same batch reuses the id.
- A rig test fires a batch and asserts the view task id and `in_flight` read `borsuk/release`.
- A pipeline test renders a saved retry batch with no task in flight, and asserts the release row appears under the id `borsuk/release`.
- A rig test asserts a second batch with a different lowest PR writes its own log file, named by that lowest PR.
- A scan test finds no literal `release-p` under `src/`, and the files under `docs/v0.5/` stay untouched.
**Depends on:** — · **Traces to:** R7
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

### C5 — Review prompt shows the tickets
**Status:** `[ ]` pending
**Build:** This chunk owns the review prompt. Reword it to the vocabulary, add the `{tickets}` placeholder — the linked ticket list as `#4, #9`, or `none` — fill it from the links cache in `render_prompt`, and write its docs copy. Extend the ban test and the docs-coupling test to the sixth const.
**AC:**
- A template test asserts `fill_template` accepts `{tickets}` and still rejects an unknown placeholder.
- A rig test asserts a multi-link review prompt contains `#4, #9` and an unlinked one contains `none`.
- The ban test and the coupling test cover all six consts and all six docs copies.
**Depends on:** C0, C2 · **Traces to:** R12
<!-- implement-chunk appends ✅ IMPLEMENTED / notes / Last updated below -->

---

## Definition of Done

- The skill validator (`writing-specs/scripts/validate_spec.py`) exits 0 on this file, and no `[NEEDS CLARIFICATION]` marker stays.
- Every requirement R1 to R12 traces to at least one chunk above.
- Each chunk lands with its own tests green and `./check.sh` passing before the next chunk starts.
