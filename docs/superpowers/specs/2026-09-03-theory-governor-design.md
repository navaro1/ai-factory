# The Theory Governor

Date: 2026-09-03 · Status: Design record, brainstorm complete, no code · Scope: The theory loop, the trust loop, the ladder, and the stance document · Predecessor: docs/v0.6/SPEC.md

Sources: Peter Naur, *Programming as Theory Building*. Piotr's notes on agentic coding and theory building (2026-09). Lauren Tan, *Loops You Can Trust* (2026-06-24) and *The Complete Guide to pstack, Part 1* (2026-08-31). Borsuk, as inspiration for scope, ratchet, and report shape.

---

## 1. Thesis and identity

Agentic coding raises the rate of system change. It does not raise the rate at which a human assimilates change. The gap between the real system and the operator's theory of it grows. Tests pass, CI is green, and the operator's model of the system decays. The operator mistakes successful delegation for understanding.

ai-factory takes one stance against this:

> The human is the limiting step, by design, at the system level, never at the diff level.

ai-factory is a harness coordinator. It owns the loops, the gates, the window, the routing, and the caches. The harnesses own all work. The factory never writes a measurer, a rule, or a model entry itself. It dispatches an agent, waits for a GitHub state change, and keeps the operator at the theory level.

The design runs two loops and one coupling:

| Loop | Question it answers | Source |
|---|---|---|
| The trust loop | Is the change correct? Agents verify agents at runtime, with evidence, not claims. | Lauren Tan |
| The theory loop | Does the operator's theory still match the system? A prediction before the change, an explanation after it. | Naur, Piotr's notes |
| The governor | The theory loop bounds the throughput of the factory. A governor is a device that limits the speed of an engine. | This document |

The factory refuses three things:

1. An agent never writes a prediction. An agent can challenge a vague one.
2. An agent never edits the model. An agent can propose an entry.
3. No human reads code to catch a defect. That is Lauren's rung 4, and the factory does not offer it.

The stance lives in `docs/STANCE.md`, linked from the first section of the README. The names in that document are the names of the configuration and of the terminal UI.

---

## 2. Vocabulary

| Term | Meaning |
|---|---|
| Model | The operator's theory of one repository, as a TOML file of entries. Human-owned. |
| Entry | One table in the model: an invariant, a state, a transition, a boundary, or a failure mode. Each entry has a stable ID. |
| Area | The region of a repository that one boundary entry covers. The boundary entry carries the path globs of its area. |
| Theory repository | The repository that holds the model and the theory records. It is the code repository, or a private shadow repository. |
| Shadow mode | The theory repository is a separate private repository. The code repository sees only what the pipeline did in v0.6. |
| Short prediction | One or two sentences by the operator at `to-refine`, from the raw ticket. |
| Full prediction | Five slots by the operator after `refined`: the behaviours that change, the states or transitions that change, the invariants at risk, the failure modes added or changed, and the other areas the change can touch. Each slot names entry IDs. |
| Tag | The confidence of one slot: sure or unsure. |
| Delta | The comparison of a prediction with the real change, per slot: sure-hit, sure-miss, unsure-hit, unsure-miss. |
| Theory event | One inbox item that asks the operator which model entry is wrong or missing. |
| Miss cause | The operator's verdict on a miss: the model, the PR, or recall. |
| Window | The cap on open deltas and open theory events per repository. A full window pauses implement. |
| Card | One retrieval question per day, from a merged PR without a prediction or from a stale entry. |
| Interview | One weekly agent-led session that probes the model and the recent changes. |
| Ladder | Lauren's order of elimination. Rung 1: architecture. Rung 2: a lint, a test, or a measurer. Rung 3: a skill or a rule. |
| Verification map | The TOML file of areas, statements, measurers, and skills. The operator owns the statements. Agents own the projection. |
| Measurer | One deterministic script that emits measurement records for one area. |
| Measurement | One record: an ID, a value, a unit, and a direction. |
| Lever | The command `aif measure`, which lets an agent ask the daemon for a measurement. |
| Audit | The task kind that checks model entries against the code. |
| Measure | The task kind that runs measurers. It has no harness. |

