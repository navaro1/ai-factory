# The Verification Toolbelt

Date: 2026-09-10 · Status: Ready · Scope: The run skill per surface, the driver ladder, the Before / After contract traced to ticket criteria, fast checks, refine grounding, review re-drive, setup, maintenance, and teach, as PR-sized chunks on top of v0.7 · Predecessors: docs/superpowers/specs/2026-09-10-verification-toolbelt-design.md (revision 3), docs/v0.7/SPEC.md

---

## 1. Objective & Non-Goals

**Objective.** Give every agent in the pipeline a quick, reliable, reusable way to prove that a change does what its ticket asks, on the real surface, and let the factory check the proof without a reviewer's tokens. The design record names every decision. This spec grounds them in the code and cuts them into chunks that stack on the v0.7 chunks they need.

**What NOT to build (non-goals):**
1. **No control CLI per surface.** A run skill names a generic driver. The factory ships no driver and installs none. [design §4.3, rule 20]
2. **No verifier subagent by default.** Fast checks plus the review re-drive give the separation. A verifier route for `very-high` is a follow-up. [design §5.5]
3. **No new cadence.** Maintenance rides the C24 audit sweep and the reviewer. [design §6.3] The v0.7 audit cadence on `sweep.days` fires that sweep, and `aif doctor --audit <alias>` fires it on demand.
4. **No principle naming in PR bodies.** Six principles are vocabulary in the stance, each backed by a structural check. [design §8]
5. **No media evidence.** Text only until `gh --attach` reaches stable. [design §14]
6. **No browser-profile integration.** Claude in Chrome and the Codex `@Chrome` are never a driver. [design refusal 5]
7. **No harness hooks.** The body check and the ticket check are daemon gates and cover every harness. Hooks are a follow-up. [design §14]
8. **No change to the governor's label state machine.** The one new label is `verify-skill`. [design §1]

---

## 2. Context & Sources (grounding)

**Reality check (2026-09-10).**
- The branch `verification-toolbelt-design` is at `fdaced0`, clean, three commits ahead of `main` at `0580f1f`. The crate is `aif` 0.6.0 (`Cargo.toml:3`). `globset = "0.4"` is a dependency (`Cargo.toml:21`), so the C10 glob work has its crate.
- Of the v0.7 chunks, only C0 has landed: `src/theory/` holds `model.rs` and `mod.rs`, and `ExecutionRole` carries `TheoryAudit` and `TheoryChat` (`src/config.rs:26-27`). `TheoryConfig { governor, window, theory, sweep, cards, interview }` exists with `checkout(repo_path)` (`src/config.rs:328-357`). No `src/theory/verify.rs`, `records.rs`, `measure.rs`, `cadence.rs`, no `src/tui/theory.rs`, no `aif measure` (`src/bin/aif.rs:54-64` lists `Tui`, `Stop`, `Doctor`). Every chunk below names the v0.7 chunk it stacks on, and none of those chunks is built. The order in section 7 is the order of implementation once its v0.7 dependencies exist.
- `docs/v0.7/` holds only `SPEC.md`. The prompt copies pinned by the byte-for-byte test all live in `docs/v0.8/prompts/`, the two ticket copies included.
- `tmux` is installed at `/usr/bin/tmux`. `chromium-cli`, `playwright-cli`, and `agent-browser` are not installed on this machine. `pandoc` and Python `markdown` 3.10.1 are.
- Claude Code 2.1.266 bundles the skills `run`, `verify`, and `run-skill-generator`. The `run` skill looks for a project skill under `.claude/skills/` whose description names launching the app, else falls back to one example per project type: CLI, server, TUI over `tmux`, Electron, browser over `chromium-cli`, library. The generator writes `.claude/skills/run-<name>/`. [code.claude.com/docs/en/skills, section "Run and verify your app"]

**Existing code / consumer contracts (verified by reading the cited lines).**
- Configuration: `RepoConfig { alias, path, owner_repo, lanes, release, theory, role_overrides, tag_route_overrides }` (`src/config.rs:380-389`). Every raw table carries `deny_unknown_fields` (`src/config.rs:218`, `:775-813`). `TheoryConfig::checkout` returns the theory path when set, else the repository path (`src/config.rs:353-357`).
- Dispatch: `dispatch_one` picks the cwd through `self.workspace(&task)`: `Workspace::Shared` is the repository path, and `Workspace::Exclusive` is the issue, PR, or train worktree (`src/daemon.rs:2290-2302`). `task_cwd` mirrors it (`src/daemon.rs:5218-5229`). `render_prompt` reads the template through `prompt_template(role)` and fills it from `placeholder_values` (`src/daemon.rs:5441-5470`). `admit_ready` (`src/daemon.rs:1538`), `prior_stage_active` (`src/daemon.rs:2058`), `finish_train` (`src/daemon.rs:2500`), `fire_train` (`src/daemon.rs:5127`), `next_deadline` (`src/daemon.rs:801`), `apply_poll` (`src/daemon.rs:1177`).
- Worktrees: `WorktreeKind { Issue, Pr }` and `WORKTREE_KINDS` (`src/worktree.rs:45-64`); `ensure_issue` (`src/worktree.rs:172`), `ensure_pr` (`src/worktree.rs:190`). The `.aif/` directory is the marker directory and is git-excluded (`src/worktree.rs:19-22`), which makes `.aif/evidence/` a free evidence home.
- Prompts: six consts (`src/prompts.rs:43-358`), `ROLES` with the two theory roles outside it (`src/prompts.rs:424`, `:735-741`), the byte-for-byte docs test (`src/prompts.rs:976`).
- GitHub: `create_issue(owner_repo, title, body)` without labels (`src/gh.rs:514`), `add_label` (`src/gh.rs:418`), `add_label_names` (`src/gh.rs:438`), `create_label` (`src/gh.rs:280`), `update_issue` (`src/gh.rs:309`). C32 adds labels to `create_issue`.
- Doctor: `Status { Warn, Fail, .. }` (`src/doctor.rs:113-121`), `Check` (`src/doctor.rs:138`), `report` (`src/doctor.rs:177`), `print_report` (`src/doctor.rs:216`).
- Tasks: `TaskPurpose { Pipeline, TicketCreate, TicketChat }` (`src/tasks.rs:63-70`), `scoped_id` (`src/tasks.rs:175`), `MAX_ATTEMPTS = 3` (`src/tasks.rs:21`).
- Routing: `complexity:` and `review-complexity:` prefixes and the closed scale (`src/routing.rs:29-41`).
- Scheduler: `Limits`, `Reason`, `can_start` (`src/sched.rs:22`, `:79`, `:159`). Links: `Links::derive(repo, snap)` per poll (`src/links.rs:33`). Decisions: `DecisionKind`, `Response` (`src/decisions.rs:20`, `:228`). State: `role_bindings` (`src/state.rs:148-171`). Runners: `RunnerFactory::build(&role)` (`src/runner/mod.rs:54-61`). Ticket proposal parser: `parse_ticket_proposal` (`src/ticket.rs:893`).
- No existing code for: a skill parser, a feature index, a Before / After grammar, a ticket check, a tier, or a teach purpose. Each is a new module or a new arm.

