# Mention Status Icons in Ticket and PR Bodies

Date: 2026-09-03 · Status: Ready · Scope: Decorate every mentioned ticket and pull request in an issue or PR body with a live TUI-native status icon, resolved by the daemon in one batched, cached call set. · Predecessors: none

---

## 1. Objective & Non-Goals

**Objective.** When the operator opens a ticket, every `#number`, `owner/repo#number`, and pasted `github.com` issue/PR link inside the body and the proposal gets a status icon next to it. The icon tells open issue, closed issue, draft PR, open PR, merged PR, closed PR, and not-found apart. The daemon resolves the statuses in one small cached fetch set per open focus and refreshes them at the 60 s cadence, so the view feels live like the footer ball without any extra keystrokes.

**What NOT to build (non-goals):**
1. **No icons in list rows.** The tickets list, the pipeline view, and the inbox rows stay untouched. Only the focused issue body/proposal and the PR detail body decorate. Row density and list performance stay as today.
2. **No closed-reason distinction.** `state_reason` (`completed`, `not_planned`, `duplicate`) does not change the icon. One closed style keeps the legend small.
3. **No navigation.** An icon is not a link target; the operator cannot jump to the mentioned ticket by pressing a key on it. A later spec can add that.
4. **No comment scanning.** Only the issue or PR body (and the pending proposal body) is scanned. Conversation comments stay plain.
5. **No other forges or API-shaped URLs.** `gitlab`, `jira`, and `repos/{o}/{r}/issues/N` API paths stay plain text. The mention grammar is closed at the three GitHub forms.

---

## 2. Context & Sources (grounding)

**Reality check (2026-09-03).**
- Working tree is a git repo of crate `aif` 0.6.0 (Rust 2021, ratatui 0.29, pulldown-cmark 0.13.4, `gh` CLI as the only GitHub transport; `Cargo.toml`).
- `cargo` and the crate registry sources are present locally; `check.sh` exists at the repo root as the verification entry point.

**Existing code / consumer contracts (verified by reading the cited lines).**
- The tickets focus view renders the issue body once per update through `markdown_lines` into `body_lines` and `proposal_lines` (`src/tui/tickets.rs:170-182`), and draws them in `draw_focus` (`src/tui/tickets.rs:826-939`). The markdown renderer knows code regions exactly: `Event::Code` and the code-block depth (`src/tui/markdown.rs:70-87`, `123-126`).
- The inbox PR detail renders the PR body as plain text wrapped by `wrapped_lines` before drawing (`src/tui/inbox.rs:1676-1719`, `1722-1728`); it already derives linked ticket numbers from `state.links`.
- The daemon handles `TicketAction::Details` from the snapshot only, with no network call (`src/ticket.rs:65-96`); content mutations already run inline `gh` subprocess calls on the daemon loop (`src/ticket.rs:206-220`), so a short inline fetch batch matches the existing design.
- The poller fetches open issues and open PRs per repository every 60 s with ETag caching (`src/poll.rs:26`, `src/gh.rs:111-130`); the snapshot holds open items only, and `Pr` already carries `draft` (`src/model.rs:130-160`, `src/gh.rs:562`). Closed and merged states never reach the TUI today.
- `GhClient` wraps `gh api -i` through the `Exec` indirection and maps JSON with strict field readers (`checked_response`, `str_field`, `bool_field`, `state_is_open`; `src/gh.rs:455-532`, `577-669`). Tests script every `gh` answer via `ScriptExec` (`src/gh.rs:677-692`).
- The TUI already has a poll/timer plumbing pattern for the focused chat (`needs_poll`/`poll` in `src/tui/tickets.rs:128-155`, tick wiring in `src/tui/mod.rs:934-1008`) and a footer status vocabulary including the live ball (`src/tui/mod.rs:1131`).
- Wire types live in `src/sock.rs`; `WIRE_PROTOCOL_REVISION = 1` guards state pushes (`src/sock.rs:52`, `src/sock.rs:1624-1628`). TUI and daemon ship in one binary pair from the same crate. Per-focus data travels as per-client pushes with request ids (`TicketDetails`, `TicketLabels`); shared repo-wide data travels inside `StateView`.
- The theme exposes `text`, `dim`, `accent`, `repo` (pink/purple), `ok` (green), `warn`, `error` (`src/tui/theme.rs:34-44`). The TUI symbol vocabulary in use: `○ ◇ ◆ ● ✓ × ›`.

