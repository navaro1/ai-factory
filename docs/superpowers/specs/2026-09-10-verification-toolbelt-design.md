# The Verification Toolbelt

Date: 2026-09-10 · Status: Design record, brainstorm complete, no code · Revision: 3, after the simplification pass and the ticket-precision pass · Scope: The run skill per surface, the driver ladder, the Before / After contract, deterministic fast checks, refine grounding, review re-drive, setup, teach, steering · Sibling: docs/superpowers/specs/2026-09-03-theory-governor-design.md and docs/v0.7/SPEC.md

Sources: Lauren Tan, *How I Use Cursor* (2026-05-25), *Loops You Can Trust* (2026-06-24), *The Complete Guide to pstack, Part 1* (2026-08-31), *The Complete Guide to pstack, Part 2* (2026-09-09). The pstack plugin 0.15.1 and the `cursor-team-kit` skills `control-ui`, `control-cli`, and `verify-this`. Claude Code's bundled `run`, `verify`, and `run-skill-generator` skills and its hooks and subagent docs. The Codex docs on skills, subagents, hooks, and MCP. Anthropic, *Effective harnesses for long-running agents* and the `cwc-long-running-agents` repository. Pulumi's token measurements of `agent-browser` against the Playwright MCP. Piotr's notes of 2026-09-10.

---

## 1. Thesis and identity

ai-factory is an opinionated way to deliver software with agents, from the seat of a principal engineer who stays at the architecture level. The theory governor keeps that engineer at the theory level. This document adds the other half of Lauren Tan's argument:

> Verification is the limiting step. An agent that can verify its own work closes the loop. An agent that cannot makes the human the bottleneck at the diff level.

The governor's trust loop measures numbers. Behaviour that needs a running application, a driven UI, a command with a transcript, or a log line has no path in the governor today. `verify.toml` names a skill and the daemon passes a name. Nothing creates the skill, nothing resolves it, and nothing checks that the review used it.

The toolbelt fills that gap with four things, and it borrows every tool it can instead of building one:

| Thing | One sentence |
|---|---|
| The run skill | One markdown file per surface that says how to launch, drive, check, and read the logs of that surface, with the generic driver it uses. Agent-owned. |
| The Before / After contract | Every agent PR states, per feature, the observed behaviour before and after, with the command that observed it. The daemon checks the shape. |
| Fast checks | The daemon runs each touched feature's fast command before review, through the measure machinery. A fail costs zero reviewer tokens. |
| Teach | One chat purpose that explains a change or an area to the operator, so the operator can predict again. |

The design follows one rule from the research: simpler with the same result means fewer tokens and faster delivery. The first revision had a control CLI per surface, an eight-section skill, a verifier subagent in every run, a maintenance cadence, and twelve principles named in every PR. Each of those had a cheaper equivalent that already exists. This revision uses the equivalents.

The toolbelt is orthogonal to the governor. It changes no label of the governor's state machine. It touches the governor at five seams: the `{skills}` placeholder, the PR body check, the measure task, the `theory.chat` machinery, and the `create_issue` path of the ladder.

Three refusals hold, from the governor:

1. No human reads code to catch a defect.
2. An agent never edits `theory/model.toml`, `theory/verify.toml`, or `theory/rules.md` off a model branch.
3. The factory keeps no journal. GitHub holds every record.

Two refusals join them:

4. A test alone is not verification. A Before / After line that rests on "it compiles", "tests pass", or an author's summary is `inconclusive`, and inconclusive is not a pass.
5. No browser-profile integration, ever. Claude in Chrome and the Codex `@Chrome` act inside a signed-in browser. Headless Playwright through a CLI is the web driver.

---

## 2. Vocabulary

