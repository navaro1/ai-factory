# The Verification Toolbelt

Date: 2026-09-10 · Status: Design record, brainstorm complete, no code · Scope: The verification skill per surface, the verifier and the lever inside a run, refine grounding, review re-drive, setup and maintenance, teach, steering · Sibling: docs/superpowers/specs/2026-09-03-theory-governor-design.md and docs/v0.7/SPEC.md

Sources: Lauren Tan, *How I Use Cursor* (2026-05-25), *Loops You Can Trust* (2026-06-24), *The Complete Guide to pstack, Part 1* (2026-08-31), *The Complete Guide to pstack, Part 2* (2026-09-09). The pstack plugin, version 0.15.1, in particular `create-verification-skill`, `maintain-verification-skill`, the feature-map example, the principle skills, and the feature, bug-fix, prototype, and opening-a-pr playbooks. Piotr's notes of 2026-09-10. Andy Grove, *High Output Management*, through Lauren's reading of it.

---

## 1. Thesis and identity

ai-factory is an opinionated way to deliver software with agents, from the seat of a principal engineer who stays at the architecture level. The theory governor keeps that engineer at the theory level. This document adds the other half of Lauren Tan's argument:

> Verification is the limiting step. An agent that can verify its own work closes the loop. An agent that cannot makes the human the bottleneck at the diff level.

The governor's trust loop measures numbers. A measurer is a script that emits values. Behaviour that needs a running application, a driven UI, a command with a transcript, or a log line has no path in the governor today. `verify.toml` names a skill and the daemon passes a name. Nothing creates the skill, nothing resolves it, nothing maintains it, and nothing checks that the review used it.

The toolbelt fills that gap with four things:

| Thing | One sentence |
|---|---|
| The verification skill | One directory per surface of the application, with a control CLI, a skill document, and a feature map. Agent-owned code. |
| The verifier | One subagent in every implement run that did not write the code, drives the skill, and writes the evidence. |
| The lever | A rule: a check done by hand twice becomes a tool in the skill, in the same PR. |
| Teach | One chat purpose that explains a change or an area to the operator, from code and history, so the operator can predict again. |

The toolbelt is orthogonal to the governor. It changes no label of the governor's state machine. It touches the governor at five seams: the `{skills}` placeholder, the `## Evidence` section and its body check, the `theory.chat` machinery, the cadence list, and the `create_issue` path of the ladder.

Three refusals hold, from the governor:

1. No human reads code to catch a defect.
2. An agent never edits `theory/model.toml`, `theory/verify.toml`, or `theory/rules.md` off a model branch.
3. The factory keeps no journal. GitHub holds every record.

One refusal joins them:

4. A test alone is not verification. An evidence item that rests on "it compiles", "tests pass", or an author's own summary is inconclusive, and inconclusive is not a pass.

---

## 2. Vocabulary

| Term | Meaning |
|---|---|
| Surface | One thing a user touches: a web UI, an admin panel, an HTTP API, a CLI, a TUI, a library. A repository has one or more. |
| Verification skill | The directory `skills/verify/<surface>/` with `SKILL.md`, `bin/`, and `features/`. Agent-owned code, reviewed like any code. |
| Control CLI | The executable `skills/verify/<surface>/bin/control-<surface>`. It launches, checks, drives, snapshots, reads logs, tests, and tears down the surface. |
| Feature | One user-facing behaviour of one surface, described in `features/<feature>.md`. It binds to one area of `verify.toml`. |
| Feature map | The `features/README.md` index plus the feature files of one surface. pstack calls it materialized memory. |
| Fast path | The one command per feature that is the quickest proof it still works. Seconds, not minutes. |
| Skills checkout | The git checkout that holds `skills/verify/`. The theory checkout by default. |
| Evidence item | One line in `## Evidence`: feature ID, command, head SHA, observed result, state. |
| State | `pass`, `fail`, or `inconclusive`. |
| Verifier | The subagent of an implement run that writes every evidence item. It never edits product code. |
| Re-drive | The review agent runs every evidence item again on the head, and for a bug also on the base. |
| Lever | A tool under `bin/` that proves one fact and exits non-zero on failure. A reviewer can rerun it. |
| Restate | The first section of a refined ticket: the request in the agent's own words. |
| Grounding | The `## Grounding` section of a refined ticket: how the code works and why it is shaped so, from code and history. |
| Teach | The `theory.chat` purpose that explains a merged PR, a delta, or an area to the operator. |
| Setup ticket | The ticket `Create the verification skill for <alias>/<surface>`, labels `to-refine` and `verify-skill`. |
| Maintain ticket | The ticket `Maintain the verification skill for <alias>/<surface>`, same labels, opened by a cadence. |

