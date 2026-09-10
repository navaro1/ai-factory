# The stance

This document is the stance of ai-factory. It names the ideas, the rules, and
the vocabulary that the configuration and the terminal UI use. The design
records hold the full argument:
`docs/superpowers/specs/2026-09-03-theory-governor-design.md` and
`docs/superpowers/specs/2026-09-10-verification-toolbelt-design.md`.

## Thesis

Agentic coding raises the rate of system change. It does not raise the rate at
which a human assimilates change. The factory takes one stance:

> The human is the limiting step, by design, at the system level, never at the
> diff level.

Verification is the second limiting step:

> Verification is the limiting step. An agent that can verify its own work
> closes the loop. An agent that cannot makes the human the bottleneck at the
> diff level.

The factory is a harness coordinator. It owns the loops, the gates, the
window, the routing, and the caches. The harnesses own all work. The factory
never writes a measurer, a rule, or a model entry itself.

## The loops

| Loop | Question it answers |
|---|---|
| The theory loop | Does the operator's theory still match the system? A prediction comes before the change, an explanation after it. |
| The trust loop | Is the change correct? Agents verify agents at runtime, with evidence, not claims. |
| The governor | The theory loop bounds the throughput of the factory. A governor is a device that limits the speed of an engine. |

The governor is mandatory by default. One explicit escape exists per
repository, and the factory shows it as a warning.

## The ladder

Every theory event ends with one rung. Rung 4, human review of code, does not
exist in the factory.

| Rung | Meaning | What the factory does |
|---|---|---|
| 1 | Architecture or a data structure eliminates the class of defect. | Opens a ticket with the label `to-refine`. |
| 2 | A lint, a test, or a measurer catches it. | Opens a ticket for a measurer in the verification map. |
| 3 | A skill or a rule prevents it. | Records the rule, and the operator approves it. |

## The rules

1. GitHub stays the source of truth. Every theory record is a GitHub artifact. The factory keeps no journal.
2. The operator owns the statements. Every projection of them is agent output under review.
3. An agent that structures the operator's stream of consciousness can sharpen and ask. It cannot add a claim the operator did not name.
4. A prediction comes from the operator's own text. Chat on a ticket stays disabled until the short prediction exists.
5. An entry describes behaviour, never implementation. The path globs on a boundary entry are the one exception.
6. The factory executes every measurement. An agent can only trigger one.
7. The governor path never shows a diff to the operator. The operator reads Why, Before, After, entries, and events.
8. A task binds the model version at dispatch.
9. Every human intervention on a repeat is a defect of the factory. The ladder eliminates the repeat.
10. One operator per daemon. Every record still carries its GitHub author.
11. The governor is mandatory by default. The escape shows as a warning.
12. A deterministic check runs before any agent check.

The toolbelt adds rules 13 to 23.

13. An agent verifies on the real surface. A Before / After line states the command and the observed result, or it is `inconclusive`.
14. A question an experiment can answer is not the operator's. An agent runs the experiment and records the result. Only a product or preference call earns `needs-human`.
15. The agent that judges a change never wrote it. The review agent re-drives every line on a fresh context and another model.
16. A check done by hand twice becomes a lever in the same PR. A lever the reviewer cannot rerun is not a lever.
17. No confirmed reproduction, no authored fix. A bug ticket reproduces twice through the driver before any plan.
18. The skills checkout is the theory checkout, unless a path override says otherwise.
19. Evidence is text. Transcripts, ARIA snapshots, exit codes, and log excerpts travel in the PR body and in comments.
20. An agent probes for drivers and never installs one. A new driver is a ticket the operator opens.
21. The daemon inlines skill content into the prompt. It never relies on a harness skill loader or a plugin.
22. The ticket defines done. Every acceptance criterion is falsifiable and names its check. Every Before / After line names the criterion it proves.
23. The simplest change that meets every criterion ships, in the style the repository already has. A test that still passes with the change reverted is not a test.

## The refusals

1. An agent never writes a prediction. An agent can challenge a vague one.
2. An agent never edits the model, the verification map, or the rules. An agent can propose an entry.
3. No human reads code to catch a defect.
4. A test alone is not verification. A Before / After line that rests on "it compiles" or "tests pass" is `inconclusive`, and `inconclusive` is not a pass.
5. No browser-profile integration. Headless Playwright through a CLI is the web driver.

## The driver ladder

The setup ticket probes the toolchain and picks the highest tier it finds. An
agent never installs a driver. The operator sets a floor per area in
`verify.toml`. A line below the floor is `inconclusive`, and the operator
accepts the lower tier or stops the line.

| Tier | Driver | Proves | Needs |
|---|---|---|---|
| `browser` | `playwright-cli`, `agent-browser`, or `chromium-cli`, headless | Rendered UI, clicks, ARIA snapshots | Playwright in the toolchain |
| `dom` | The repository's `jsdom` or `happy-dom` runner, or fetch plus an HTML parser | Rendered markup and component behaviour | Node or Python already present |
| `http` | `curl` or an HTTP or GraphQL client | Status, headers, body, side effects in logs and data | Nothing new |
| `terminal` | `tmux`, `agent-tty`, or direct invocation | A CLI or TUI end to end | `tmux` |
| `none` | No driver reaches the feature | Nothing. Every line is `inconclusive` | Nothing |