| Term | Meaning |
|---|---|
| Surface | One thing a user touches: a web UI, an admin panel, an HTTP API, a CLI, a TUI, a library. A repository has one or more. |
| Run skill | The file `.claude/skills/run-<surface>/SKILL.md` plus its `features/` directory and optional helper scripts. Agent-owned code, reviewed like any code. |
| Driver | The generic tool that drives a surface. One of five tiers, section 4.3. |
| Tier | The driver's level: `browser`, `dom`, `http`, `terminal`, or `none`. |
| Floor | The lowest tier the operator accepts for an area, set in `verify.toml`. |
| Feature | One user-facing behaviour of one surface, described in `features/<feature>.md`. It binds to one area of `verify.toml`. |
| Fast command | The one command per feature that is the quickest proof it still works. Seconds, not minutes. Exit code is the verdict. |
| Fast check | The daemon's run of a fast command as a measure task, with an exit-code record. |
| Skills checkout | The git checkout that holds the run skills. The theory checkout by default. |
| Acceptance criterion | One falsifiable statement in the refined ticket, with an ID `AC-<n>`, that names the check that proves it. |
| Trace | The chain from one acceptance criterion to the Before / After line that proves it and the test or drive behind that line. |
| Before / After line | One line in `## Before / After`: criterion, feature, tier, command, before, after. |
| State | `pass`, `fail`, or `inconclusive`. Derived from the line and the fast check. |
| Re-drive | The review agent runs every Before / After command again on the head, and for a bug also on the base. |
| Lever | A helper script under the skill that proves one fact and exits non-zero on failure. A reviewer can rerun it. |
| Restate | The first section of a refined ticket: the request in the agent's own words. |
| Teach | The `theory.chat` purpose that explains a merged PR, a delta, or an area to the operator. |
| Setup ticket | The ticket `Create the run skill for <alias>/<surface>`, labels `to-refine` and `verify-skill`. |

---

## 3. Rules

The twelve rules of the governor hold. These join them.

13. An agent verifies on the real surface. A Before / After line states the command and the observed result, or it is inconclusive.
14. A question an experiment can answer is not the operator's. An agent runs the experiment and records the result. Only a product or preference call earns `needs-human`.
15. The agent that judges a change never wrote it. The review agent re-drives every line on a fresh context and another model. The daemon runs the fast checks, and no agent grades its own fast check.
16. A check done by hand twice becomes a lever in the same PR. A lever the reviewer cannot rerun is not a lever.
17. No confirmed reproduction, no authored fix. A bug ticket reproduces twice through the driver before any plan.
18. The skills checkout is the theory checkout, unless a path override says otherwise. A repository that many people share stays clean through shadow mode or the override.
19. Evidence is text. Transcripts, ARIA snapshots, exit codes, and log excerpts travel in the PR body and in comments.
20. An agent probes for drivers and never installs one. A new driver is a ticket the operator opens.
21. The daemon inlines skill content into the prompt. It never relies on a harness skill loader, a harness browser integration, or a plugin.
22. The ticket defines done. Every acceptance criterion is falsifiable and names its check. Every Before / After line names the criterion it proves. A change that no criterion asks for does not ship.
23. The simplest change that meets every criterion ships, in the style the repository already has. No new abstraction, layer, or dependency without a criterion that needs it. A test that still passes with the change reverted is not a test.

---

## 4. The run skill

### 4.1 Layout

One directory per surface, in the shape Claude Code's `run-skill-generator` writes, so that generator can seed it and a human can `/run` it. A repository with an admin panel, an API, and a web application has three.

```
.claude/skills/
  run-admin/
    SKILL.md                  the surface skill, section 4.2
    features/README.md        the feature index
    features/user-roles.md    one file per feature that needs a drive recipe
    wait_for.sh               a helper, only when a step needs one
  run-api/
    SKILL.md
    features/README.md
  run-web/
    SKILL.md
    features/README.md
    features/checkout.md
```

The path carries a vendor name. That is the price of a free generator and a free `/run`. The daemon inlines the content, so Codex and OpenCode read the same file. A human on Codex can symlink `.agents/skills/run-<surface>` to it.

### 4.2 The skill file

The front matter names the surface, the driver, and the tier. The body has the four project-specific items of Claude Code's `run` skill plus two lines. Everything generic stays out, because the driver's own help text carries it.

```
---
name: run-web
description: Launch and drive the web app for verification.
surface: web
driver: playwright-cli
tier: browser
blind: pixels below 320 px width, native file dialogs
---
```

| Section | Must contain |
|---|---|
| Run | The dev command, the port, the ready signal, and the stop command. Background launch, poll the port, kill by port, never `pkill -f`. For a TUI: the `tmux new-session` line and the ready marker. |
| Fast | The priority test command for this surface and its expected wall time. |
| Auth or seed | Whatever gets a usable session: a cookie line, a login sequence, a seed script. |
| Drive | One representative interaction through the driver, ending in an observable state. Stable handles: ARIA roles and names, routes, prompt strings, key names. |
| Logs | Where logs go and the grep that finds one request or one command. |
| Gotchas | Only the ones the author hit. |