**External references (durable copies, if any).**
- `docs/superpowers/specs/2026-09-10-verification-toolbelt-design.md` — the design record, revision 3: rules 13 to 23, refusals 4 and 5, the driver ladder, the contract, the six-group build order that section 7 cuts into twelve chunks.
- `docs/v0.7/SPEC.md` — R23 (the area schema), R27 and C11 (the body check), C10 (placeholders and the prompt rewrite), C16 (purposes and the gate skip), C17 (chat machinery), C24 (the audit sweep), C25 (the map parser), C26 to C28 (measure tasks and the base worktree), C32 (`create_issue` with labels).
- The pstack plugin 0.15.1, `skills/poteto-mode/scripts/check-plan.mjs` — the three prose lint rules: long dash, curly quote, mid-sentence colon.

---

## 3. Requirements & Acceptance Criteria

Functional requirements (EARS). The design record section is in brackets.

Stance:
- **R1** — `docs/STANCE.md` shall carry rules 13 to 23, refusals 4 and 5, the driver ladder, the six principles with the structure that enforces each, and every new term of this spec: surface, run skill, driver, tier, floor, feature, fast command, fast check, Before / After line, acceptance criterion, trace, lever, teach. [§1, §2, §3, §8]

The run skill:
- **R2** — WHEN the polled default branch of the skills checkout moves, the daemon shall read every `.claude/skills/run-*/SKILL.md` and `features/*.md` at that commit through `git show`, parse the front matter (`name`, `description`, `surface`, `driver`, `tier`, `blind` on the skill; `area`, `fast` on a feature), ship the result or the first error in the state view, and never stop on a parse error. [§4.1, §4.2, §4.4]
- **R3** — The daemon shall resolve the skills of an area in this order: the `skills` list of the area in `theory/verify.toml`, then every feature whose `area` equals the area id. The operator's list wins on conflict. [§4.1]
- **R4** — IF a feature names an `area` that is not in `theory/verify.toml`, or a skill names a `tier` outside `browser | dom | http | terminal | none`, THEN the daemon shall ship one lint finding that names the file, and the doctor shall print it. [§4.1]
- **R5** — WHEN `factory.toml` names a repository, the parser shall accept `skills = { path }` (optional); the skills checkout shall be that path when set, else `TheoryConfig::checkout`. Unknown fields stay rejected. [§4.5]
- **R6** — The `[[area]]` table of `theory/verify.toml` shall accept `min_tier` (optional, one of the five tiers, default `none`). [§4.3]
- **R7** — `aif doctor` shall print one line per repository and surface: `run skill <alias>/<surface>: <tier>`, `missing`, or `lint: <file>: <reason>`; one `Warn` per area whose `min_tier` is above the tier of every surface that maps to it; and one `Warn` per complexity level whose implement route and review route resolve to the same model family. [§6.1]
- **R8** — The Theory view AREAS panel shall show the tier per area: the tier name, `-` for none, `!` for a lint finding or a floor above reach. [§12]

Slicing:
- **R9** — WHEN the daemon renders a stage prompt, it shall fill `{skills}` with content, not names: for refine the feature index and the Run and Fast sections of each surface of the short prediction's areas, plus Drive and Logs for a `bug` ticket; for implement the whole skill file of each surface of the full prediction's areas and the feature files that bind to those areas; for review the implement slice over the areas of `git diff --name-only <base>...<head>`; for teach the feature files of its areas. Past two surfaces or six feature files the daemon shall inline the index only and name the paths. [§4.6]

Fast checks:
- **R10** — WHEN a governed PR enters review admission, the daemon shall queue one measure task per touched feature that names a `fast` command, run it in the head worktree through the script runner, record `{ id: <feature>, value: <exit code>, unit: "exit", direction: "lower" }`, and hold the review in `prior_stage_active` until every fast task ends. [§5.3]
- **R11** — IF any fast record has a non-zero value, THEN the daemon shall not dispatch the review, shall post one finding comment `fast check failed: <feature> exit <n>` on the theory record, and shall re-queue the implement task of every linked ticket. [§5.3]