**External references (durable copies).**
- GitHub REST API, "Get an issue", API version 2022-11-28, fetched 2026-09-03: the issue response carries `state` (`open`|`closed`), a `pull_request` object with `merged_at` (string or null) when the number is a PR, and a top-level `draft` boolean. One call per mentioned number therefore yields the full status set, including for closed and merged items that the open-only poller never sees.

---

## 3. Requirements & Acceptance Criteria

Functional requirements (EARS):

- **R1** — WHEN the TUI renders a focused issue body or proposal body, the system shall draw a status icon immediately before every recognized mention whose status is known, and no icon before a mention whose status is unknown.
- **R2** — WHEN the TUI renders a PR detail description in the inbox, the system shall draw the same status icons for recognized mentions in that description.
- **R3** — WHEN the daemon resolves mentions for one focus, the system shall recognize all three forms: bare `#N` (same repository), `owner/repo#N`, and `github.com/owner/repo/issues/N` or `/pull/N` URLs.
- **R4** — WHEN the daemon resolves one mentioned number, the system shall spend at most one REST call per number per TTL window (90 s), consulting the snapshot and the status cache before any fetch.
- **R5** — IF a mentioned number answers as a PR (`pull_request` key present), THEN the status shall distinguish draft, open, merged, and closed-unmerged; otherwise the status shall distinguish open and closed issues.
- **R6** — WHILE the focus view stays open, the tickets view shall send one status refresh request per tick of a 60 s timer owned by the tickets view, and the view shall update in place when fresher statuses arrive.
- **R7** — IF a fetch fails, returns 404, or hits the rate limit, THEN the system shall keep the icon absent, keep any earlier statuses, and make no further attempt for those numbers until the next TTL window. A 404 maps to the not-found status, which renders no glyph.
- **R8** — The system shall NOT alter body text: decoration inserts styled icon spans only, leaves the mention text intact, and skips mentions inside code spans and code blocks.
- **R9** — WHEN one body holds more than 12 unique mentions, the system shall resolve the first 12 in document order and leave the rest plain.
- **R10** — Status resolution shall never block the first paint: the details push goes out before any fetch, and statuses arrive as a second push.
- **R11** — IF a mention targets a repository outside the configured set, or a number the snapshot does not hold, THEN the system shall resolve it directly or answer with an empty status set, and the view shall not show an error.

Acceptance (Given/When/Then, representative):
- *Given* a focused issue whose body says `Depends on #8` with #8 open, *when* the daemon answers details and then mentions, *then* the rendered screen shows the filled ball icon in the ok color before `#8`.
- *Given* the same body with #8 closed, *when* the screen renders, *then* `#8` carries the hollow circle icon in the dim color.
- *Given* a body mentioning `octo/repo#12` where #12 is a merged PR, *when* the screen renders, *then* the filled diamond icon in the repo color appears before the mention.
- *Given* a PR detail in the inbox whose description mentions #7 (open), *when* the detail draws, *then* the filled ball icon appears before `#7` in the description text.
- *Given* the rate limit answered the last fetch, *when* the next refresh ticks within the same TTL window, *then* the scripted `Exec` records no `gh` call.

---

## 4. Design (HOW)

**Architecture.**

```
src/mentions.rs        (new, lib)   scanner (all forms), canonical keys,
                                    status enum, glyph table, classify
src/sock.rs            (edit)       MentionStatus, TicketMentions push,
                                    Mentions/PrMentions actions, wire rev 2
src/gh.rs              (edit)       fetch_mention_status via the existing
                                    field readers
src/ticket.rs          (edit)       resolve-on-focus at the plan seam
src/tui/markdown.rs    (edit)       mention decoration at render time
src/tui/tickets.rs     (edit)       statuses field, merge on push,
                                    60 s refresh timer while focus open
src/tui/inbox.rs       (edit)       PR description decoration pre-wrap
src/tui/mod.rs         (edit)       route new push/action, timer tick
```