A placeholder left in any section fails the setup ticket.

### 4.3 The driver ladder

The setup ticket probes the toolchain and the machine, picks the highest tier it finds, and records it in the front matter. An agent never installs a driver.

| Tier | Driver | Proves | Needs |
|---|---|---|---|
| `browser` | `playwright-cli`, `agent-browser`, or `chromium-cli`, headless | Rendered UI, clicks, ARIA snapshots | Playwright in the toolchain. No extension, no browser profile. |
| `dom` | The repo's `jsdom` or `happy-dom` runner, or fetch plus an HTML parser | Rendered markup and component behaviour | Node or Python already present. |
| `http` | `curl`, an HTTP or GraphQL client | Status, headers, body, side effects in logs and data | Nothing new. |
| `terminal` | `tmux`, `agent-tty`, direct invocation | A CLI or TUI end to end | `tmux`. |
| `none` | No driver reaches the feature | Nothing. Every line is `inconclusive` with the reason. | Nothing. |

A CLI driver costs a few characters per action. The Playwright MCP costs the full accessibility tree per action. The ladder names CLIs only.

The operator sets a floor per area in `verify.toml`: `min_tier = "browser"`. A line below the floor is `inconclusive`. The body check holds the PR and opens a theory event through `open_event`. The operator accepts the lower tier or stops the line. No silent downgrade.

### 4.4 The feature file

Front matter, one H1, one paragraph, then the four H2s of the pstack feature-map example: `Sub-features`, `How to get to it (user POV)`, `Driving it`, `Gotchas`. The index lists every feature in one line each. A feature gets its own file only when its drive needs more than one command.

```
---
area: web-checkout
fast: npx playwright test checkout --reporter=line
---
```

`fast` finishes in seconds and exits non-zero on failure. The file names user paths, stable handles, required state, commands, and observable proof. It names no function and no file of the implementation, for the same reason rule 5 of the governor gives for the model.

### 4.5 Location and override

The skills checkout is the theory checkout: `TheoryConfig::checkout(repo_path)`. In in-repository mode that is the code repository. In shadow mode that is the private theory repository, and the code repository never sees a skill file, a label, or a comment. A per-repository override `skills = { path = "..." }` points anywhere. A path without a remote keeps commits local. This is how the operator experiments against a repository that hundreds of people share.

The daemon renders absolute paths into every prompt. Helper scripts run inside the code worktree and find the application there.

### 4.6 Slicing

The daemon inlines files, not directories. The cap is two surfaces and six feature files per prompt. Past the cap it inlines the index only and names the paths.

| Stage | Areas | Inlined into `{skills}` |
|---|---|---|
| Refine | Short prediction's areas | The index and the Run and Fast sections of each surface. For a bug ticket, the Drive and Logs sections too. |
| Implement | Full prediction's areas | The whole skill file of each named surface, and the feature files that bind to those areas. |
| Review | The areas of `git diff --name-only <base>...<head>` | The same rule over the diff areas alone. Spec R9 narrows this row and wins over the table. |
| Teach | The areas of the PR, delta, or area | The feature files of those areas, index only past the cap. |

This changes design record §5.1 and spec C28, which pass a name. Section 9 lists the edit.

---

## 5. The run

### 5.1 The Before / After contract

Every agent PR has this body and nothing else. It is a briefing, not a lab notebook, in Lauren Tan's opening-a-pr shape.

```
## Why
One or two short paragraphs. The behaviour that changes and for whom.

## Before / After
- AC-1 · checkout-submit · browser · `npx playwright test checkout` · before: an empty card is accepted and the API returns 500 · after: the field shows "Card is required" and no request is sent
- AC-2 · api-orders · http · `curl -s -X POST :4000/orders -d @empty.json` · before: 500, log `NullPointer at Orders.create` · after: 422 `{"error":"card_required"}`
- AC-3 · poll_p95 · measure · `aif measure` · 12 → 11 ms

## Blast radius
One to three sentences. What else the change touches and why it is safe.
```

One line per acceptance criterion, more when one criterion needs two surfaces. Each line names the criterion, the feature, the tier, the command, and the observed state before and after. A `measure` line comes from the daemon's own Before and After comment of C28, so the number is a factory number. For a bug, before is the repro on base and after is the same command on head. A transcript longer than the cap goes under its line in a fenced block, cut to the cap, and the full file stays at `.aif/evidence/` in the author's worktree. Screenshots wait for `gh --attach` to reach stable.