---

## 3. Rules

The twelve rules of the governor hold. These join them.

13. An agent verifies on the real surface. A test alone is not verification. An evidence item states the command, the head SHA, and the observed result, or it is inconclusive.
14. A question an experiment can answer is not the operator's. An agent runs the experiment and records the result. Only a product or preference call earns `needs-human`.
15. The agent that judges a change never wrote it. Inside a run, the verifier writes the evidence and the authors do not. Across stages, the review agent re-drives every item.
16. A check done by hand twice becomes a lever in the same PR. A lever the reviewer cannot rerun is not a lever.
17. No confirmed reproduction, no authored fix. A bug ticket reproduces twice through the control CLI before any plan.
18. The skills checkout is the theory checkout, unless a path override says otherwise. A repository that many people share stays clean of experiments through shadow mode or the override.
19. Evidence is text. Transcripts, ARIA snapshots, exit codes, and log excerpts travel in the PR body and in comments. A screenshot without the command that produced it is not evidence.
20. The daemon inlines skill content into the prompt. It never relies on a harness skill loader, a harness browser integration, or a plugin.

---

## 4. The verification skill

### 4.1 Layout

One directory per surface. A repository with an admin panel, an API, and a web application has three.

```
skills/verify/
  README.md                    index of surfaces, one line each
  admin/
    SKILL.md                   the surface skill, sections in 4.2
    bin/control-admin          the control CLI, contract in 4.4
    features/README.md         feature index and baseline preconditions
    features/user-roles.md     one file per feature, contract in 4.3
    features/audit-log.md
  api/
    SKILL.md
    bin/control-api
    features/README.md
    features/auth-tokens.md
  web/
    SKILL.md
    bin/control-web
    features/README.md
    features/checkout.md
    features/search.md
```

A feature file binds to one `[[area]].id` of `verify.toml` through its front matter. The daemon resolves the skills of an area in this order: the `skills` list on the area, then every feature file whose `area` equals the area ID. The operator's list wins on conflict. A feature file whose `area` is not in `verify.toml` is a lint failure of the skills checkout, reported by the doctor, never a model error.

### 4.2 The surface skill

`SKILL.md` has eight sections in this fixed order. Each is grounded in the repository. A placeholder left in any section fails the setup ticket.

| Section | Must contain | Why |
|---|---|---|
| Fast | The priority test command for this surface and the fastest partial spin-up, with the expected wall time and what the partial spin-up does not run. | The agent needs a proof in under a minute before it pays for a full launch. |
| Launch | The exact start command, the ready signal, and the teardown. For a CLI or TUI: build once, then one PTY session per drive. | Wrong launch steps teach every later agent the wrong thing. |
| Doctor | One read-only check: process up, right build hash, port owned by this run, auth valid. Its JSON shape and exit codes. | Doctor runs before every drive and after every failed drive. |
| Drive | The recipe with real handles from this repository: ARIA roles and names, routes, prompt strings, subcommands. Never coordinates. | Stable handles survive UI churn. |
| Logs | Where logs go, the grep that finds one request or one command, one excerpt of a healthy log, the lines that mean failure. | Logs are the only view into why. They are text, so they travel. |
| Evidence | What to capture and the proof standard: the real user path, the action and the resulting state, side effects checked, no test-only endpoints. Text first. | Adapted from pstack for rule 19. |
| Cleanup | How to stop what this run started, by handle from the session file. Evidence survives. | A stranded port breaks the next run. |
| Helpers | Each script in `bin/`, its invocation, one example. | A script the reader must reverse-engineer is not a helper. |

### 4.3 The feature file

Front matter, one H1, one paragraph, then the four H2s of the pstack feature-map example: `Sub-features`, `How to get to it (user POV)`, `Driving it with <control CLI>`, `Gotchas`.

```
---
area: web-checkout
surface: web
fast: control-web test --tag checkout
---
```