Refine:
- **R12** — The refine task shall run in the issue worktree, not the repository checkout. [§5.4]
- **R13** — The refine prompt shall require, before the plan table: a restatement that opens `## Problem`; a `## Grounding` section with the mechanism, the history, and the paths; a `## Decisions` section with one line per open question, its answer, and the command that answered it; for a `bug` ticket a `## Repro` section with the command, two observed outputs, and the exit code; and `## Acceptance criteria` as lines `- AC-<n> · <falsifiable statement> · check: <feature drive | feature fast | measure <id>>`. The plan table shall gain a `Fast` column. [§5.4]
- **R14** — WHEN a refined ticket becomes implement-ready, the daemon shall check its body before dispatch: the sections of R13 present, every criterion line carries `AC-<n>` and `check:`, every check names a feature of the resolved skills or a measurer of `verify.toml`, and the plan table parses with real owned paths. A failure shall post one finding comment and re-queue the refine task. [§5.4]

The contract:
- **R15** — The body check of C11 shall gain, for a governed PR: `## Before / After` present with lines `- AC-<n> · <feature | measurer> · <tier | measure> · <command> · before: <text> · after: <text>`; `## Summary` and `## Test plan` absent; every `AC-<n>` of the linked ticket in at least one line and every line naming an existing one (trace); every touched area with a resolved skill in at least one line (coverage); every line's tier at or above the area's `min_tier` (tier); no line with `inconclusive` in its after text (state); every changed path under an owned path of the plan table, under `.claude/skills/run-*/`, or a test file, and a dependency manifest changed only when the ticket names the dependency (scope); no long dash, no curly quote, no mid-sentence colon outside code, and at most 40 prose lines (prose). A failure shall post the finding and re-queue implement, as C11 does. [§5.1, §5.2]
- **R16** — IF the tier line fails, THEN the daemon shall also open one theory event through `open_event` on the record of the PR with the area, the floor, and the tier reached. [§4.3]

Prompts:
- **R17** — The implement prompt shall carry: the simplest-change paragraph (read the conventions of touched files first, the smallest change that meets every criterion, no abstraction or dependency no criterion needs, delete dead weight in its own commit, strip narrating comments and unasked guards before each commit); drive every touched feature once and write one Before / After line per criterion from what was observed; run every fast command and paste the exit code; the lever rule; every added test asserts a literal result through the public path and fails with the change reverted; the body contract of R15 with the writing paragraph. [§5.1, §5.5]
- **R18** — The review prompt shall carry: re-run every Before / After command on the head; for a `bug` ticket run `## Repro` in the base worktree and expect the before, then on the head and expect the after; run every added test against the base worktree with only the test files applied and expect a failure; run every lever the PR adds; remove what no criterion needs and align style deviations; post the reviewer's own lines as a comment in the same shape; a bug that does not fail on base goes to `needs-human` with both outputs; repair run-skill drift met during a re-drive. [§5.6]

Setup and maintenance:
- **R19** — WHEN the operator presses `v` on a repository row of the Theory view and enters a surface name, the daemon shall create the ticket `Create the run skill for <alias>/<surface>` with labels `to-refine` and `verify-skill` and the body of design §6.2, through `create_issue` with labels. [§6.2]
- **R20** — WHILE a ticket or PR carries `verify-skill`, the daemon shall skip both prediction gates for it, like `model-pr`, and run the ticket check, the body check, the fast checks, and the review as for any governed item. In shadow mode the agent writes to `{skills_dir}` and the daemon opens the skills PR on the theory repository through the C7 commit-push-PR path on branch `aif/<alias>/skills-<n>`. [§6.2, §4.5]
- **R21** — The audit sweep prompt of C24 shall gain one paragraph: check each run skill's Run and Fast sections and each feature's handles against the code and report dead paths and dead handles as one `<aif-event-v1>` block per surface with kind `skill-drift`; WHEN the sweep reports drift for a surface with no open `verify-skill` ticket, the daemon shall create `Maintain the run skill for <alias>/<surface>` with `to-refine` and `verify-skill` and the eight-line maintain body. [§6.3]

Teach:
- **R22** — WHEN the operator presses `t` on a merged PR row, a DELTAS row, or an AREAS row, the daemon shall start a `theory.chat` task with purpose `Teach`, id `<alias>/teach-<kind>-<key>`, and the teach prompt with `{model}`, `{skills}`, the diff or the area, and the git and PR history; the agent shall end with zero or more `<aif-event-v1>` blocks, one per contradiction, and each shall open an event through `open_event`. [§7]
- **R23** — WHEN a card is answered with cause `recall`, the inbox row shall offer `t` and start the teach task for the card's PR or entry. [§7]

Non-functional rules. Each has an ID and a tracing chunk.
- **N1** — Every derivation of this spec rebuilds per poll and persists nothing, like `Links`. The one persisted exception is the cadence last-fire list in `StateFile.cadences`.
- **N2** — The wire revision is 5 and stays 5: every new `StateView` field carries `#[serde(default)]`, every new `Action`, `TheoryAction`, and `Push` variant joins `every_action()`.
- **N3** — The ticket check, the body check, and the fast checks run no harness and spend no agent tokens.
- **N4** — A fast task that exceeds its timeout, default 120 s, yields value `124`, so it fails.
- **N5** — Every stage prompt rewrite lands once, with its docs copy pinned byte for byte under `docs/v0.8/prompts/`.
- **N6** — `./check.sh` passes after every chunk.