Writing rules, from `technical-writing` and `unslop`, as one paragraph in the prompt: short declarative sentences, one thought per sentence, active voice, no long dash, no curly quote, no mid-sentence colon, no `## Summary`, no `## Test plan`, no narration of the work, body under 40 lines before the fenced blocks.

### 5.2 The body check

The deterministic check of C11 gains these lines. It costs zero agent tokens and covers every harness.

| Line | Rule |
|---|---|
| Sections | `## Why` and `## Before / After` present. `## How`, a heading that starts with `Implementation`, `## Summary`, and `## Test plan` absent. |
| Trace | Every `AC-<n>` of the linked ticket appears in at least one line. Every line names an `AC-<n>` that exists in the ticket. |
| Coverage | Every touched area with a run skill has at least one line. Every line names a feature in the index or a measurer in `verify.toml`. |
| Tier | Every line's tier is at or above the area's floor. |
| State | No line is `inconclusive`. |
| Scope | Every changed path is under an owned path of the plan table, under the run skill, or is a test file. A dependency manifest changes only when the ticket names the dependency. |
| Prose | No long dash, no curly quote, no mid-sentence colon outside code, body under the line cap. pstack's `check-plan.mjs` holds the same three lint rules. |

A failure posts one finding comment and re-queues implement, as C11 does today. A floor failure also opens a theory event. The trace and scope lines are the structural form of rules 22 and 23: a change without a criterion, or a path outside the plan, never reaches a reviewer.

### 5.3 Fast checks

At review admission, next to the base and head measurements of C28, the daemon queues one measure task per touched feature that names a `fast` command. The task runs the command in the head worktree through the script runner and records `{ id: <feature>, value: <exit code>, unit: "exit", direction: "lower" }`. A non-zero exit is a `fail`, and the review does not dispatch. The daemon posts the finding and re-queues implement. No reviewer tokens are spent on work whose own fast path fails.

This is Build the Lever at its cheapest. The agent names the lever in the feature file. The factory pulls it.

### 5.4 Refine

Refine moves from the repository checkout to the issue worktree, so an experiment never touches the operator's checkout. Four steps come before the plan table. The refine agent does them itself. Subagents stay allowed for sizeable research, as today, and are not required.

| Step | The agent | Writes into the ticket |
|---|---|---|
| Restate | Rewrites the request in its own words before it reads code. This is the indirect prompt of pstack Part 2. | `## Problem` opens with the restatement, one paragraph. |
| Ground | Reads the affected code and `git log` and `gh pr list` for the paths it touches. | `## Grounding`: the mechanism, the history, the paths, with citations. |
| Prototype before ask | Classifies each open question. A question an experiment can answer is not the operator's. The agent runs the experiment in a scratch directory under the worktree, never committed, and records the result. Only a product or preference call goes to `needs-human`. | `## Decisions`: one line per question, the answer, the command. |
| Repro twice, bug tickets only | Drives the surface to reproduce the defect twice on the base. A third miss goes to `needs-human` with the attempts. | `## Repro`: the exact command, two observed outputs, the exit code. |

The acceptance criteria change shape. Each criterion is one falsifiable line with an ID, in the form of `verify-this`: the condition, the observable result, and the check that proves it.

```
## Acceptance criteria
- AC-1 · An empty card field blocks submit and shows "Card is required" · check: checkout-submit drive
- AC-2 · POST /orders with no card returns 422 and `{"error":"card_required"}` · check: api-orders fast
- AC-3 · poll_p95 does not worsen · check: measure poll_p95
```

A criterion that no command can falsify is not a criterion. The refine agent rewrites it or asks, under rule 14, only when no experiment can settle it. A criterion that the ticket text does not ask for is scope creep, and the refine agent drops it.

The plan table gains one column, `Fast`, next to `Validation`. A chunk names the fast command that proves it, or `new: <feature>` when the chunk must add a feature file. Every owned path in the table is a real path or glob, because the body check of 5.2 reads it.

Before implement dispatches, the daemon checks the refined ticket the way it checks a PR: the four sections present, every criterion carries an `AC-<n>` and a `check:`, every check names a feature in the index, a fast command, or a measurer, and the plan table parses. A failure re-queues refine with the finding. This is pstack's `check-plan.mjs` moved into the daemon, and it costs zero agent tokens.