---

## 3. Rules

These rules hold across every part of the design.

1. GitHub stays the source of truth. Every theory record is a GitHub artifact: a file in the theory repository, a comment, a label, or an issue. The factory keeps no journal. A cache holds only what the factory can derive again.
2. The operator owns the statements: the model entries, the area list, and what "good" means per area. Every projection of them, path globs, scripts, and skills, is agent output under review.
3. An agent that structures the operator's stream of consciousness can sharpen and ask. It cannot add a claim or a property that the operator did not name.
4. A prediction comes from the operator's memory and from the operator's own text. The chat action on a ticket is disabled until the short prediction exists, and again between `refined` and the full prediction.
5. An entry describes behaviour, never implementation. No function names. No file names. The path globs on a boundary entry are the one exception, because the delta needs them.
6. The factory executes every measurement. An agent can only trigger one. Only factory records count.
7. The governor path never shows a diff to the operator. The operator reads Why, Before, After, entries, and events.
8. A task binds the model version at dispatch, the way it binds its settings.
9. Every human intervention on a repeat is a defect of the factory. The first occurrence reaches the operator as a theory event. The ladder eliminates the repeat.
10. One operator per daemon. Every record still carries its GitHub author.
11. The governor is mandatory by default. One explicit escape exists per repository, and the factory shows it as a warning.
12. A deterministic check runs before any agent check, wherever a deterministic check exists.

---

## 4. The theory loop

### 4.1 The model

The model is one TOML file in the theory repository. Each entry is one table. The table holds the ID, the kind, the title, the statement in prose, and the relations as arrays. The relations are: a transition names its from-state and its to-state; a boundary names its two sides and its path globs; an invariant names the states or areas it constrains; a failure mode names the boundary it crosses. The Theory view draws the map from the relations. The audit checks the statements.

The operator is the only writer. An edit travels as a model-only PR. The factory recognizes the PR by its paths. The PR skips the prediction gates. Its review stage runs the audit role: the agent checks each changed entry against the code and reports a contradiction as a theory event. The release train merges it. A PR from an agent that touches the model file fails the deterministic body check and returns to implement.

A task binds the model version at dispatch. When an entry in the task's predicted areas changes before the review ends, the factory raises one theory event: "entry X changed while PR #n was in flight. Does the PR still hold?" The operator chooses: re-queue implement under the new model, or accept the PR. No automatic restart happens.

Each agent receives a slice, not the whole model. The implement agent receives every entry of the areas in the full prediction, plus every invariant and failure mode that names one of those areas or their states, plus every boundary that touches them. The review agent receives the same slice, plus the areas that the diff touched. The refine agent receives the slice of the short prediction.

### 4.2 Bootstrap

A model does not exist for an existing repository. The operator writes it from memory, one area at a time. The first area is the area of the next ticket. The operator dictates a stream of consciousness in a chat role. An agent structures it into entries with IDs and adds no claim. The result travels as a model-only PR. The audit runs as its review and reports wrong claims and missing parts. Each discrepancy is a theory event.

Coverage grows where change happens. A ticket that names an area without entries holds at its gate until the operator bootstraps that area. The Theory view shows "area X has no entries" and offers the bootstrap action.

### 4.3 Predictions

Two predictions exist per ticket.

| Prediction | When | Gate | Input allowed |
|---|---|---|---|
| Short | When the operator adds `to-refine`. | Refine does not dispatch without it. Every area it names must have entries. | The raw ticket and the model. |
| Full | After `refined`, before implement. | Implement does not dispatch without it. Every area it names must have entries. | The refined ticket and the model. No chat. |

The full prediction has five slots. Each slot names entry IDs and carries one tag: sure or unsure. The factory writes a template file with the candidate IDs of the named areas and opens the operator's editor from the TUI. On save, the factory parses the file, rejects a malformed one at once, and posts the prediction as a marker block comment on the theory record of the ticket. The short prediction is one inline line in the TUI.

An agent never writes a prediction. The refine agent can comment that a slot is vague. The operator decides.