Acceptance (Given/When/Then, representative):
- *Given* `.claude/skills/run-web/SKILL.md` with `tier: browser` and `features/checkout.md` with `area: web-checkout`, *when* the daemon polls, *then* the AREAS row of `web-checkout` reads `browser` and the doctor prints `run skill borsuk/web: browser`.
- *Given* a feature with `area: nope`, *when* the daemon polls, *then* the state view carries `lint: features/x.md: area nope unknown` and the daemon keeps running.
- *Given* a refined ticket whose `AC-2` names `check: api-orders fast` and no feature `api-orders` exists, *when* implement would dispatch, *then* no implement task starts, the ticket carries the comment `ticket: AC-2 check api-orders is not a feature or a measurer`, and one refine task is queued.
- *Given* a PR whose ticket has `AC-1` and `AC-2` and whose body has a line for `AC-1` only, *when* review admission runs, *then* the comment reads `PR: AC-2 has no Before / After line` and the implement task is queued.
- *Given* a feature with `fast: cargo test --test cli` that exits 1 at the head, *when* review admission runs, *then* no review task dispatches, the comment reads `fast check failed: cli exit 1`, and the implement task is queued.
- *Given* `min_tier = "browser"` on `web-checkout` and a line with tier `http`, *when* the body check runs, *then* the finding names the floor and one `THEORY` inbox row appears.
- *Given* a refine task on a governed ticket, *when* it dispatches, *then* its cwd is `<state_dir>/worktrees/borsuk/issue-<n>`.
- *Given* the operator presses `v` on `borsuk` and types `web`, *when* the daemon handles it, *then* `create_issue` is called with the title `Create the run skill for borsuk/web` and the labels `to-refine` and `verify-skill`.

---

## 4. Design (HOW)

**Architecture** — one new module parses and slices the run skills, one new module holds the contract grammar and the two deterministic checks, the measure machinery of C26 to C28 runs the fast commands, and the prompts carry the rest. Every check is a pure function over strings the daemon already has.

```
src/config.rs            RepoConfig gains skills: Option<SkillsPath>; RawRepo gains `skills`
src/theory/skills.rs     new: SkillSet, RunSkill, Feature, Tier, parse_skill, parse_feature, lint,
                         resolve(area, verify) -> Vec<&Feature>, slice(stage, areas) -> String
src/theory/verify.rs     (C25) Area gains min_tier: Tier
src/theory/contract.rs   new: Criterion, parse_criteria, check_ticket, BeforeAfterLine, parse_lines,
                         check_body_lines (trace, coverage, tier, state, scope, prose)
src/theory/records.rs    (C11) check_pr calls contract::check_body_lines for a governed PR
src/theory/measure.rs    (C26) Record with unit "exit"; queue_fast(alias, worktree, features)
src/daemon.rs            Workspace for refine -> Exclusive(Issue); fast tasks in review admission;
                         ticket check in implement admission; skills cache next to the model cache;
                         TeachJob purpose; setup and maintain ticket creation; sweep drift handler
src/tasks.rs             TaskPurpose gains Teach(TeachKey)
src/prompts.rs           TEACH_PROMPT; REFINE, IMPLEMENT, REVIEW rewritten; AUDIT_SWEEP_PROMPT gains
                         the drift paragraph (C24); docs/v0.8/prompts/*.md pinned
src/doctor.rs            skill lines, floor warning, family warning
src/routing.rs           model_family(&str) -> &str for the family warning
src/sock.rs              TheoryView.skills (serde default), TheoryAction::{Setup, Teach}
src/tui/theory.rs        (C1, C25) AREAS tier mark, keys v and t
src/tui/inbox.rs         (C14, C22) the recall offer
docs/STANCE.md           (C3) the addendum
docs/v0.8/prompts/       the pinned copies
```

**Data flow and contracts.**
- The skills cache. Next to the model cache of C1, keyed by the skills checkout commit: `(commit, Result<SkillSet, String>)`. `SkillSet { surfaces: BTreeMap<String, RunSkill>, features: Vec<Feature>, lint: Vec<Finding> }`. `RunSkill { name, description, surface, driver, tier, blind, body: String, sections: BTreeMap<String, String> }` with the six section names of design §4.2 as keys. `Feature { surface, id, area, fast: Option<String>, body }`. Files are read with `git show <commit>:<path>` after `git ls-tree -r --name-only <commit> .claude/skills/` filters `run-*/`.
- `Tier` is a closed enum `None < Terminal < Http < Dom < Browser` with `Ord`. `min_tier` parses into it. The floor check is `line.tier >= area.min_tier`.
- The slice. `slice(stage, areas, verify, skills) -> String` renders sections by the table of R9 and applies the cap. The daemon fills `{skills}` with it in `placeholder_values`, replacing the name fill of C28.
- The contract grammar. A criterion line is `- AC-<n> · <text> · check: <target>`, where `<target>` is `<feature> drive`, `<feature> fast`, or `measure <id>`. A Before / After line is `- AC-<n> · <feature | measurer> · <tier | measure> · <command> · before: <text> · after: <text>`. The separator is ` · ` (U+00B7 with spaces). `parse_lines` returns one struct per line and one finding per malformed line.
- The ticket check runs in `admit_ready` where `implement_ready` admits a governed ticket, after the C9 prediction gate and before the task exists. Its inputs are the issue body from the snapshot and the resolved skills. A failure posts through `post_issue_comment` and adds `to-refine` back, which the poll gate turns into a refine task. That reuses the v0.6 refine path and needs no new task plumbing.
- The body check. `check_pr(body, paths, branch)` of C11 gains a fourth input, `ctx: &ContractContext { criteria, features, areas, min_tiers, owned_paths, manifests }`, and calls `check_body_lines`. The finding text names the line and the rule. The prose rules are the three of `check-plan.mjs` plus the line cap, applied to text outside fenced blocks.
- Fast tasks. `queue_fast` reuses `queue_measure` with a synthetic measurer `{ id: <feature>, command: <fast>, mode: pr, timeout_s: 120 }` and the `exit` unit. The task id is `<alias>/fast-<tree8>-<feature>`. The comparison of C27 treats `exit` like any `lower` value. Review admission holds on both the base and head measure tasks of C28 and the fast tasks; the fail rule runs when the last one ends. C28 is a later chunk and is not built, so the fast tasks alone hold the review today, and no base and head comment exists yet.
- Refine cwd. `workspace(&task)` returns `Exclusive(Issue(number))` for `Stage::Refine` on an issue, except the ticket-creation task of `is_ticket_creation`, which keeps `Shared`.
- Setup and maintain tickets. One function `create_skill_ticket(alias, surface, kind)` builds the title, the labels, and the body from two consts `SETUP_BODY` and `MAINTAIN_BODY` with `{alias}`, `{surface}`, `{skills_dir}`, `{app_path}` filled, and calls `create_issue` with labels (C32). The gate skip reads the `verify-skill` label the way C16 reads `model-pr`.
- Teach. `TaskPurpose::Teach(TeachKey::{Pr(n), Delta(n), Area(id)})` with a `PurposeSpec` under `theory.chat`, `wants_final_block` returning the event tag, and `open_event` per block, all through the C16 and C17 seams.
- Doctor. `skill_checks(config, skills_by_alias, verify_by_alias, routes) -> Vec<Check>`.