### 5.5 Implement

The coordinator and its author subagents work as today. Three rules join the prompt.

The simplest change. The coordinator reads the conventions of the files it touches before the first edit and follows them. It makes the smallest change that meets every criterion. It adds no abstraction, layer, flag, or dependency that no criterion needs. It deletes dead weight it meets in its own commit. Before each commit it removes narrating comments, guards no criterion asks for, and edits outside the plan. This is pstack's Laziness Protocol, Subtract Before You Add, and `deslop`, as one paragraph.

The coordinator drives every touched feature once through the driver before it opens the PR and writes the Before / After line from what it observed, with the criterion it proves. It runs every fast command and pastes the exit code. It writes no line it did not observe. Every test it adds asserts a literal result through the public path and fails with the change reverted.

The lever rule. When an agent checks the same fact by hand twice, or writes a throwaway script to check it, it adds the script to the run skill in the same PR, with one invocation line in `SKILL.md`. A one-off `grep`, a shell history line, or a test that passes when every dependency returns nothing is not a lever.

No verifier subagent runs by default. The fast checks of 5.3 and the re-drive of 5.6 give the same separation at lower cost. A `very-high` tag route may add a verifier subagent through the harness's own mechanism: `--agents` JSON for Claude Code, a role file for Codex, on the review route's model. That is a route setting, not a prompt rule.

### 5.6 Review

The review agent runs on another model in a fresh context. It trusts no Before / After line until it re-drives it.

1. Run every line's command on the head. Compare the observed state to the stated after.
2. For a `bug` ticket, run the `## Repro` command in the base worktree of C28 and expect the stated before. Then run it on the head and expect the after. Red on base, green on head.
3. Run every lever the PR adds and get the stated exit code.
4. Run every test the PR adds against the base worktree with only the test files applied. Each must fail there. A test that passes on base tests nothing, and the reviewer deletes it or rewrites it.
5. Read the diff once for what it does not need. An abstraction, a flag, a guard, or a dependency that no criterion asks for is a finding, and the reviewer removes it. A deviation from the conventions of the surrounding files is a finding, and the reviewer aligns it.
6. Post the reviewer's own Before / After lines as a PR comment, in the same shape.

A mismatch is a finding. The reviewer repairs the code, or repairs the check when the check was the defect, then re-drives the full list. A bug that does not fail on base is a wrong root cause, and that finding goes to `needs-human` with both outputs. The two outcomes of the review contract stay: `gh pr ready` when every line passes on the reviewer's run, `needs-human` otherwise.

The reviewer also repairs run-skill drift it meets, as it repairs any finding. That is the daily maintenance Lauren describes, done by the agent that is already there.

---

## 6. Setup and maintenance

### 6.1 The doctor

`aif doctor` prints one line per configured repository and surface: `run skill <alias>/<surface>: <tier>`, `missing`, or `lint: <feature file>: area <id> unknown`. It warns when an area's floor is above the tier its surface reaches, and when the implement route and the review route of one complexity level resolve to the same model family.

### 6.2 The setup ticket

Key `v` on a repository row of the Theory view creates the ticket `Create the run skill for <alias>/<surface>` with labels `to-refine` and `verify-skill`. The daemon asks for the surface name inline. The body tells the agent to:

1. Interview the repository, not the operator: the surface, the run command, the drivers present, the evidence, the isolation.
2. Probe the driver ladder. Pick the highest tier present. Install nothing.
3. Fix a checkout that does not start, or report it precisely, before any generation.
4. Write `SKILL.md` to section 4.2. Measure the Fast path and record its time. Under Claude Code, `run-skill-generator` writes the first draft.
5. Seed `features/` from `verify.toml`: the index, and one file per area that maps to this surface and needs a drive recipe, the top three to five.
6. Prove it once end to end: run, fast, drive one feature, read the logs, stop. Confirm the evidence file still exists.
7. Write the PR to the contract of 5.1. The Before / After line of a setup PR states `before: no run skill` and the after is the proof of step 6.
8. Propose the `skills` entry for `verify.toml` as text in the PR. Never edit `verify.toml`.

A `verify-skill` ticket and its PR skip both prediction gates, like `model-pr`. They change no behaviour of the application. The body check and the review still run.