### 4.4 The delta

The review agent computes the delta. It receives the diff, both predictions, the model slice, and the measurement comparison. Until part 2 exists, the comparison is absent and the agent judges correctness from the tests and the evidence alone. It reports three things: correctness, model conformance, and the delta per slot. The delta of each slot is one of: sure-hit, sure-miss, unsure-hit, unsure-miss. A path that maps to an area outside the prediction is a miss on the "other areas" slot.

The agent returns the delta in a marker block in its report. The factory parses the block, posts it as a comment on the theory record, and adds the window label. A hit-only delta needs one confirmation from the operator to close. A delta with a miss raises a theory event per miss.

### 4.5 The window

Each repository has a cap on open deltas plus open theory events. When the count reaches the cap, the factory pauses implement dispatch for that repository. Refine and review continue. The count derives from the window label, so the factory keeps no counter. The Theory view shows the gauge and the pause state.

### 4.6 Theory events

A theory event is one inbox item. It carries the event, the model entries in scope, the prediction when one exists, and one question. The question form follows the tag: a sure-miss asks "which entry is wrong", an unsure-miss asks "which entry is missing". The review agent writes the question for a delta. The audit role writes it for every other event.

The sources of theory events:

| Source | Detected by |
|---|---|
| A prediction miss | The delta in review. |
| A model violation | Model conformance in review. |
| A `needs-human` question from an agent | The existing label. |
| A merge conflict in a release train | The existing release stage. |
| A bug ticket | A new ticket with the label `bug`. The audit role maps it to entries. |
| A mid-flight model change | The binding check in section 4.1. |
| A card or interview miss | The grader in section 4.7. |
| An audit contradiction | The audit role in section 4.8. |

The operator's answer has two parts: the miss cause, and the rung.

| Cause | Meaning | Consequence | Closes when |
|---|---|---|---|
| The model | The entry is wrong or absent. | The operator edits the model. | The main branch of the theory repository carries a change to the named entry, after the answer. The factory reads the git history of the model file. |
| The PR | The entry is right. The PR violated it. The reviewer missed it. | The PR returns to implement with the finding. The reviewer prompt gains a rule. | The answer. |
| Recall | The entry is right and present. The operator did not recall it. | No edit. The miss counts toward the card signal. | The answer. |

The rung is 1, 2, or 3. Section 6 describes what the factory does with it.

### 4.7 Cards and the interview

Once per day the factory builds a batch of at most three cards, on the daemon's deadline clock. Two sources feed it. The first is a merged PR with no prediction by the operator: "PR #88 merged, area Y. Which entries changed, and how?" An agent grades the answer against the diff. The second is spaced repetition over the model: an entry that no prediction touched for N days: "State INV-3. What would violate it?" An agent grades the answer against the entry and the code. A miss on a card is a theory event under the rules of section 4.6.

Once per week an agent leads an interview. It is the ticket chat with the roles reversed: the agent asks, for a fixed time, about the model and the recent changes. It writes each gap as a theory event. The interview runs on the same machinery as the cards, with a different prompt.

### 4.8 Audit cadence

The audit role runs in four ways:

1. As the review of a model-only PR.
2. As the grader of a card and the mapper of a bug ticket.
3. As a weekly sweep per repository: every entry against the code.
4. As a scoped sweep after each release train: the entries of the areas the batch touched. This checks the combined effect of a batch, which single-PR deltas cannot see.

`aif doctor --audit` runs the full sweep on demand. The events of a sweep enter the Theory view as one batch under the same daily cap as the cards, so a sweep never floods the inbox.

### 4.9 Shadow mode

In shadow mode, the theory repository is a separate private repository. It holds the model, the verification map, and one shadow issue per code ticket or PR. The shadow issue title keys it to the code item, for example `borsuk#142`. The shadow issue carries the predictions, the delta, the theory events, and the window label. The code repository sees only what the v0.6 pipeline did: labels, agent PRs, agent comments.

Agent output that names model entries goes into a marker block in the agent report. The factory parses the block and posts it to the theory repository, never to the code PR. In shadow mode the "Why" section of a PR names behaviours in words, never entry IDs.