`fast` is one command. It finishes in seconds and exits non-zero on failure. The agent runs it first and pastes the result. The `Driving it` section opens with `Preconditions:` and pairs each user action with one exact command and one observable result. The file names user paths, stable handles, required state, commands, and observable proof. It names no function and no file of the implementation. That rule mirrors rule 5 of the governor for the model, for the same reason: the map must survive a refactor.

### 4.4 The control CLI

The contract, adapted from pstack and the benny control adapter:

| Property | Meaning |
|---|---|
| Composable subcommands | `up`, `doctor`, `drive`, `snapshot`, `logs`, `test`, `down`. Each does one thing. Output pipes. |
| `--dry-run` on destructive commands | `down`, `reset`, and `seed` print what they would do and exit 0. |
| JSON output | `--json` on every command. Agents assert on JSON. Humans read the default text. |
| Rich `--help` | Every subcommand shows one example. The help text is the second copy of the skill. |
| Error text says what to do instead | `port 4173 owned by pid 812 (not ours). Run: control-web down --session <id>` |
| Doctor before drive | `drive` refuses when no doctor passed in this session. `--force` exists and is logged. |
| Kill only what you started | `up` writes a session file with pids and ports. `down` reads it. Never `pkill`. |
| Evidence survives cleanup | Evidence goes to `$AIF_EVIDENCE_DIR`, default `.aif/evidence/<session>/` in the worktree. `down` never touches it. |
| No harness features | Browser driving uses CDP or Playwright inside the CLI. Never the Chrome integration of one harness. |

A minimal command set for a web surface:

| Command | Example |
|---|---|
| up | `control-web up --only checkout --seed minimal --json` |
| doctor | `control-web doctor --session s1 --json` |
| drive | `control-web drive click --role button --name "Place order"` |
| snapshot | `control-web snapshot --aria --path $AIF_EVIDENCE_DIR/checkout/confirm.aria.txt` |
| logs | `control-web logs --since up --grep "order_id=" --tail 40` |
| test | `control-web test --tag checkout` |
| down | `control-web down --session s1 --dry-run` |

A minimal command set for a CLI or TUI surface:

| Command | Example |
|---|---|
| build | `control-cli build` |
| doctor | `control-cli doctor --json` |
| run | `control-cli run -- app search "quarterly" --format json` |
| tui | `control-cli tui start --cols 120 --rows 40`, `tui send "/"`, `tui expect "Search"`, `tui transcript --path $AIF_EVIDENCE_DIR/search/tui.txt` |
| logs | `control-cli logs --session t1 --grep ERROR` |
| test | `control-cli test --tag search` |
| down | `control-cli tui stop --session t1` |

### 4.5 Location and override

The skills checkout is the theory checkout: `TheoryConfig::checkout(repo_path)`. In in-repository mode that is the code repository, and `skills/verify/` ships with the code. In shadow mode that is the private theory repository, and the code repository never sees a skill file, a label, or a comment. A per-repository override `skills = { path = "..." }` points anywhere. A path without a remote keeps commits local. This is how the operator experiments against a repository that hundreds of people share.

The daemon renders absolute paths into every prompt and sets `AIF_SKILLS_DIR` in the environment of every task. The control CLI runs inside the code worktree and finds the application there. It finds itself through `AIF_SKILLS_DIR`.

A setup or maintain PR lands in the checkout that holds the path. In shadow mode that is the theory repository, through the model-branch path of C7. In in-repository mode that is a normal PR.

### 4.6 Slicing

The daemon inlines files, not directories. The cap is two surfaces and six feature files per prompt. Past the cap it inlines the feature index only and names the paths.

| Stage | Areas | Inlined into `{skills}` |
|---|---|---|
| Refine | Short prediction's areas | The feature index of each surface and the Fast section. For a bug ticket, the Drive and Logs sections too. |
| Implement | Full prediction's areas | The feature files that bind to those areas. The Fast, Doctor, Drive, Logs, and Evidence sections of each named surface. Launch and Cleanup once per surface. Helpers dropped, `--help` replaces it. |
| Review | Implement slice plus areas of `git diff --name-only <base>...<head>` | The same rule over the wider set, plus `features/README.md` of each surface for the proof standard. |
| Teach | The areas of the PR, delta, or area | The feature files of those areas, index only past the cap. |

This changes design record §5.1 and spec C28, which pass a name. Section 9 lists the edit.

---

## 5. The run

### 5.1 Refine