Data flow: the TUI opens a focus and sends `Details` as today. The daemon answers with the snapshot issue immediately (R10) and then runs the resolver: extract mentions from the body, plan candidates (snapshot first, then status cache, then a fetch list), fetch and map what the plan still needs, and push `TicketMentions`. The same-repo numbers that the snapshot already holds as open issues or open/draft PRs resolve with zero network cost; only closed, merged, cross-repo, and unconfigured targets cost a fetch. The plan step is the named seam where C2 later inserts the TTL cache. The TUI merges the statuses into the open focus and re-renders the cached body lines. While the focus stays open, a 60 s timer re-sends the refresh action; the daemon re-plans and pushes again only when a status changed or was missing. The inbox PR detail follows the same two-step shape: opening a PR detail sends `PrMentions`, the daemon resolves against the PR body from the snapshot, and the inbox decorates its own status map.

**Status vocabulary (shape tells the kind, color tells the state).**

| Status | Glyph | Color |
|---|---|---|
| open issue | `●` | `ok` |
| closed issue | `○` | `dim` |
| draft PR | `◇` | `dim` |
| open PR | `◇` | `ok` |
| merged PR | `◆` | `repo` |
| closed PR (unmerged) | `◇` | `error` |
| no status yet, or not found | *(none)* | — |

The `MentionStatus` enum, the glyph table, and one `classify(state, merged, draft)` function live together in `src/mentions.rs`; `gh.rs` extracts the raw `state`, `pull_request.merged_at`, and `draft` fields only. A new status therefore touches one file.

**Mention grammar (one scanner, all forms).** The scanner in `src/mentions.rs` is an ordered list of per-form matchers run in document order: `owner/repo#N`; bare `#N` resolved against the focus repository; and `github.com/owner/repo/(issues|pull)/N`. A bare `#N` counts as a mention only when the preceding character is not a word character, `/`, `&`, or `#` (this keeps `abc#12`, URLs, and `##` headings out). The canonical cache key is `owner/repo#N` with owner and repo lowercased. The grammar is closed (non-goal 5); a future form adds one matcher instead of an edit to shipped logic.

**Decoration (two appliers, one scanner).** The scanner is the single source of mention parsing; each surface owns its applier. The tickets view decorates inside the markdown render pipeline, where the renderer knows code regions exactly (`Event::Code` and the code depth), so code-span mentions never decorate (R8). The inbox decorates the raw PR body text before `wrapped_lines` runs, so icons never push a line past its width and a mention split across a wrap boundary still resolves. Both appliers insert one styled glyph span plus a single space before each recognized mention whose status the lookup knows; text never changes (R8). Each decorated surface pays a fixed wiring cost — one action variant, one push route, one status store, one daemon arm, one view — and the spec accepts that cost to keep the two views independent.

**Error handling / graceful degradation.** A failed `gh` call or an unparsable body leaves that number without a status until the next TTL window (R7). A 404 maps to the not-found status so the cache can hold it. A rate-limited call is a failure like any other: the numbers stay plain and the next attempt waits for the TTL window (R7). The TUI never surfaces these failures in the body; a mention whose status never arrived simply stays plain, which is today's rendering.

---

## 5. Boundaries

- ✅ **Always:** instant first paint of the details view (R10); at most one REST call per number per TTL window; snapshot and cache consulted before any fetch; icon set only from the fixed glyph table; body text unchanged.
- ⚠️ **Ask-first:** raising the 12-mention cap; adding a closed-reason distinction to the legend.
- 🚫 **Never:** block the details push on a fetch; scan comments or list rows; render an error string inside the body pane.

---

## 6. Open Questions