**Extension axes.**

| Axis | State | One variant touches |
|---|---|---|
| Tiers | closed | none; the ladder is the design's ladder |
| Skill sections | closed | none; six names |
| Body check rules | open | one function in `src/theory/contract.rs` and one row in its table test |
| Check targets of a criterion | open | one arm in `parse_criteria` |
| Ticket kinds under `verify-skill` | open | one body const and one arm in `create_skill_ticket` |
| Teach keys | open | one `TeachKey` arm and one prompt placeholder |
| Drivers | open | none in code; a skill names any driver whose tier it declares |

**Error handling / graceful degradation.** A skill or feature parse error ships as a lint finding and blocks nothing; the doctor and the AREAS mark show it, and `{skills}` renders the files that parsed. A missing skills directory is an empty `SkillSet`, and every check that needs a skill is inert for that area, so v0.7 behaviour holds for a repository with no run skill. A fast command that times out fails the check with value `124`. A failed `create_issue` fails the action with the `gh` error and creates nothing. A ticket check failure never loops: the refine task carries the finding, and `MAX_ATTEMPTS` still bounds it.

---

## 5. Boundaries

- ✅ **Always:** every check is a pure function the daemon runs with no harness; every derivation rebuilds per poll; the operator's `verify.toml` wins over a feature's front matter; the code repository in shadow mode receives no skill file and no comment, and the only labels it receives are the ticket's own pipeline labels (`to-refine`, `verify-skill`, and the v0.6 stage labels), because the daemon polls only the code repository and a ticket elsewhere would never reach refine or implement; `./check.sh` stays green after every chunk.
- ⚠️ **Ask-first:** the setup ticket is created only on the operator's `v`; a maintain ticket is created only when the sweep reports drift; a skill PR merges through the release train and its policy.
- 🚫 **Never:** an agent installs a driver; an agent edits `theory/verify.toml`; the factory names a browser-profile integration in any prompt; a verifier subagent runs by default; a Before / After line is written by the daemon from agent claims.

---

## 6. Open Questions

All questions are resolved. None waits for clarification.

1. **Skill path** — `.claude/skills/run-<surface>/` in the skills checkout. Reason: Claude Code's generator and `/run` read it as is; the daemon inlines it for the other harnesses. (User decision, 2026-09-10.)
2. **Verifier subagent** — none by default; a `very-high` route is a follow-up. (User decision, 2026-09-10.)
3. **TOML fields** — `skills = { path = "..." }` on the repository table; `min_tier = "<tier>"` on `[[area]]`. Front matter keys: `surface`, `driver`, `tier`, `blind` on a skill; `area`, `fast` on a feature.
4. **Caps** — 40 prose lines in the body, 30 lines per fenced transcript, two surfaces and six feature files per prompt, 120 s per fast command. Constants in `src/theory/contract.rs` and `src/theory/skills.rs`, not configuration, until a repository needs otherwise.
5. **The `v` prompt** — inline text for the surface name, like the short prediction of C8. A pick list from `verify.toml` boundaries is a later nicety.
6. **Shadow mode skill PRs** — the agent writes to `{skills_dir}`, the daemon commits, pushes, and opens the PR on the theory repository through the C7 path on `aif/<alias>/skills-<n>`. Reason: C7 already owns commit-push-PR on the theory checkout.
7. **The separator** — ` · ` (U+00B7). Reason: it is what the design record's examples use, it never appears in a shell command, and pandoc keeps it.
8. **Ticket check placement** — in `admit_ready`, before the implement task exists, with `to-refine` re-added on failure. Reason: it reuses the v0.6 refine path and touches no dispatch code.
9. **Fast task timeout** — 120 s, value `124` on timeout, the same convention as `timeout(1)`.

---

## 7. Chunks and Acceptance Criteria

Twelve chunks. Every chunk names its v0.7 base on its own line: the C chunk it stacks on, what that chunk provides, and whether it is built at the date of this spec. `Depends on` lists every C id the chunk uses directly, and every V id. The design record's six groups map to them as: V0 is group V0; V1 to V3 are group V1; V4 is group V3; V5 to V8 are group V4; V9 and V10 are group V2 and the maintenance half of V4; V11 is group V5. `Depends on` names v0.7 chunks by their C ids and chunks of this spec by their V ids.

### V0 — The stance addendum
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C3 (`docs/STANCE.md` and its vocabulary test over `include_str!`). Built: no.
**Build:** Extend `docs/STANCE.md` with rules 13 to 23, refusals 4 and 5, the driver ladder table, the six principles with the structure that enforces each, the steering table, and every new term of R1 in the vocabulary table. Extend the C3 vocabulary test so it asserts the new terms, the five tier names, and the label `verify-skill`.
**AC:**
- The vocabulary test fails when `verify-skill` or any of the five tier names is removed from the table and passes on the committed file.
- The README link test of C3 still passes.
**Depends on:** C3 · **Traces to:** R1