Four steps come before the plan table. Refine moves from the repository checkout to the issue worktree, so an experiment never touches the operator's checkout.

| Step | The agent | Writes into the ticket |
|---|---|---|
| Restate | Rewrites the request in its own words before it reads code. This is the indirect prompt of pstack Part 2. It exposes a wrong fixation early. | `## Problem` opens with the restatement, one paragraph. |
| Ground | Sends at most three read-only subagents. One traces how the affected code works. One reads `git log`, `gh pr list`, and linked issues for why it is shaped so. No subagent writes. | `## Grounding`: the mechanism, the history, the paths it touches, with citations. |
| Prototype before ask | Classifies each open question. A question an experiment can answer is not the operator's. The agent runs the experiment in a scratch directory under the worktree, never committed, and records the result. Only a product or preference call goes to `needs-human`. | `## Decisions`: one line per question, the answer, the command that answered it. |
| Repro twice, bug tickets only | Drives the control CLI to reproduce the defect twice on the base. A defect that does not reproduce twice is not planned. The agent tightens conditions or instruments until it fires. A third miss goes to `needs-human` with the attempts. | `## Repro`: the exact command, two observed outputs, the exit code. |

The plan table gains one column, `Lever`, next to `Validation`. A chunk names the `bin/` command that proves it, or `new: <name>` when the chunk must add one.

### 5.2 Implement

One run has one coordinator, at most three author subagents per wave, and one verifier after the last wave.

| Role | May | May not | Receives |
|---|---|---|---|
| Coordinator | Plan waves, own shared files, integrate, commit, run `gh`, repair, open the draft PR, write `## Why`. | Write an evidence item from its own run. Skip the verifier. | Ticket, `{model}`, `{rules}`, `{skills}`, the worktree. |
| Author, at most three per wave, disjoint owned paths | Edit owned paths, run focused validation, add a lever under `bin/` when its owned paths include it. | Git or `gh` writes. Start a subagent. Edit another chunk's paths. Write evidence. | Chunk goal, owned paths, acceptance criteria, validation command, lever column. |
| Verifier, one per run | Read the skill, drive the control CLI, run every fast path and every claimed lever, call `aif measure`, write `## Evidence`. | Edit product code or tests. Git or `gh` writes. Accept an author's summary as proof. | Acceptance criteria, the skill slice, the head SHA, the list of claimed levers. |

The verifier writes one evidence item per acceptance criterion. Each item names the feature ID, the command, the head SHA, the observed result, and the state. Inconclusive means the verifier could not reach the surface, the control CLI errored, or the result rested on a proxy.

A run with any `fail` or `inconclusive` item does not open the PR. The coordinator reads the item, repairs, and dispatches a fresh verifier on the new head. After two repair loops the coordinator takes the human path with `needs-human` and the last item in the comment.

The verifier runs on the review route's model where the harness allows a model per subagent. Where it does not, it runs on the same model in a fresh context. Either way the review agent re-drives every item, so verifier drift is caught one stage later.

### 5.3 The lever rule

When an agent checks the same fact by hand twice, or writes a throwaway script to check it, it adds the tool to `skills/verify/<surface>/bin/` in the same PR, with one invocation line in `SKILL.md` and one line in the `## Evidence` items that uses it.

| Counts as a lever | Does not count |
|---|---|
| A `bin/` command that drives the surface and exits non-zero on failure. | A one-off `grep` over the tree. |
| A measurer command that `aif measure` can run and that emits one JSON record. | A shell history line pasted into the PR. |
| A fixture generator or seed script a check depends on. | A test that still passes when every dependency returns nothing. |
| A repro command for a bug, with its expected exit code. | A screenshot with no command that produced it. |

The reviewer checks a claimed lever three ways. It exists at the named path in the diff. `SKILL.md` names its invocation. The reviewer runs it on the head and gets the stated exit code. A lever that fails any of the three is a finding.

Relation to the ladder. Rung 2 turns a human intervention into a measurer or a test. The lever rule reaches the same place from the other side: the agent builds the check while it works, before any human sees a defect. A lever that proves useful across PRs is a candidate measurer. The operator promotes it through the area chat of C31. The agent never edits `verify.toml`.

### 5.4 The evidence contract

`## Evidence` holds one item per line, in a details block when it grows:

```
- <feature-id> · <command> · <head-sha8> · <observed result> · pass|fail|inconclusive
```