- Resolved: status set and legend — user chose the full set with shape=kind glyphs (interview 2026-09-03).
- Resolved: placement — issue and PR bodies only (interview 2026-09-03).
- Resolved: freshness — fetch on open plus a live 60 s refresh while the focus stays open (interview 2026-09-03).
- Resolved: mention forms — all three forms (interview 2026-09-03).
- Resolved: API shape — the REST "Get an issue" response carries `state`, `pull_request.merged_at`, and `draft`, so one call per number suffices (docs.github.com, API version 2022-11-28, fetched 2026-09-03).
- Resolved: decoration point — the markdown renderer knows code regions exactly, and the inbox decorates raw text before wrapping (review 2026-09-03).

---

## 7. Chunks and Acceptance Criteria

### C0 — Same-repo mention icons end to end
**Status:** `[x]` ✅ IMPLEMENTED
**Build:** The walking skeleton. Add `src/mentions.rs` with the full three-form scanner, canonical keys, the `MentionStatus` enum, the glyph table, and `classify`. Add the `TicketMentions` push and the `TicketAction::Mentions` variant in `src/sock.rs` (bump `WIRE_PROTOCOL_REVISION` to 2). Add `GhClient::fetch_mention_status` in `src/gh.rs`, reusing the existing response plumbing and field readers, extracting raw `state`, `pull_request.merged_at`, and `draft` for `classify`. On `Details`, `TicketController` runs the resolver — extract, plan (same-repo snapshot hits resolve free; misses fetch, capped at 12 in document order per R9), push details first and mentions second (R10). The tickets view stores the statuses for the open focus, merges a `TicketMentions` push only when repo and number match, and re-renders `body_lines` and `proposal_lines` with mention decoration inside the markdown render path, skipping code regions (R8).
**AC:**
- A `mentions.rs` unit test fails before the chunk and passes after: `Depends on #8` against a lookup that maps `acme/borsuk#8` to open yields an inserted `●` span before `#8` and no other text change; `octo/repo#12`, `https://github.com/octo/repo/issues/12`, and `https://github.com/octo/repo/pull/12` each produce the canonical key `octo/repo#12`; `x/y#3` inside a URL query and `abc#3` produce none (R3).
- A `gh.rs` scripted test maps all four PR rows through `classify` — draft, open, merged, closed-unmerged — plus the closed-issue row; the old `fetch_issue` still rejects PR objects (R5). Regression guard: this clause stays green before and after.
- A daemon test with a scripted `Exec` proves the order and the plan: the details push goes out before any `gh` call, snapshot-known numbers record zero calls, unknown numbers record exactly one call each in document order, and a body with 14 unique mentions records exactly 12 calls with mentions 13 and 14 left plain (R4, R9, R10).
- A tickets-view draw test shows the ok `●` before an open mention and the dim `○` before a closed mention; a draft PR mention renders the dim `◇` and a closed PR mention the error `◇`; a mention with no status shows no glyph (R1, R5).
- A draw test with the mention inside an inline code span and inside a code block shows no glyph in either (R8).
- A draw test with a proposal body that holds a mention shows the glyph there under the same rules (R1).
**Depends on:** — · **Traces to:** R1, R3, R5, R8, R9, R10

Implementation notes: shipped in commit `2d5865a`. `src/mentions.rs` carries the scanner, `classify`, `glyph`, and `tone`; the TUI maps the tone onto `THEME`. `markdown_lines_with_mentions` decorates inside the render pipeline; `markdown_lines` stays as the undecorated wrapper. The daemon follow-up runs after the details push; the `details_answers_from_the_snapshot_before_any_gh_call` test pins that order.
Last updated: 2026-09-03

### C1 — Unconfigured repositories
**Status:** `[x]` ✅ IMPLEMENTED
**Build:** Teach the resolver in `src/ticket.rs` to fetch `owner/repo` targets that no configured alias covers, directly under their canonical key; configured repositories keep their alias mapping (R11). The scanner and the tickets view need no change; C0 shipped the full grammar.
**AC:**
- A daemon test with a body that mentions an unconfigured `other/repo#5` records exactly one `gh api repos/other/repo/issues/5` call (R3, R11).
- A tickets-view draw test feeds the status for the `other/repo#5` canonical key and renders the right glyph (R3, R11).
- A regression draw test repeats the C0 same-repo case unchanged (guard).
**Depends on:** C0 · **Traces to:** R3, R11