### V1 — The run skill parser and the AREAS mark
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C1 (the model cache in `apply_poll`, `TheoryView` in the state view, the Theory view shell) and C25 (`src/theory/verify.rs`, `Area`, `VerifyMap::parse`, the AREAS panel). The skills cache sits next to the C1 model cache, `min_tier` joins the C25 `Area`, and the tier mark joins the C25 AREAS row. Built: no.
**Build:** Add `src/theory/skills.rs` with `Tier`, `RunSkill`, `Feature`, `SkillSet`, `parse_skill`, `parse_feature`, and `lint(set, verify)`. Add `SkillsPath` to `RepoConfig` and `skills = { path }` to `RawRepo`, and `skills_checkout(repo)` that prefers it over `TheoryConfig::checkout`. Add `min_tier` to `Area` in `src/theory/verify.rs`. In `apply_poll`, next to the model read of C1, list `.claude/skills/run-*/` at the skills checkout commit and read each file through `git show`; cache `(commit, Result<SkillSet, String>)`; ship `TheoryView.skills: BTreeMap<String, SurfaceView { tier, features, lint }>` with a serde default. Draw the tier mark on the AREAS row of C25 by `resolve`.
**AC:**
- A parse test accepts a skill with all five front-matter keys and a feature with `area` and `fast`, and rejects `tier: pixels` with a finding that names the file.
- A lint test reports `features/x.md: area nope unknown` for a feature whose area is not in the map, and nothing for a bound one.
- A config test accepts `skills = { path = "/tmp/s" }`, resolves `skills_checkout` to it, and rejects `skills = { paht = "x" }`.
- A daemon test with `ScriptExec` runs one `git ls-tree` and one `git show` per file when the commit moves, none when it does not, and keeps running on a parse error.
- A Theory view test renders `web-checkout · browser` and `api-orders · -`.
**Depends on:** C1, C25 · **Traces to:** R2, R3, R4, R5, R6, R8, N1, N2

### V2 — The doctor lines
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C2 (`theory_checks` in `src/doctor.rs`, the per-repository `Warn` and `Fail` lines) and C25 (the areas the floor warning reads). The family warning reads the v0.6 tag routes of `src/routing.rs`, which exist. Built: C2 and C25 no, routing yes.
**Build:** Add `skill_checks` to `src/doctor.rs`: one line per repository and surface from the skills checkout on disk (`run skill <alias>/<surface>: <tier>` or `missing`), one line per lint finding, one `Warn` per area whose `min_tier` exceeds every mapped surface's tier, and one `Warn` per complexity level whose implement and review routes share a model family. Add `model_family` to `src/routing.rs`: the slug prefix before the first `-` after the vendor, so `claude-opus-5` and `claude-fable-5-1` share `claude`, and `gpt-5.6-sol` and `gpt-6-astra` share `gpt`.
**AC:**
- A doctor test with one surface at `browser` prints `run skill borsuk/web: browser`; with no skills directory prints `run skill borsuk: missing`.
- A doctor test with `min_tier = "browser"` on an area whose only surface is `http` prints one `Warn` that names the area and both tiers.
- A doctor test with implement `claude-opus-5` and review `claude-fable-5-1` at `high` prints one `Warn` naming `high`; with `gpt-5.6-sol` for review prints none.
- `tests/cli.rs` still pins the doctor help unchanged.
**Depends on:** C2, C25, V1 · **Traces to:** R7

### V3 — Slicing into `{skills}`
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C10 (`{skills}` in the complete placeholder set of `render_prompt`, `Model::areas_for_paths`, the once-only prompt rewrite) and C28 (the name fill of `{skills}` in the review and implement prompts, which this chunk replaces). Built: no.
**Build:** Add `slice(stage, areas, verify, set) -> String` to `src/theory/skills.rs` per the table of R9, with the cap of two surfaces and six feature files, rendering `<!-- skills: index only, N files -->` and the paths past the cap. In `placeholder_values`, fill `{skills}` with it for refine, implement, review, and teach, replacing the name fill of C28. A `bug` label on the ticket adds Drive and Logs to the refine slice.
**AC:**
- A slice test for implement with one area and one feature renders the whole skill body and the feature file, and for refine renders only the index and the Run and Fast sections.
- A slice test with three surfaces renders the index-only marker and three paths.
- A dispatch test asserts the review prompt contains the feature file of the area that `git diff --name-only` touched and not the file of an untouched area.
- A refine dispatch of a `bug` ticket renders the Drive section; a plain ticket does not.
**Depends on:** C10, C28, V1 · **Traces to:** R9

### V4 — Fast checks at review admission
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C26 (`ScriptRunner`, `TaskPurpose::Measure`, `queue_measure`, `Record`, `parse_lines`), C27 (`tree_hash`, the cache, `compare`), and C28 (the base and head measure tasks at review admission, the `prior_stage_active` hold, `WorktreeKind::Base`). The fast task is one more `queue_measure` call with a synthetic measurer. Built: no.
**Build:** Add `queue_fast(alias, worktree, features)` in `src/daemon.rs` over `queue_measure` with a synthetic measurer per feature (`mode: pr`, `timeout_s: 120`) and the `exit` unit in `src/theory/measure.rs`. In review admission of a governed PR, queue one fast task per touched feature with a `fast` command and hold the review in `prior_stage_active` until every task ends. C28 is a later chunk and is not built, so no base and head tasks run and no base and head comment exists yet. When the last one ends, read the fast records: on any non-zero value, cancel the review task, post `fast check failed: <feature> exit <n>` on the theory record, and re-queue the implement task of every linked ticket. Show `fast check failed` in the pipeline hint.
**AC:**
- A daemon test with two touched features queues two fast tasks with ids `borsuk/fast-<tree8>-checkout` and `borsuk/fast-<tree8>-orders`, and the review stays held until both end.
- A fast task whose script exits 1 leaves no review task, posts the finding with `exit 1`, and queues one implement task per linked ticket.
- A fast task whose script sleeps past 120 s records value `124` and fails the check.
- Two fast tasks that exit 0 release the review. C28 is a later chunk and is not built, so no base and head comment exists yet.
**Depends on:** C26, C27, C28, V1 · **Traces to:** R10, R11, N3, N4