Long outputs go under the item in a fenced block, cut to a per-item cap the skill names. The full file stays at `$AIF_EVIDENCE_DIR` in the author's worktree, and the reviewer regenerates it by re-driving, never by copying.

The deterministic check, in the C11 slot, gains three lines. Every touched area with a skill has at least one item. Every item names a feature ID that exists in the map. No item reads `inconclusive`. A failure re-queues implement with the finding, as C11 does today.

### 5.5 Review

The review agent trusts no evidence item until it re-drives it.

1. Run every evidence item on the head. Compare the observed result to the stated one.
2. For a `bug` ticket, run the `## Repro` command in the base worktree of C28 and expect the stated failure. Then run it on the head and expect the pass. Red on base, green on head.
3. Run every claimed lever per section 5.3.
4. Post the reviewer's own items as a PR comment, in the same shape, with the reviewer's SHA and outputs.

A mismatch is a finding. A stated pass that turns to fail or inconclusive means the author's evidence was wrong. The reviewer repairs the code, or repairs the check when the check was the defect, then re-drives the full list. A bug that does not fail on base is a wrong root cause, and that finding goes to `needs-human` with both outputs. The two outcomes of the review contract stay: `gh pr ready` when every item passes on the reviewer's run, `needs-human` otherwise.

---

## 6. Setup and maintenance

### 6.1 The doctor

`aif doctor` prints one line per configured repository and surface: `verify skill <alias>/<surface>: ok`, `missing`, or `lint: <feature file>: area <id> unknown`. It warns when the implement route and the review route of one complexity level resolve to the same model family. That warning is Lauren's rule that a verdict comes from a different family than the work.

### 6.2 The setup ticket

Key `v` on a repository row of the Theory view creates the ticket `Create the verification skill for <alias>/<surface>` with labels `to-refine` and `verify-skill`. The daemon asks for the surface name inline. The body tells the agent to:

1. Interview the repository, not the operator: surface, run command, drive method, evidence, isolation.
2. Fix a checkout that does not start, or report it precisely, before any generation.
3. Write `bin/control-<surface>` to the contract of 4.4. Check `--help` and `--dry-run` on every subcommand.
4. Write `SKILL.md` with the eight sections of 4.2. Measure the Fast path and record its time.
5. Seed `features/` from `verify.toml`: one file per area that maps to this surface, the top three to five features.
6. Prove it once end to end: up, doctor, fast, drive one feature, snapshot, logs, down. Confirm the evidence still exists.
7. Paste the transcript, the ARIA snapshot, and the log excerpt into `## Evidence`. Write `## Why`. No `## How`.
8. Propose the `skills` entries for `verify.toml` as text in the PR. Never edit `verify.toml`.

A `verify-skill` ticket and its PR skip both prediction gates, like `model-pr`. They change no behaviour of the application. The PR check of C11 still runs, and the review still re-drives. The first proof of step 6 is Lauren's first unit by hand, done by the agent and checked by the review.

### 6.3 The maintain cadence

`ScheduleKind::Maintain` fires on `maintain.days`, default 7, and after each release train when `maintain.after_train` is on, default on. It creates `Maintain the verification skill for <alias>/<surface>` with `to-refine` and `verify-skill`, one per surface, and skips a surface that already has an open maintain ticket. The body tells the agent to:

1. Index hygiene: every feature file is in the README and binds to a live area ID.
2. Source wave: one read-only subagent per feature file reads the source and flags drift with citations. No driving yet.
3. Live pass: doctor, then drive every feature once. Doctor again after any failed drive.
4. Triage. A wrong description is doc drift, fix it. Working behaviour the harness cannot drive is a harness gap, fix it in `bin/`. Broken application behaviour is a product gap. Never fix product code.
5. Report each product gap as one new ticket labelled `bug`, with the drive transcript and the log excerpt. Keep it out of this PR.
6. Re-drive every harness fix before it ships.
7. End in exactly one outcome. `changed`: one PR under `skills/verify/<surface>/` only. `clean` or `blocked`: no PR, one comment with the coverage and the block, and the ticket closes.
8. Final `down`. Confirm the evidence path still exists.

A `bug` ticket that a maintain run files enters the governed path of C19.

---

## 7. Understanding: teach