## The principles

The stance carries six principles as vocabulary. Each one is backed by a
structural check, not by a naming rule. An agent names none of them in a PR.

| Principle | The structure that enforces it |
|---|---|
| Prove It Works | The Before / After lines and the body check. |
| Build the Lever | The fast command per feature and the daemon's fast check. |
| Never Block on the Human | Prototype before ask in refine. |
| Fix Root Causes | Red on base, green on head in review. |
| Laziness Protocol | The trace and scope lines of the body check, and the reviewer's pass for what the diff does not need. |
| Test Behavior, Not Implementation | Every added test runs against base in review and must fail there. |

## Steering

The operator steers without a diff.

| Handle | Changes | Where |
|---|---|---|
| Ticket text | Scope, acceptance criteria, the restatement the agent must match. | The GitHub issue. |
| Complexity labels | The model of implement and review per ticket. | `complexity:*`, `review-complexity:*`. |
| The model | The theory every agent reads as its slice. | `theory/model.toml`. |
| `verify.toml` | Policies, floors, and skills per area. | The area chat. |
| The run skill | How agents launch, drive, and prove each surface. | Setup tickets, or a direct PR. |
| `rules.md` | One rule every prompt carries. | A theory event answer with rung 3. |
| Prompt edits | The stage wording. | The Settings view. |

## Vocabulary

| Term | Meaning |
|---|---|
| Model | The operator's theory of one repository, as a TOML file of entries. The operator is the only writer. |
| Entry | One table in the model: an invariant, a state, a transition, a boundary, or a failure mode. Each entry has a stable ID. |
| Area | The region of a repository that one boundary entry covers. |
| Theory repository | The repository that holds the model and the theory records. |
| Shadow mode | The theory repository is a separate private repository. The code repository sees only the pipeline. |
| Short prediction | One or two sentences by the operator at `to-refine`. |
| Full prediction | Five slots by the operator after `refined`, each naming entry IDs, each tagged sure or unsure. |
| Delta | The comparison of a prediction with the real change, per slot. |
| Theory event | One inbox item that asks the operator which model entry is wrong or missing. |
| Miss cause | The operator's verdict on a miss: the model, the PR, or recall. |
| Ladder | The order of elimination: architecture, a check, a skill or a rule. Rung 4 does not exist. |
| Verification map | The TOML file of areas, statements, measurers, and skills. |
| Measurer | One deterministic script that emits measurement records for one area. |
| Measurement | One record: an ID, a value, a unit, and a direction. |
| Audit | The task kind that checks model entries against the code. |
| Measure | The task kind that runs measurers. It has no harness. |
| `governor` | The switch of the theory loop per repository. Off holds v0.6 behaviour and shows a warning. |
| `window` | The cap on open deltas and open theory events per repository. A full window pauses implement. |
| `theory` | The theory repository configuration: in-repository mode, or shadow mode. |
| `sweep` | The audit sweep cadence over the model entries. |
| `cards` | The daily batch of at most three retrieval questions. |
| `interview` | The weekly agent-led session that probes the model. |
| `theory-short` | The label that marks a ticket with a posted short prediction. |
| `theory-full` | The label that marks a ticket with a posted full prediction. |
| `delta-open` | The label of a record with an open delta. |
| `event-open` | The label of a record with an open theory event. |
| `model-pr` | The label of a model-only PR. It skips the prediction gates. |
| `ladder-1` | The label of a rung 1 ticket. |
| `ladder-2` | The label of a rung 2 ticket. |
| `ladder-3` | The label of a rung 3 record. |
| `verify-skill` | The label of a run-skill ticket. It skips the prediction gates. |
| `MAP` | The Theory view panel that draws the model of one area. |
| `DELTAS` | The Theory view panel of open deltas and events. |
| `LADDER` | The Theory view panel of events per rung over time. |
| `AREAS` | The Theory view panel of areas, their measurers, and their tiers. |
| surface | One thing a user touches: a web UI, an admin panel, an HTTP API, a CLI, a TUI, a library. |
| run skill | The file `.claude/skills/run-<surface>/SKILL.md` plus its `features/` directory. Agent-owned, reviewed like any code. |
| driver | The generic tool that drives a surface. Its level is one of the five tiers. |
| tier | The driver's level: `browser`, `dom`, `http`, `terminal`, or `none`. |
| floor | The lowest tier the operator accepts for an area, set in `verify.toml`. |
| feature | One user-facing behaviour of one surface. It binds to one area of `verify.toml`. |
| fast command | The one command per feature that is the quickest proof it still works. Exit code is the verdict. |
| fast check | The daemon's run of a fast command as a measure task before review. |
| Before / After line | One line in `## Before / After`: criterion, feature, tier, command, before, after. |
| acceptance criterion | One falsifiable statement in the refined ticket, with an ID `AC-<n>`, that names its check. |
| trace | The chain from one acceptance criterion to the Before / After line that proves it. |
| lever | The command `aif measure`, or a helper script under a run skill that proves one fact and exits non-zero on failure. |
| teach | The `theory.chat` purpose that explains a merged PR, a delta, or an area to the operator. |
| skills checkout | The git checkout that holds the run skills. The theory checkout by default. |