A promotion step can come later. The factory moves the model into the code repository. The IDs stay stable, so nothing else changes.

---

## 5. The trust loop

### 5.1 The verification map

The verification map is one TOML file in the theory repository. Each area maps to one boundary entry of the model. An area holds one statement of what "good" means: named properties, each one "must hold" or "must not worsen", with thresholds. An area lists its measurers and its skills.

The ownership flow:

| Step | Who | Output |
|---|---|---|
| 1. Dictate | Operator | A stream of consciousness per area. |
| 2. Refine | Agent | The structured statement. The agent asks short questions. It adds no property. |
| 3. Approve | Operator | The statement becomes the "what" of the area. |
| 4. Implement | Agent, through the pipeline | The measurers, as a ticket. |
| 5. First run | Operator | Approval of the first measurement. |

The map names a skill by name. Each harness resolves the skill itself. The factory passes only the name. Skills carry the fuzzy runtime checks, such as driving the application and capturing evidence. Their evidence goes into the PR body.

### 5.2 Measurement

The factory executes every measurement. The `measure` task has no harness and no model. It has a concurrency limit and a cache keyed by tree hash, area, and measurer hash. A tree hash covers uncommitted work, so an agent can measure before it commits.

An agent runs `aif measure` inside its worktree. The command asks the daemon over the existing socket to measure the touched areas. The daemon runs the scripts, caches the result, and returns the comparison in agent-text form. An agent can call the lever at any time, as often as it likes. Repeats are free. A measurement that an agent asked for is still a factory number.

"Before" is the record at the base commit. "After" is the record at the PR head. The daemon runs both itself when no agent asked for them. A new PR head, which the supersede logic already detects, triggers a new "after". The factory posts Before and After as one comment on the PR.

Each measurer emits one small JSON record per measurement: an ID, a value, a unit, and a direction. The factory compares records. The states are new, resolved, worsened, improved, unchanged, and incomparable. The policy is ratchet by default: no new or worsened value. Hard gates use error. The rest uses observe. The review agent reads the comparison and judges. It never computes.

Verification follows the diff. Only the areas that a change touches run. An area can widen its scope, the way a TypeScript project widens to its project references. Implement runs the fast mode. Review runs the PR mode. The release train runs the full mode on the batch before it merges.

### 5.3 The PR body

The agent writes two sections: Why, and the evidence of any skill run. The Why names the behaviour that changes and, outside shadow mode, the entry IDs. The agent writes no How. The diff is the How, and only the reviewer reads it. Model proposals go into the routed marker block. The factory posts Before and After.

The factory checks the body before it dispatches the review. A body without Why, or with a How narrative, re-queues the implement task with one finding. The agent fixes its own PR. No reviewer tokens. No human.

### 5.4 Plugins

Later. A plugin packages measurers and skills for one stack. The map refers to a measurer by name, so a plugin is only one more source of measurers. Nothing in this design depends on a plugin.

---

## 6. The ladder

Every theory event ends with one rung. Rung 4, human review of code, does not exist in the factory.

| Rung | Meaning | What the factory does |
|---|---|---|
| 1 | Architecture or a data structure eliminates the class. | Opens a ticket with the label `to-refine`. The operator shapes it like any ticket. |
| 2 | A lint, a test, or a measurer catches it. | Opens a ticket for a new measurer in the verification map. An agent implements it. The ratchet catches the defect next time. |
| 3 | A skill or a rule prevents it. | Records a rule under one fixed path in the theory repository. An agent drafts it. The operator approves it. The factory appends the rules of a repository to its stage prompts. |

The ladder metric is the count of events per rung over time, next to the calibration share. A repository where rung 1 and rung 2 grow and events shrink is a factory that learns.

---

## 7. Pipeline changes