The governor makes the operator explain to the agent, in the bootstrap and the area chat, and tests the operator, with cards and the interview. Lauren's Part 2 goes the other way: the agent explains, and the human then predicts. Teach is that path.

`TaskPurpose::Teach` runs under `theory.chat`. Key `t` on a merged PR row, a DELTAS row, or an AREAS row starts it. The prompt tells the agent to explain what changed and why, from the diff, the git and PR history, the feature map slice, and the model slice. It gives the smallest complete answer first, then adds layers on request. It builds a picture in steps, one part at a time. It keeps the confidence language of what it found in history: a hedge is a finding, not style. It names no framing labels and prints no quiz.

Teach ends with one `<aif-event-v1>` block per contradiction it finds between the model and the code, else with no block. A contradiction opens a theory event through `open_event`, on the record of the PR or the repository record. The inbox offers `t` after a card miss with cause `recall`, because a recall miss is the moment to teach.

Teach adds no claim to the model. The operator predicts again on the next ticket, and the delta measures whether the teaching held.

---

## 8. Steering and principles

The operator steers without a diff. These are the handles, in the order a new operator meets them.

| Handle | Changes | Where |
|---|---|---|
| Ticket text | Scope, acceptance criteria, the restatement the agent must match. | The GitHub issue. |
| Complexity labels | The model of implement and review per ticket. | `complexity:*`, `review-complexity:*`. |
| The model | The theory every agent reads as its slice. | `theory/model.toml`, edit-model flow. |
| `verify.toml` policies | Which properties gate, ratchet, or observe. Which skills an area names. | The area chat. |
| The verification skill | How agents launch, drive, and prove each surface. | Setup and maintain tickets, or a direct PR. |
| `rules.md` | One rule every prompt carries. | A theory event answer with rung 3. |
| Prompt edits | The stage wording. | The Settings view. |
| Principle names | Which decisions the stance rewards. | The index below, in `docs/STANCE.md`. |

The stance carries a principle index of twelve. An agent that lets a principle change a decision names it in `## Why`, with the decision it changed. A name with no changed decision is a finding. Every entry adapts a pstack principle by Lauren Tan.

| Principle | The decision it changes here |
|---|---|
| Prove It Works | No evidence item from a proxy or a self-report. |
| Build the Lever | A second hand check becomes a `bin/` tool in the PR. |
| Sequence Verifiable Units | The repro commit lands before the fix commit. Each chunk is verified before the next wave. |
| Fix Root Causes | No fix ships without a base repro that fails. |
| Test Behavior, Not Implementation | A test asserts a literal result through the public path. |
| Never Block on the Human | Reversible work proceeds. Only a product call earns `needs-human`. |
| Encode Lessons in Structure | A repeated finding becomes a lever or a rung, never more prose. |
| Laziness Protocol | The smallest script that proves the job. Never a framework. |
| Model the Domain | Name the data shape before the first edit. |
| Subtract Before You Add | Delete dead weight in its own commit before the feature. |
| Separate Before Serializing Shared State | Chunks split by owned path. Shared files go to the coordinator. |
| Guard the Context Window | Bulk reads go to read-only subagents. Summaries stay in the coordinator. |

---

## 9. Changes to the governor documents

The toolbelt asks for three edits to the sibling documents. Each is one sentence.

| Document | Today | Change |
|---|---|---|
| Design record §5.1, spec C28 | The map names a skill by name. The factory passes only the name. | The factory inlines the skill slice of 4.6 into `{skills}`. |
| Spec C11, R27 | The check requires `## Why` and forbids `## How`. | The check also applies the three evidence lines of 5.4. |
| Spec §2 and the refine cwd in `dispatch_one` | Refine runs in the repository checkout. | Refine runs in the issue worktree. |

Nothing else in C0 to C32 changes.

---

## 10. Pipeline changes

| Stage or task | Change |
|---|---|
| refine | Runs in the issue worktree. Restate, Ground, Decisions, and Repro sections. The Lever column. Reads the refine slice of `{skills}`. |
| implement | The verifier subagent writes `## Evidence`. The lever rule. No PR opens on a fail or inconclusive item. |
| body check | Three evidence lines join the C11 check. |
| review | Re-drives every item. Red on base, green on head for a bug. Runs every claimed lever. Posts its own items. |
| release | Unchanged. |
| `verify-skill` tickets | Skip both prediction gates. Take the normal pipeline otherwise. |
| `maintain` cadence | New `ScheduleKind`. Creates one maintain ticket per surface. |
| `teach` | New `TaskPurpose` under `theory.chat`. |
| doctor | Skill lines per surface. Family warning. |