### 6.3 Maintenance

No new cadence. Two existing paths carry it. The v0.7 audit cadence on `sweep.days` also fires the sweep now, and `aif doctor --audit <alias>` fires it on demand.

The reviewer repairs run-skill drift it meets during a re-drive, per 5.6. The weekly audit sweep of C24 gains one paragraph: check each run skill's Run and Fast sections and each feature file's handles against the code, report dead paths and dead handles. When the sweep finds drift, the daemon opens one ticket `Maintain the run skill for <alias>/<surface>` with `to-refine` and `verify-skill` through the `create_issue` path of C32, and skips a surface that already has one open. Its body is the pstack maintain recipe in eight lines: index hygiene, source pass, live pass, triage into doc drift or harness gap or product gap, a `bug` ticket per product gap, re-drive every fix, one PR or one comment, final stop.

---

## 7. Understanding: teach

The governor makes the operator explain to the agent and tests the operator. Lauren's Part 2 goes the other way: the agent explains, and the human then predicts. Teach is that path.

`TaskPurpose::Teach` runs under `theory.chat`. Key `t` on a merged PR row, a DELTAS row, or an AREAS row starts it. The prompt tells the agent to explain what changed and why, from the diff, the git and PR history, the feature index, and the model slice. It gives the smallest complete answer first, then adds layers on request. It builds a picture in steps, one part at a time. It keeps the confidence language of what it found in history. It prints no framing labels and no quiz.

Teach ends with one `<aif-event-v1>` block per contradiction it finds between the model and the code, else with no block. A contradiction opens a theory event through `open_event`. The inbox offers `t` after a card miss with cause `recall`.

Teach adds no claim to the model. The operator predicts again on the next ticket, and the delta measures whether the teaching held.

---

## 8. Steering and principles

The operator steers without a diff.

| Handle | Changes | Where |
|---|---|---|
| Ticket text | Scope, acceptance criteria, the restatement the agent must match. | The GitHub issue. |
| Complexity labels | The model of implement and review per ticket, and the optional verifier route. | `complexity:*`, `review-complexity:*`. |
| The model | The theory every agent reads as its slice. | `theory/model.toml`. |
| `verify.toml` | Policies, floors, skills per area. | The area chat. |
| The run skill | How agents launch, drive, and prove each surface. | Setup tickets, or a direct PR. |
| `rules.md` | One rule every prompt carries. | A theory event answer with rung 3. |
| Prompt edits | The stage wording. | The Settings view. |

The stance carries six principles as vocabulary, each adapted from pstack by Lauren Tan, and each backed by a structural check rather than a naming rule. An agent names none of them in a PR.

| Principle | The structure that enforces it |
|---|---|
| Prove It Works | The Before / After lines and the body check. |
| Build the Lever | The fast command per feature and the daemon's fast check. |
| Never Block on the Human | Prototype before ask in refine. |
| Fix Root Causes | Red on base, green on head in review. |
| Laziness Protocol | The trace and scope lines of the body check, and the reviewer's pass for what the diff does not need. |
| Test Behavior, Not Implementation | Every added test runs against base in review and must fail there. |

---

## 9. Changes to the governor documents

| Document | Today | Change |
|---|---|---|
| Design record §5.1, spec C28 | The map names a skill by name. The factory passes only the name. | The factory inlines the skill slice of 4.6 into `{skills}`. |
| Spec C11, R27 | The check requires `## Why` and forbids `## How`. | The check applies the table of 5.2, and a sibling check runs on the refined ticket before implement. |
| Spec C28 | Base and head measurements at review admission. | Fast checks of 5.3 join them. |
| Spec R23 | The area schema. | `min_tier` joins it. |
| Spec §2 and the refine cwd | Refine runs in the repository checkout. | Refine runs in the issue worktree. |
| Spec C24 | The sweep checks entries against the code. | One paragraph on run-skill drift, and the maintain ticket. |

Nothing else in C0 to C32 changes.

---

## 10. Pipeline changes