### V5 — Refine in the issue worktree
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C10 (the refine prompt's `{model}` slice and the pinned `docs/v0.7/prompts/refine.md` this chunk edits). The cwd change itself touches only `workspace(&task)` in `src/daemon.rs:2290-2302`, which exists. Built: C10 no.
**Build:** Change `workspace(&task)` so `Stage::Refine` on an issue returns `Exclusive(Issue(number))`, except `is_ticket_creation`, which stays `Shared`. `ensure_issue` already cuts the branch from the default base. Update the refine prompt's first paragraph to name `{worktree}` as its own git worktree, and the README table of the four stages.
**AC:**
- A dispatch test asserts the refine cwd is `<state_dir>/worktrees/borsuk/issue-42` and one `git worktree add` call.
- A ticket-creation dispatch keeps the repository path as cwd.
- The doctor `--clean` preview of C2 lists a refine worktree of a closed ticket as removable.
**Depends on:** C10 · **Traces to:** R12

### V6 — The refine prompt and the criteria
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C10 (the once-only rewrite rule, the placeholder set, and the `docs/v0.7/prompts/` pin that this chunk moves to `docs/v0.8/prompts/`). Built: no.
**Build:** Rewrite `REFINE_PROMPT` once: the Restate, Ground, Decisions, and Repro sections of R13 in that order before the plan table; `## Acceptance criteria` as `- AC-<n> · <statement> · check: <target>` lines with the three target forms; the `Fast` column in the plan table; prototype-before-ask under rule 14; repro twice for a `bug` ticket; a criterion the ticket text does not ask for is dropped. Add `parse_criteria` to `src/theory/contract.rs`. Move the pinned copies to `docs/v0.8/prompts/` and re-pin the byte-for-byte test for every stage prompt.
**AC:**
- `parse_criteria` reads three lines and returns three criteria with ids 1 to 3 and their targets, and returns one finding for a line without `check:`.
- The prompt test asserts `REFINE_PROMPT` contains `## Acceptance criteria`, `AC-<n>`, `check:`, `## Repro`, and the `Fast` column header, and does not contain the v0.7 acceptance wording.
- The docs test pins `docs/v0.8/prompts/refine.md` byte for byte.
**Depends on:** C10, V3, V5 · **Traces to:** R13, N5

### V7 — The ticket check
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C9 (the `theory-full` gate on `implement_ready` in `admit_ready`, after which the ticket check runs), C4 (`GhClient::post_comment` and `TheoryRecords`), and C25 (the measurers a `check:` target may name). Built: no.
**Build:** Add `check_ticket(body, features, measurers) -> Result<(), Finding>` to `src/theory/contract.rs`: the sections of R13 present, every criterion parses, every target names a resolved feature or a measurer, the plan table parses and every owned path is non-empty. In `admit_ready`, for a governed ticket that `implement_ready` admits, run it after the C9 gate and before the task exists; on failure post `ticket: <finding>` and add `to-refine`, so the poll gate queues a refine task.
**AC:**
- A table test yields the finding text for each rule: missing section, criterion without `check:`, unknown target, empty owned path.
- A daemon test with a ticket whose `AC-2` names `api-orders fast` and no such feature queues no implement task, posts `ticket: AC-2 check api-orders is not a feature or a measurer`, and re-adds `to-refine`.
- A ticket with the governor off is not checked.
**Depends on:** C4, C9, C25, V6 · **Traces to:** R14, N3

### V8 — The Before / After contract and the body check
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C11 (`records::check_pr(body, paths, branch)` and its slot in `dispatch_one` after the cwd exists and before the prompt), C14 (`Daemon::open_event` for the floor failure), and C28 (`WorktreeKind::Base`, which the review prompt's red-on-base step drives). Built: no.
**Build:** Add `BeforeAfterLine`, `parse_lines`, and `check_body_lines(body, ctx)` to `src/theory/contract.rs` with the seven rules of R15 as one function each and one table test row each. Extend `check_pr` of C11 with a `ContractContext` built from the linked ticket's criteria, the resolved features, the touched areas and their `min_tier`, the plan's owned paths, and the manifest names `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`. On a tier failure also call `open_event` with the area, the floor, and the tier reached. Rewrite `IMPLEMENT_PROMPT` and `REVIEW_PROMPT` once with the paragraphs of R17 and R18 and the body contract with its writing paragraph, and pin `docs/v0.8/prompts/implement.md` and `review.md`.
**AC:**
- `parse_lines` reads the three example lines of design §5.1 and returns criterion ids 1 to 3, tiers `browser`, `http`, `measure`, and rejects a line with four fields.
- The table test yields one finding per rule: `AC-2 has no Before / After line`, `line 3 names AC-9 which the ticket lacks`, `area web-checkout has no line`, `line 1 tier http is below the floor browser`, `line 2 is inconclusive`, `src/other.rs is outside the plan`, `Cargo.toml changed and the ticket names no dependency`, `long dash at line 4`, `body has 41 prose lines`.
- A dispatch test with a tier failure posts the finding, re-queues implement, and opens one event whose text names the floor.
- The prompt tests assert `IMPLEMENT_PROMPT` contains `## Before / After`, `smallest change`, and `fails with the change reverted`, and `REVIEW_PROMPT` contains `base worktree`, `only the test files`, and `no criterion needs`.
- The docs test pins the three v0.8 stage prompts byte for byte.
**Depends on:** C11, C14, C28, V4, V7 · **Traces to:** R15, R16, R17, R18, N3, N5

### V9 — The setup ticket
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C16 (`PurposeSpec` and the `model-pr` gate skip in `admit_ready`, which `verify-skill` mirrors), C32 (`GhClient::create_issue` with labels), C8 (the inline input precedent of the short prediction), C7 (the commit-push-PR path on the theory checkout that shadow mode reuses), and C20 (shadow mode routing). Built: no.
**Build:** Add `SETUP_BODY` in `src/prompts.rs` with the eight steps of design §6.2 and the placeholders `{alias}`, `{surface}`, `{skills_dir}`, `{app_path}`. Add `TheoryAction::Setup { repo, surface }` and key `v` on a repository row of the Theory view with an inline input for the surface, like the short prediction of C8. Add `create_skill_ticket` in `src/daemon.rs` that calls `create_issue` with the title and the labels `to-refine` and `verify-skill`. Read `verify-skill` in `admit_ready` next to `model-pr` of C16 and skip both prediction gates for it. In shadow mode, on the implement task's exit, commit and push `{skills_dir}` changes through the C7 path on `aif/<alias>/skills-<n>` and open the PR there.
**AC:**
- A TUI test presses `v`, types `web`, and sends `TheoryAction::Setup { repo: "borsuk", surface: "web" }`; `every_action()` gains it.
- A daemon test asserts `create_issue` with the title `Create the run skill for borsuk/web`, both labels, and a body that contains `{skills_dir}` filled and step 6 `Prove it once`.
- A gate test admits a `verify-skill` ticket to refine and implement with no `theory-short` and no `theory-full`.
- A shadow-mode test asserts one commit, one push to `aif/borsuk/skills-`, and one `gh pr create` on the theory repository, and no write on the code repository.
**Depends on:** C7, C8, C16, C20, C32, V1 · **Traces to:** R19, R20

### V10 — Drift from the audit sweep
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C24 (`AUDIT_SWEEP_PROMPT`, `AuditJob::Sweep`, the sweep's `<aif-event-v1>` blocks and their `open_event` posting) and C32 (`create_issue` with labels). Built: no.
**Build:** Add the drift paragraph to `AUDIT_SWEEP_PROMPT` of C24 and a `kind: "skill-drift"` field on the sweep's event block with `surface`. Add `MAINTAIN_BODY` with the eight maintain lines of design §6.3. When the sweep's blocks carry `skill-drift` for a surface with no open issue labelled `verify-skill` whose title names that surface, call `create_skill_ticket(alias, surface, Maintain)`.
**AC:**
- A sweep result with two `skill-drift` blocks for `web` and one open `verify-skill` ticket for `web` creates no ticket; the same with no open ticket creates one titled `Maintain the run skill for borsuk/web` with both labels.
- A sweep result with no `skill-drift` block creates nothing and the C24 events still open.
- The prompt test asserts `AUDIT_SWEEP_PROMPT` contains `dead handles` and `skill-drift`.
**Depends on:** C24, C32, V9 · **Traces to:** R21

### V11 — Teach
**Status:** `[x]` implemented on 2026-09-10 (branch verification-toolbelt)
**v0.7 base:** C17 (`ChatKey::Theory`, `TheoryAction::Chat`, the chat machinery of the bootstrap), C16 (`PurposeSpec` and `wants_final_block`), C14 (`open_event`), and C22 (cards and the `recall` cause the offer hooks into). Built: no.
**Build:** Add `TaskPurpose::Teach(TeachKey)` with a `PurposeSpec` under `theory.chat`, id `<alias>/teach-<pr|delta|area|entry>-<key>`, cwd the repository checkout, and `wants_final_block` returning the event tag. Add `TEACH_PROMPT` per design §7 with `{model}`, `{skills}`, `{subject}` (the diff, or the area's entries), and `{history}` (the `git log --oneline` and `gh pr list` lines the daemon renders for the paths of the subject). Add `TheoryAction::Teach { repo, key }` and key `t` on a merged PR row of the pipeline view, a DELTAS row, and an AREAS row; on the answer of a card with cause `recall`, offer `t` in the inbox row. Open one event per block through `open_event` on the record of the PR, else the repository record.
**AC:**
- A TUI test presses `t` on an AREAS row and sends `TheoryAction::Teach { repo: "borsuk", key: Area("web-checkout") }`; `every_action()` gains it.
- A dispatch test renders `{history}` with two commit lines and one PR line for the subject's paths and `{skills}` with the area's feature file.
- A teach turn that ends with two blocks opens two events; one with no block opens none and the task ends `Done`.
- An inbox test answers a card with `recall` and asserts the row offers `t`.
**Depends on:** C14, C16, C17, C22, V3 · **Traces to:** R22, R23, N2

---

## Definition of Done

- `python3 ~/.claude/skills/writing-specs/scripts/validate_spec.py docs/v0.8/SPEC.md` exits 0 with zero open `[NEEDS CLARIFICATION]` markers.
- Every requirement R1 to R23 and rule N1 to N6 traces to at least one chunk: R1 V0; R2 to R6 and R8 V1; R7 V2; R9 V3; R10, R11 V4; R12 V5; R13 V6; R14 V7; R15 to R18 V8; R19, R20 V9; R21 V10; R22, R23 V11; N1, N2 V1 and V11; N3 V4, V7, V8; N4 V4; N5 V6, V8; N6 every chunk.
- Every chunk names its v0.7 dependencies, and no chunk depends on a chunk listed after it.
- Every acceptance check names an observable result a test can fail on before the chunk exists.