| Stage or task | Change |
|---|---|
| `to-refine` | The short prediction gate. Every named area must have entries. |
| refine | Reads the slice of the short prediction. Can comment that a slot is vague. |
| `refined` | The full prediction gate. Every named area must have entries. Chat disabled until the prediction exists. |
| implement | Binds the model version. Reads its slice. Runs the lever freely. Writes Why and evidence. Never touches the model file. |
| body check | Deterministic. Runs before review. A failure re-queues implement. |
| review | Reads the diff, both predictions, the slice, and the comparison. Reports correctness, conformance, and the delta. Writes the explain-back question. For a model-only PR, the audit role runs instead. |
| release | Runs the full measurement mode on the batch. Merges. Runs the scoped audit sweep after the train. |
| `measure` | New task kind. No harness. Concurrency limit. Cache. |
| `audit` | New task kind with a role table. Reviews model PRs, grades cards, maps bug tickets, runs sweeps. |
| cards | Daily batch on the deadline clock. |
| interview | Weekly session on the chat machinery. |

Marker blocks follow the existing `<aif-ticket-proposal-v1>` pattern. New blocks: the prediction, the delta, the model proposal, the measurement comparison, and the theory event. The factory routes every block to the theory repository.

---

## 8. Configuration sketch

```toml
[stage.audit]
harness = "claude"
model = "claude-opus-5[1m]"
limit = 2

[measure]
limit = 2

[repo.borsuk]
path = "/home/you/Workplace/borsuk"
governor = "on"                  # "off" shows a warning and a doctor line
window = 3                       # open deltas plus open events
theory = { repo = "navaro1/borsuk-theory" }   # omit for in-repository mode
sweep = { days = 7, after_train = true }
cards = { per_day = 3, stale_days = 30 }
interview = { weekday = "monday" }
```

The exact field names belong to the per-part specs.

---

## 9. The terminal UI

A Theory view joins the pipeline, the inbox, and the tickets.

| Panel | Shows |
|---|---|
| The map | The model as a diagram per area: states as boxes, transitions as arrows, boundaries as frames, failure modes as marked edges. Box-drawing characters. |
| The window | One gauge per repository: open deltas against the cap, and the pause state of implement. |
| The deltas | The open deltas and theory events, each with its slot outcomes. |
| The ladder | Events per rung over time, and the calibration share. |

Long text goes to `$EDITOR` through a template file: the full prediction, a model edit, a card answer that needs more than a line. On save, the factory parses, posts, and for a model edit commits, pushes, and opens the model-only PR. Short items stay inline: the short prediction, a hit confirmation, a miss cause, a rung, a card answer. The stream of consciousness for a bootstrap or an area statement goes through a chat role in the existing chat pane.

The Settings view marks `governor = "off"` with a warning. The doctor reports every repository with the governor off, and offers `--audit`.

---

## 10. Build order

| Part | Content | Depends on |
|---|---|---|
| 0. The stance | `docs/STANCE.md`. The thesis, the loops, the governor, the ladder, the rules, the refusals, the names. | nothing |
| 1. The theory loop | The model file with path globs on boundaries, the bootstrap, both predictions and gates, the delta in review, the window, theory events and their closing rules, cards and the interview, the audit role and its cadence, shadow mode, the Theory view. | 0 |
| 2. The trust loop | The verification map, the `measure` task, the lever, the cache, before and after, the ratchet, the PR body contract and check, the modes per stage. | 0 |
| 3. The ladder | The rung on each event, the tickets it opens, the rules path, the ladder metric. | 1 and 2 |

Each part gets one spec and one implementation cycle. The path globs sit in part 1, because the delta maps diff paths to areas before any measurer exists.

---

## 11. Follow-ups outside this design

- Several operators per repository, with one shadow per person and a merge of theories.
- Promotion of a shadow model into the code repository.
- Plugins that package measurers and skills per stack.
- CI failures on the main branch as one more theory event source.
- Cloud agents instead of worktrees, when the worktree count becomes the limit.

---

## 12. Open items for the per-part specs

- The exact label names, marker block names, and TOML schema of the model and the map.
- The template file grammar for the full prediction.
- The measurement record schema and the agent-text form of a comparison.
- The layout algorithm for the map, and its behaviour on a large area.
- The default values of the window cap, the card cap, the stale days, and the sweep interval.