Implementation notes: shipped in commit `77e6516`. The planner resolves each mention target — the focus repository for a bare number, a configured alias via `owner_repo` match, otherwise the canonical key — and every status entry carries its target key.
Last updated: 2026-09-03

### C2 — Live refresh and the status cache
**Status:** `[x]` ✅ IMPLEMENTED
**Build:** Insert the 90 s TTL status cache at the plan seam in `TicketController`, so every resolution path consults the cache before fetching and spends at most one REST call per number per TTL window (R4). Add the refresh timer to the tickets view: while the focus stays open and details are present, each 60 s tick sends the `TicketAction::Mentions` refresh request; the daemon re-plans and pushes `TicketMentions` only when a status changed or was previously missing. A failed or rate-limited fetch follows R7.
**AC:**
- A tickets-view test drives the 60 s tick with a scripted clock: each tick while the focus stays open emits exactly one `TicketAction::Mentions` carrying the focused repo and number; after the focus closes, a further tick emits none (R6).
- A daemon test repeats one resolution inside the 90 s TTL and records zero new `gh` calls; the same test advances the fake clock past the TTL and records exactly one fresh call per number (R4).
- A daemon refresh test with no changed status records zero `TicketMentions` pushes (R6).
- A tickets-view draw test drives a refresh push that flips a status from open to closed and asserts the glyph changes in place (R6).
- A daemon test feeds a 403 with `Retry-After`, asserts the number stays plain with no error text in the body pane, and records no further `gh` calls for that number until the TTL window passes (R7).
- A daemon test feeds a 404, asserts the not-found status is cached, renders no glyph, and records no refetch before the TTL window passes (R7).
**Depends on:** C0 · **Traces to:** R4, R6, R7

Implementation notes: shipped in commit `d0ebd3d`. The controller keeps a 90 s status cache plus a failure map, so a rate-limited number waits for the TTL window; `force_push` on the details follow-up re-sends statuses to a fresh focus while an unchanged refresh pushes nothing. The tickets view owns the 60 s timer; the run loop sends the refresh through the sink without a toast.
Last updated: 2026-09-03

### C3 — Status icons in the inbox PR description
**Status:** `[x]` ✅ IMPLEMENTED
**Build:** Add `TicketAction::PrMentions` in `src/sock.rs`, resolved by `TicketController` against the PR body of the snapshot (open PRs live in `RepoSnapshot.prs`); a number with no snapshot entry answers an empty status set with no `gh` call (R11). The inbox stores one status map, sends `PrMentions` when a PR detail becomes the visible selection, merges the resulting push, and decorates the raw description text with the shared scanner before `wrapped_lines` runs.
**AC:**
- An inbox draw test with a PR whose description says `Closes #7` and a scripted statuses push shows the ok `●` before `#7` in the description block, with the `Closes` link line and all other text unchanged (R2, R8).
- A controller test shows `PrMentions` for an unknown PR number answers with an empty status set and no `gh` call (R11).
- Selecting an issue detail in the inbox records no `PrMentions` action; selecting a PR detail records exactly one (R2).
**Depends on:** C0 · **Traces to:** R2, R11

Implementation notes: shipped in commit `cd597e5`. Both inbox surfaces decorate: the gate decision path and the release-train path. The description decorates on the raw text before `wrapped_lines` and recolors the icon spans per wrapped line. The request goes out once per pull request identity, after key handling and state pushes.
Last updated: 2026-09-03

---

## Definition of Done

- `python3 ~/.claude/skills/writing-specs/scripts/validate_spec.py docs/superpowers/specs/2026-09-03-mention-status-icons-spec.md` exits 0 with no open `[NEEDS CLARIFICATION]` markers.
- Every requirement R1-R11 traces to at least one chunk; every chunk traces to at least one requirement.
- Each chunk keeps its own tests green (`cargo test`, plus the repo's `check.sh`) before the next chunk starts.
- The finished TUI shows no behavior change for bodies without mentions: zero `gh` calls, byte-identical rendering.