| Stage or task | Change |
|---|---|
| refine | Issue worktree. Restate, Ground, Decisions, Repro. Criteria with IDs and checks. The Fast column. Reads the refine slice. |
| ticket check | Deterministic, before implement dispatches. Sections, criteria, checks, plan table. A failure re-queues refine. |
| implement | The simplest change in the repository's style. Drives once, writes Before / After per criterion, runs the fast commands, adds levers. The body contract and the writing paragraph. |
| body check | The table of 5.2, with trace and scope. |
| fast checks | Measure tasks per touched feature at review admission. A fail re-queues implement. |
| review | Re-drives every line. Red on base, green on head for a bug. Every added test red on base. Runs levers. Removes what no criterion needs. Posts its own lines. Repairs drift. |
| release | Unchanged. |
| `verify-skill` tickets | Skip both prediction gates. |
| audit sweep | One paragraph, one ticket on drift. |
| `teach` | New `TaskPurpose` under `theory.chat`. |
| doctor | Tier per surface, lint, floor warning, family warning. |

New labels: `verify-skill`. New marker blocks: none. New measurement unit: `exit`.

---

## 11. Configuration sketch

```toml
[repo.borsuk]
path = "/home/you/Workplace/borsuk"
theory = { repo = "navaro1/borsuk-theory", path = "/home/you/Workplace/borsuk-theory" }
skills = { path = "/home/you/Workplace/borsuk-verify" }   # optional, default: the theory checkout
```

```toml
# theory/verify.toml
[[area]]
id = "web-checkout"
boundary = "B-checkout"
min_tier = "browser"          # optional, default: none
skills = ["run-web"]
```

The exact field names belong to the spec.

---

## 12. The terminal UI

| Surface | Addition |
|---|---|
| Theory view, AREAS panel | The tier per area: `browser`, `http`, `-` none, `!` lint or below floor. Key `v` creates a setup ticket. Key `t` teaches an area. |
| Theory view, DELTAS panel | Key `t` teaches the PR of a delta. |
| Pipeline view | A held implement row shows `fast check failed` with the feature. |
| Inbox | A card miss with cause `recall` offers `t`. A floor failure appears as a theory event. |
| Settings | The `skills.path` field per repository. |
| Doctor | The lines of 6.1. |

---

## 13. Build order

| Chunk | Content | Depends on |
|---|---|---|
| V0 The stance addendum | Rules 13 to 21, refusals 4 and 5, the vocabulary, the four principles, the steering table, the driver ladder. | C3 |
| V1 Skill resolution and the doctor | The `run-<surface>` parser, the feature front matter, the resolution order of 4.1, the lint, `TheoryConfig.skills`, `min_tier` in the area schema, the doctor lines, the AREAS tier mark. | C25 |
| V2 The setup ticket | Key `v`, the surface prompt, `create_issue` with `to-refine` and `verify-skill`, the body of 6.2, the gate skip for `verify-skill`. | C16, C32, V1 |
| V3 Slicing and fast checks | The slice rule of 4.6 for the four stages. The cap. Fast checks as measure tasks at review admission, the `exit` unit, the re-queue on fail. Replaces the name fill of C28. | C10, C28, V1 |
| V4 The contract and the prompts | The body check table of 5.2 in `check_pr`, with trace and scope. The refined-ticket check before implement dispatch. The refine cwd. The refine, implement, and review prompts rewritten once and pinned. The audit sweep paragraph and the maintain ticket. | C11, C24, V3 |
| V5 Teach | `TaskPurpose::Teach`, the prompt, keys `t`, the recall offer, events through `open_event`. | C17, V3 |

V0 can start now. V1 to V3 wait for the trust-loop parsers. V4 rewrites the three prompts once. V5 comes last.

---

## 14. Follow-ups outside this design

- Screenshots and video in the PR, when `gh --attach` reaches stable.
- A browser only in CI: run the repo's end-to-end workflow on the PR branch and read the run log through `gh run view`. One more GitHub call per review.
- A verifier subagent in every run, if the fast checks plus re-drive prove too weak.
- A multi-model review panel with an agreement map.
- A blinded eval of a prompt change, in the style of the pstack eval playbook.
- Harness Stop hooks as a second gate, if the body check is ever bypassed.

---

## 15. Open items for the spec

- The exact TOML fields of `skills` and `min_tier`, and the front-matter keys of the skill and feature files.
- The line cap of the body and the per-line transcript cap.
- The surface prompt of key `v`: inline text, or a pick from `verify.toml` boundaries.
- Whether a `verify-skill` PR in shadow mode rides the model branch of C7 or its own branch.
- The `very-high` verifier route: the exact `--agents` JSON for Claude Code and the role file for Codex, and the OpenCode fallback.