New labels: `verify-skill`. New marker blocks: none. Teach reuses `<aif-event-v1>`. New environment: `AIF_SKILLS_DIR`, `AIF_EVIDENCE_DIR`.

---

## 11. Configuration sketch

```toml
[repo.borsuk]
path = "/home/you/Workplace/borsuk"
theory = { repo = "navaro1/borsuk-theory", path = "/home/you/Workplace/borsuk-theory" }
skills = { path = "/home/you/Workplace/borsuk-verify" }   # optional, default: the theory checkout
maintain = { days = 7, after_train = true }
```

The exact field names belong to the spec.

---

## 12. The terminal UI

| Surface | Addition |
|---|---|
| Theory view, AREAS panel | A skill mark per area: `✓` resolved, `-` none, `!` lint. Key `v` creates a setup ticket. Key `t` teaches an area. |
| Theory view, DELTAS panel | Key `t` teaches the PR of a delta. |
| Pipeline view | A merged PR row takes `t`. A held implement row shows `awaits evidence` while the verifier loops. |
| Inbox | A card miss with cause `recall` offers `t`. |
| Settings | The `skills.path` and `maintain` fields per repository. |
| Doctor | The lines of 6.1. |

---

## 13. Build order

| Chunk | Content | Depends on |
|---|---|---|
| V0 The stance addendum | Rules 13 to 20, refusal 4, the vocabulary, the principle index of section 8, the steering table. Sources cited. | C3 |
| V1 Skill resolution and the doctor | `skills/verify/` layout parser, feature front matter, the resolution order of 4.1, the lint, `TheoryConfig.skills`, `AIF_SKILLS_DIR`, the doctor lines and the family warning, the AREAS mark. | C25 |
| V2 The setup ticket | Key `v`, the surface prompt, `create_issue` with `to-refine` and `verify-skill`, the body of 6.2, the gate skip for `verify-skill`. | C16, C32, V1 |
| V3 `{skills}` slicing | The slice rule of 4.6 for refine, implement, review, teach. The cap. Replaces the name fill of C28. | C10, C28, V1 |
| V4 The evidence contract and the check | The item grammar, `AIF_EVIDENCE_DIR`, the three lines in `check_pr`. | C11, V3 |
| V5 Refine grounding | The issue worktree for refine. The Restate, Ground, Decisions, Repro sections. The Lever column. The refine prompt rewritten and pinned. | C10, V3 |
| V6 The verifier and the lever | The implement prompt rewritten and pinned: roles, the verifier, the lever rule, no PR on fail or inconclusive, two repair loops then `needs-human`. | V4 |
| V7 Review re-drive | The review prompt rewritten and pinned: re-drive, red on base for a bug, lever checks, the reviewer's own items. | C28, V4 |
| V8 The maintain cadence | `ScheduleKind::Maintain`, `maintain` config, the body of 6.3, one open ticket per surface. | C21, V2 |
| V9 Teach | `TaskPurpose::Teach`, the prompt, keys `t`, the recall offer, events through `open_event`. | C17, V3 |

V0 can start now. V1 to V4 wait for the trust-loop parsers. V5 to V7 rewrite the three prompts once each and can land in one train. V8 and V9 come last.

---

## 14. Follow-ups outside this design

- Media evidence: screenshots and video, once a home exists that `gh` can link.
- A multi-model review panel with an agreement map, if the single reviewer plus re-drive proves too weak.
- A blinded eval of a prompt change, in the style of the pstack eval playbook.
- Cloud agents, when the worktree count becomes the limit, as the governor already notes.
- A decision-log comment per PR, if overnight orchestrations appear and the marker blocks stop being enough.

---

## 15. Open items for the spec

- The exact TOML fields of `skills` and `maintain`, and the front-matter keys of a feature file.
- The per-item output cap of an evidence item, and the details-block form.
- The surface prompt of key `v`: inline text, or a pick from `verify.toml` boundaries.
- Whether a `verify-skill` PR in shadow mode rides the model branch of C7 or its own `aif/<alias>/skills-<n>` branch.
- The harness capability table for a per-subagent model, so the verifier binding of 5.2 is precise.
