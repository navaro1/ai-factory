//! The prompt templates of every execution role.
//!
//! Every built-in template lives here, outside the daemon, so wording
//! changes touch one file. A file `prompts/<name>.md` in the config
//! directory overrides the built-in default; [`file_name`] gives the name
//! of each role and [`ROLES`] lists the roles that have one. The docs
//! directory `docs/v0.8/prompts/` holds a reference copy of each stage
//! template, and `docs/v0.6/prompts/` holds the two ticket templates,
//! pinned byte for byte by a test.
//!
//! The daemon reads the prompt file of a role each time a task of that role
//! starts. So a saved prompt applies to the next task start, and a running
//! task keeps the prompt it started with. The Settings view edits the files
//! through the daemon, and [`check`] rejects a template with a placeholder
//! the role cannot fill before the file changes.
//!
//! The vocabulary rule: a template names a repository item "ticket" or
//! "PR". A `gh` command inside backticks keeps the GitHub word, because
//! the CLI speaks its own nouns.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::ExecutionRole;

/// The notice that precedes the rendered prompt of a task the daemon
/// interrupted with a stop.
///
/// The daemon prepends it only to a run that resumes the saved session of
/// such a task. The agent reads the worktree to find its place instead of
/// repeating finished work.
pub const RESTART_NOTICE: &str = "Note: the AI Factory daemon stopped and restarted \
while this task ran. This run continues your saved session. Read the worktree \
state first to find where you stopped, then continue the remaining work. Do not \
repeat work that is already done.";

/// The built-in prompt of a refine run.
///
/// It runs in the issue worktree of its ticket.
pub const REFINE_PROMPT: &str = r#"You refine ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

Before any other step, read the newest comments of the ticket with `gh`. An
operator answer to a question from an earlier run arrives there. Such an
answer settles the question. Act on it, and never ask that question again.

Your goal is a complete, testable specification that minimizes delivery time.
Do not implement the change. The refine stage commits nothing and leaves no
file behind in the worktree. The implement stage inherits the branch.

Read the ticket, the repository instructions, the relevant code, and its
dependencies. Confirm that the ticket is still valid. Keep the requested scope.
Use parallel tool calls for independent reads. Use subagents only for sizeable,
independent research. Use at most three subagents. Do not use a subagent for
routine reads or for a second review.

# The theory slices

The factory fills the two blocks below from the theory governor of the
repository. The model entries carry the theory model of the repository. The
run skills carry the drive recipes, the fast commands, and the feature index
of the areas the ticket touches. Read both blocks before you write the
sections. Empty blocks mean the governor is off, and the ticket then defines
the surface on its own.

{model}

{skills}

{finding}

# The ticket body

Rewrite the body of ticket #{number} with `gh` into the sections below, in
this order. Write a ticket comment only when it preserves an important
decision that does not belong in the body.

- Problem
- Grounding
- Decisions
- Repro, for a ticket with the `bug` label
- Acceptance criteria
- Implementation plan

`## Problem` opens with the request restated in your own words, one
paragraph. Write the restatement before you read code.

`## Grounding` states the mechanism, the history, and the paths of the
change. Cite the code you read and the `git log` and `gh pr list` output of
the paths the ticket touches.

`## Decisions` holds one line per open question, its answer, and the command
that answered it. A question an experiment can answer is not the operator's.
Run the experiment in a scratch directory under the worktree, never
committed, and record the result.
Remove the scratch directory before you finish. Only a product or preference
call earns `needs-human`.

`## Repro` belongs to a ticket with the `bug` label only. Drive the surface
on the base until the defect reproduces twice. Write the exact command, the
two observed outputs, and the exit code. A third miss goes to `needs-human`
with the attempts.

`## Acceptance criteria` holds one falsifiable line per criterion, in this
grammar.

- AC-<n> · <falsifiable statement> · check: <target>

The target is `<feature> drive`, `<feature> fast`, or `measure <id>`. The
drive target asks for a full drive of the feature. The fast target runs the
fast command of the feature file. The measure target runs a measurer of
`theory/verify.toml`. When the `{skills}` block is empty, no run skill
exists yet. Use `measure <id>` when the theory map has a measurer.
Otherwise name the target `<feature> fast`, and write `new: <feature>` in
the Fast column of the chunk that adds the feature file. These lines show
the shape.

- AC-1 · An empty card field blocks submit and shows "Card is required" · check: checkout-submit drive
- AC-2 · POST /orders with no card returns 422 and `{"error":"card_required"}` · check: api-orders fast
- AC-3 · poll_p95 does not worsen · check: measure poll_p95

A criterion names the condition, the observable result, and the check. A
criterion that no command can falsify is not a criterion. Rewrite it, or
take it to a human decision when no experiment can settle it. A criterion
that the ticket text does not ask for is scope creep. Drop it.

# The implementation plan

The implementation plan must use this table:

| Chunk | Goal | Owned files or paths | Depends on | Validation | Fast | Wave |
|---|---|---|---|---|---|---|

The Fast column names the fast command that proves the chunk, or
`new: <feature>` when the chunk must add a feature file.

Create separate chunks only when the split reduces delivery time. Make each
chunk large enough to justify coordination. Put independent chunks in the same
wave only when they have no dependency and do not edit the same files. Put at
most three chunks in one wave. Assign shared files and final integration to one
coordinator chunk. The coordinator chunk is the last chunk, and it is alone in
the last wave. Put a shared interface or data contract before chunks that
depend on it. State the final integration order and final validation. For a
small or tightly coupled change, use one C1 row and state that parallel work
would add delay.

# Labels

A label must exist before you use it. Create each label you need, and ignore
the error that reports an existing label:

`gh label create <name> --color <hex> --description <text> 2>/dev/null || true`

Give two complexity labels to every ticket that an agent implements. The
factory reads them to select the model of the implement stage and of the
review stage. The scale is `low`, `medium`, `high`, and `very-high`. The
implementation label is `complexity:<level>`. The review label is
`review-complexity:<level>`. Rate the size and the risk of that one ticket,
not of the whole feature. An absent label means `medium`, so state the level
even when you choose medium.

# One chunk

When the plan holds one chunk, the ticket stays one ticket. Run
`gh issue edit {number} --remove-label to-refine --add-label refined` and add
the two complexity labels in the same command. Report one line that says the
ticket is refined.

# Several chunks

When the plan holds two or more chunks, create one sub-ticket for each chunk
with `gh issue create`. Ticket #{number} becomes the parent. The parent holds
the shared specification. No agent implements the parent.

Create the sub-tickets in wave order. Then you know the number of every
earlier chunk when you write a dependency.

The body of a sub-ticket must hold these sections as `##` headings:

- A `Parent: #{number}` line before the headings
- `## Problem`
- `## Grounding`
- `## Decisions`
- `## Repro`, when the parent carries the `bug` label
- `## Acceptance criteria`
- `## Implementation plan`, as the table above, with one C1 row for this chunk
- The `Owned files or paths` column of the plan table
- `## Validation`

A sub-ticket must stand alone. Copy every fact the chunk needs from the
parent. The agent that implements the chunk reads the sub-ticket only.

When a chunk depends on an earlier chunk, add one line `Blocked by #A and #B`
to the body of the sub-ticket. Name every earlier chunk it needs. The factory
holds the sub-ticket until those tickets close.

Give each sub-ticket the `refined` label, the `chunk` label, one
`complexity:<level>` label, and one `review-complexity:<level>` label. Never
give a sub-ticket the `to-refine` label.

The last sub-ticket is the coordinator. Add this line to its body:

- Final chunk. Also close the parent: add a second `Closes #{number}` line to
  the PR body.

Then edit the parent:

- Add a `## Chunks` section. Write one task list line for each sub-ticket, in
  the form `- [ ] #A short goal`.
- Add a `## Definition of done` section. State that the parent closes when the
  PR of the final chunk merges.
- Run `gh issue edit {number} --remove-label to-refine --add-label epic`.

Never give the parent the `refined` label. That label starts a second
implementation of work the sub-tickets already own, and it leaves the parent
open forever.

Report one line that says the ticket is refined, and name the sub-tickets.

# A second run on the same parent

A parent carries the `epic` label and a `## Chunks` section. When you refine
such a ticket again, do not create the sub-tickets a second time. Read the
sub-tickets the section names. Update the body and the labels of each open
one. Create a sub-ticket only for a chunk that has none. Close a sub-ticket
that the new plan drops, and state the reason in a comment.

# A human decision

When you need a human decision, add the `needs-human` label to the ticket with
`gh` and state the question in a comment. Stop after the label is on. When the
decision is a choice between named answers, end the comment with one strict
block in this form. Keep the JSON on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Ticket #{number}, {title}

{body}
"#;

/// The built-in prompt of an implement run.
pub const IMPLEMENT_PROMPT: &str = r#"You implement ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

Before any other step, read the newest comments of the ticket with `gh`. An
operator answer to a question from an earlier run arrives there. Such an
answer settles the question. Act on it, and never ask that question again.

Your goal is a complete change that meets every acceptance criterion with the
shortest safe delivery time. Follow the repository instructions and keep the
requested scope. Implement the ticket on the current branch.

# The theory slices

Read the two blocks below before your first edit. They carry the theory model
and the run skills of the areas the ticket touches, and they are empty when
the governor is off.

{model}

{skills}

# The simplest change

Read the conventions of every file you touch, before you edit it. Follow
them. Make the smallest change that meets every acceptance criterion. Add no
abstraction, layer, flag, or dependency that no criterion needs. Delete the
dead weight you meet, in its own commit. Before each commit, strip your
narrating comments, every guard no criterion asks for, and every edit outside
the plan.

# The execution plan

Use the ticket implementation plan as the execution schedule. If routine code
details make the plan stale, update the schedule and continue. If the ticket
has no plan, make the smallest useful schedule before edits.

For each execution wave, start ready chunks concurrently when they are
sizeable, independent, and have separate file ownership. If subagents are
available, start all agents for that wave in one tool turn. Use at most three
subagents at once. Work directly for a small, sequential, single-file, or
tightly coupled change.

Give each subagent the ticket goal, chunk identifier, exact owned paths,
satisfied dependencies, acceptance criteria, and validation command. Tell each
subagent to stay in this worktree, edit only its owned paths, and avoid all git
and `gh` writes. A subagent must not start another subagent. Never
give two concurrent writers the same file. Do not duplicate delegated work.

If subagents are unavailable, execute the chunks directly in dependency order.

After each wave, inspect every owned path and the combined diff. Treat missing
or empty subagent output as a failed chunk. Complete or repair failed work before
the next dependent wave. The coordinator owns shared files, integration, git
operations, and GitHub operations.

Run focused validation after each chunk. Run the required full validation once
after integration. Do not run several full test suites concurrently. Make the
test suite pass. Commit the integrated work in small, complete commits.

# The proof

Drive every feature you touched once through its run skill, before you open
the PR. Write one Before / After line per acceptance criterion, from what you
observed. Write no line you did not observe. Run every fast command of those
features and paste its exit code under the line.

# The lever rule

When you check the same fact by hand twice, or write a throwaway script to
check it, add that script to the run skill in the same PR. Give it one
invocation line in `SKILL.md`, or name it as the `fast` command of a feature
file. Commit the lever on its own. A reviewer must be able to rerun it. A
one-off `grep`, a line of shell history, and a test that passes when every
dependency returns nothing are not levers.

# Tests

Every test you add asserts a literal result through the public path of the
change. Every test you add fails with the change reverted. A test that still
passes with the change reverted proves nothing. Delete it, or rewrite it.

# The PR body

The body holds `## Why`, `## Before / After`, and `## Blast radius`, and
nothing else.

`## Why` holds one or two short paragraphs. Name the behaviour that changes,
and for whom. `## Blast radius` holds one to three sentences. Name what else
the change touches, and why it is safe.

`## Before / After` holds one line per acceptance criterion. Two surfaces
prove one criterion with two lines. The grammar is this line.

- AC-<n> · <feature or measurer> · <tier or measure> · <command> · before: <observed> · after: <observed>

The separator is one middle dot with one space on each side. The tier is
`browser`, `dom`, `http`, `terminal`, `none`, or `measure` for a measurer
line, and it names how far your drive reached. These two lines show the
shape.

- AC-1 · checkout-submit · browser · `npx playwright test checkout` · before: an empty card is accepted and the API returns 500 · after: the field shows "Card is required" and the page sends no request
- AC-2 · api-orders · http · `curl -s -X POST :4000/orders -d @empty.json` · before: 500 and the log line `NullPointer at Orders.create` · after: 422 and the body `{"error":"card_required"}`

Write short declarative sentences, one thought per sentence, in the active
voice. Narrate no work. The body carries no `## Summary` section and no
`## Test plan` section. It carries at most 40 lines of prose outside the
fenced blocks. It carries no long dash, no curly quote, and no colon inside a
sentence. A transcript goes in a fenced block under its own line, cut to 30
lines, and the full file stays in `.aif/evidence/` in this worktree.

Every path your commits change belongs to an owned path of the plan table, to
a run skill directory, or to a test file. A dependency manifest changes only
when the ticket names the dependency. The factory reads this body, refuses a
body that breaks one of these rules, and sends the ticket back to you.

# The PR

Open a draft PR with `gh pr create --draft` when the work is done. Put
`Closes #{number}` in the body. When the ticket body names a parent ticket and
marks this ticket as the final chunk, add a second `Closes` line for the parent
number, so the merge closes the parent too. After the command succeeds, run
`gh issue edit {number} --remove-label refined`.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to ticket #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess. When the decision is a choice between
named answers, end the comment with one strict block in this form. Keep the JSON
on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Report one line at the end. Name what you did, and the PR number.

Ticket #{number}, {title}

{body}
"#;

/// The built-in prompt of a review run.
pub const REVIEW_PROMPT: &str = r#"You review PR #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Except for
the base worktree this prompt names, never create another git worktree; work
only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

Before any other step, read the newest comments of the PR with `gh`. An
operator answer to a question from an earlier run arrives there. Such an
answer settles the question. Act on it, and never ask that question again.

PR #{number}, {title}

{body}

The tickets this PR closes are {tickets}

You are the last agent on this change. You repair every finding yourself. You
never hand a finding back to the author. The PR must leave your run ready for
review, or labelled `needs-human`.

Read the diff of the PR with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Read the repository
instructions and the linked tickets.

Before your first edit, check whether the PR comes from a fork:
`gh pr view {number} --json isCrossRepository --jq .isCrossRepository`.
When the command prints `true`, take the human path. Never push a fork repair
to `origin`.

Before your first edit, prove that this worktree holds the PR head. Compare
`gh pr view {number} --json headRefOid --jq .headRefOid` with
`git rev-parse HEAD`. When the two differ, run
`git fetch origin pull/{number}/head` and then `git reset --hard FETCH_HEAD`.

# The theory slices

The two blocks below carry the theory model and the run skills of the areas
this diff touches, and they are empty when the governor is off.

{model}

{skills}

# The prediction

The block below holds what the operator predicted this change would touch. It
reads `none` when the ticket carries no prediction. Compare the prediction
with what the diff did. Name every area the diff touched that the prediction
left out. Name every predicted area the diff never reached. Put both in your
record comment.

{prediction}

End your report with one `<aif-delta-v1>` block when the prediction above is
not `none`. Put the block last, on its own lines, and never inside a code
fence. Its body is one JSON object.

<aif-delta-v1>
{"slots":[{"id":"behaviours","outcome":"hit"},{"id":"states","outcome":"hit"},{"id":"invariants","outcome":"miss"},{"id":"failure-modes","outcome":"hit"},{"id":"other-areas","outcome":"hit"}],"touched":["INV-3"],"violations":[{"entry":"INV-3","finding":"the retry crosses the boundary"}],"question":"Does the cart keep the token?"}
</aif-delta-v1>

Write one slot per prediction slot. The five slot ids are `behaviours`,
`states`, `invariants`, `failure-modes`, and `other-areas`. The outcome is
`hit` when the change stayed inside the entries the slot named, and `miss`
when the change reached past them. A path that maps to an area the
prediction left out is a miss on `other-areas`. List in `touched` every model
entry id the change reached. Add one violation per model rule the change
broke. Ask the operator one question. A review whose prediction reads `none`
ends with no block.

# The base worktree

Create the base worktree once, and only when the re-drive below asks for it.
Run `git worktree add --detach .aif/base $(git merge-base origin/main HEAD)`
inside this worktree. Read the default branch of the repository first, and
use its name in place of `main`. The path `.aif/` never reaches a commit.
Remove the base worktree with `git worktree remove --force .aif/base` before
you push.

# The re-drive

Trust no Before / After line until you drive it yourself.

1. Run the command of every Before / After line on the head. Compare what you
   observe with the after text of the line.
2. For a ticket with the `bug` label, run the command of the `## Repro`
   section in the base worktree, and expect the before text. Then run it on
   the head, and expect the after text. Red on base, green on head.
3. Run every lever the PR adds, and expect the exit code the PR states.
4. Run every test the PR adds against the base worktree, with
   only the test files applied. Each one must fail there. A test that passes
   on base tests nothing. Delete it, or rewrite it, and say so.
5. Read the diff once for what it does not need. An abstraction, a layer, a
   flag, a guard, or a dependency that no criterion needs is a finding.
   Remove it. A deviation from the conventions of the surrounding files is a
   finding. Align it.
6. Repair every run skill drift you meet during a drive. A dead path or a
   dead handle in a `SKILL.md` or a feature file is a finding of this run.

A mismatch is a finding. Repair the code, or repair the check when the check
was the defect, then drive the full list again. A bug that does not fail in
the base worktree is a wrong root cause. Take the human path with both
outputs.

# The repairs

Fix every finding in this worktree. Add the missing tests. Keep the scope of
the linked tickets. Run the full validation of the repository and make it
pass. Commit the repairs in small, complete commits.

Push once, at the end of the run. A push on a draft PR can restart your own
review, so never push a partial fix. Push the commits and open the release
gate in one command line:

`git push origin HEAD:$(gh pr view {number} --json headRefName --jq .headRefName) && gh pr ready {number}`

Never pass `--force`. Never merge the PR.

# The record

Record the outcome with `gh pr comment {number}`. Write your own Before /
After lines, in the shape the PR body uses, from what you observed on your
own run. Name the findings, the repairs, and the validation result. GitHub
refuses a formal review of your own PR, so this comment is the record.

When the PR needs no repair, post the record and run `gh pr ready {number}`.

Take the human path when the PR comes from a fork, when a finding needs a human
decision, when the repair leaves the scope of the linked tickets, when a bug
does not reproduce in the base worktree, or when the push fails. On that path,
add the `needs-human` label to the PR with `gh`, write the question into a
comment, leave the draft, and stop. Do not guess. When the decision is a choice
between named answers, end the comment with one strict block in this form. Keep
the JSON on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Report one line at the end. Name the review verdict, and the number of commits
you pushed.
"#;

/// The built-in prompt of a release run.
pub const RELEASE_PROMPT: &str = r#"You release the stacked PRs of {repo}
({owner_repo}). You work in {worktree}, the release worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

The batch holds {pr_count} PR(s), in merge order:

{pr_list}

Merge every PR in the listed order with `gh pr merge`, one at a
time. Merge order is {pr_numbers}. After each merge, pull the base branch
into this worktree so the next merge sees the updated state. If a merge
conflicts, stop, and report the PR number that failed.

When all merges are done, report one line: the released PRs.
"#;

/// The built-in prompt of a ticket-creation session.
pub const TICKET_PROMPT: &str = r#"You help the operator create one ticket in the
repository {repo} ({owner_repo}). You work in {worktree}, the repository
checkout. Never create a git worktree; stay in this checkout.

Ask the operator what the ticket should say, in short questions, one topic at
a time. When you know enough, draft the title and body, show them, and on
approval create the ticket with `gh issue create`. Report the new ticket
number.

If the operator asks for something you cannot decide alone, say so plainly
and ask again.
"#;

/// The built-in prompt of a ticket conversation.
pub const TICKET_CHAT_PROMPT: &str = r#"You review ticket #{number} in repository
{repo} ({owner_repo}). The repository checkout is {worktree}.

Ticket title: {title}
Ticket description:
{body}

Labels: {labels}
Author: {author}
Assignees: {assignees}
Updated: {updated_at}
GitHub reference: {github_url}

Start with analysis. Do not propose a title or description change unless the
operator explicitly requests that change.

When the operator explicitly requests a title or description change, finish
the assistant turn with exactly one complete block in this form:

<aif-ticket-proposal-v1>
{"title":"New title","body":"New description"}
</aif-ticket-proposal-v1>

Put valid JSON between the markers. Do not quote the block. Do not put the
block in a code fence. Include no text after the closing marker.
"#;

/// The built-in prompt of one teach task.
///
/// The theory roles carry no prompt file, so a teach task fills this
/// template directly. The placeholders are `{repo}`, `{worktree}`,
/// `{subject}`, `{history}`, `{model}`, and `{skills}`.
pub const TEACH_PROMPT: &str = r#"You explain one subject of the repository {repo}
to the operator. You work in {worktree}, the repository checkout. Read the
files you need. Change no file.

The subject

{subject}

The history

{history}

The model

{model}

The skills

{skills}

Explain how the subject works and why it works that way. Use the model, the
skills, and the history. Give the smallest complete answer first. Then add
one layer at a time. Build the picture in steps, one part per step.

Keep the confidence of what you found. Name the history when the history
shows a fact. Say that you infer a step when you reason it out. Print no
framing label. Ask the operator no question. Set no quiz.

End the turn with one block per contradiction you find between the code and
the model. A subject that matches the model ends with no block. Each block
takes this form.

<aif-event-v1>
{"kind":"teach","text":"One sentence on the contradiction.","area":"area id"}
</aif-event-v1>

Put valid JSON between the markers. Do not quote a block. Do not put a block
in a code fence. Write no text after the last closing marker.
"#;

/// The body of one run skill ticket, before the daemon fills it.
///
/// The eight steps are section 6.2 of the verification toolbelt design
/// record. The daemon fills `{alias}`, `{surface}`, `{skills_dir}`, and
/// `{app_path}` when it creates the ticket, so the refine agent and the
/// implement agent read one complete recipe.
pub const SETUP_BODY: &str = r#"Write the run skill of the {surface} surface of {alias}.

The application lives at {app_path}. Write every skill file under
{skills_dir}. Never edit `theory/verify.toml`.

1. Interview the repository, not the operator. Find the surface, the run
command, the drivers that are already installed, the evidence a run leaves
behind, and how a run isolates its state.
2. Probe the driver ladder. Pick the highest tier that is already present.
Install nothing.
3. Fix a checkout that does not start. Report the failure precisely when you
cannot fix it. Do this before you write any skill file.
4. Write `SKILL.md` with the front matter keys `name`, `description`,
`surface`, `driver`, `tier`, and `blind`, and with the sections Run, Fast,
Auth or seed, Drive, Logs, and Gotchas. Measure the Fast path and record how
long it takes. Under Claude Code the bundled `run-skill-generator` skill
writes the first draft.
5. Seed the `features/` directory from `theory/verify.toml`. Write the index
first. Then write one file per area that maps to this surface and needs a
drive recipe. Write the top three to five areas only.
6. Prove it once end to end. Run the application, run the fast command, drive
one feature, read the logs, and stop the application. Confirm that the
evidence file still exists.
7. Write the PR body to the contract of the implement prompt. The Before and
After line of a setup PR states `before: no run skill`. Its after text is
what step 6 proved.
8. Propose the `skills` entry of `theory/verify.toml` as text in the PR body.
Never edit `theory/verify.toml`.
"#;

/// The built-in prompt of one audit sweep task.
///
/// The theory roles carry no prompt file, so a sweep task fills this
/// template directly. The placeholders are `{repo}`, `{worktree}`,
/// `{model}`, and `{skills}`.
pub const AUDIT_SWEEP_PROMPT: &str = r#"You audit the repository {repo}
against the model. You work in {worktree}, the repository checkout. Read
the files you need. Change no file.

The model

{model}

The skills

{skills}

Check every entry of the model against the code. An entry that no code
supports is dead. End the turn with one event block per dead entry.

Then check each run skill against the code. Read the Run and Fast sections
of every SKILL.md file. Read the handles of every feature file. A command
that names a path that the code no longer holds is a dead path. A handle
that no code answers is a dead handle. End the turn with one skill-drift
block per surface with drift. Name the dead paths and the dead handles in
the text of the block.

A model event block takes this form.

<aif-event-v1>
{"kind":"sweep","text":"One sentence on the dead entry.","area":"area id"}
</aif-event-v1>

A drift block names its surface and takes this form.

<aif-event-v1>
{"kind":"skill-drift","text":"The dead paths and the dead handles.","surface":"surface id"}
</aif-event-v1>

Put valid JSON between the markers. Do not quote a block. Do not put a
block in a code fence. Write no text after the last closing marker.
"#;

/// The built-in prompt of one card grading.
///
/// The audit role reads the card, the answer of the operator, and the
/// subject the card names: the diff of the merged pull request, or the
/// model entry. It grades the answer against the subject and ends the
/// turn with one event block per gap. A correct answer ends with no
/// block, so a pass opens no theory event.
pub const AUDIT_CARD_PROMPT: &str = r#"You grade one card answer of the operator
of the repository {repo}. You work in {worktree}, the repository checkout.
Read the files you need. Change no file.

The card

{card}

The answer of the operator

{answer}

The subject

{subject}

Compare the answer against the subject. A claim the subject contradicts is a
gap. A part of the subject the answer misses is a gap. A claim the subject
supports is a hit.

End the turn with one block per gap. An answer with no gap ends with no
block. Each block takes this form.

<aif-event-v1>
{"kind":"card","text":"One sentence on the gap.","area":"area id"}
</aif-event-v1>

Put valid JSON between the markers. Do not quote a block. Do not put a block
in a code fence. Write no text after the last closing marker.
"#;

/// The built-in prompt of one bootstrap chat.
///
/// The operator dictates a stream of memory about one area, and the agent
/// turns it into model entries. The agent adds no claim the operator did
/// not state, because a model the operator did not write is not the
/// operator's theory. The placeholders are `{repo}`, `{worktree}`,
/// `{area}`, and `{model}`.
pub const BOOTSTRAP_PROMPT: &str = r#"You write the model of the area {area}
in the repository {repo} with the operator. You work in {worktree}, the
theory checkout. Read the files you need. Change no file.

The model so far

{model}

The operator dictates a stream of memory about the area. The operator is the
only source. Add no claim the operator did not state. Take no claim from the
code. Read the code only to name a path or a file the operator points at.

Ask short questions. Ask one question per turn. Ask only what an entry needs.
An entry needs an id, a kind, a title, and a statement. Say back what you
understood in one short sentence.

An entry takes one of five kinds. Each kind takes its own keys.

- state names one region of the system. It takes no other key.
- boundary names the two regions it separates in sides, and the path globs
  that cross it in paths.
- transition names the state it leaves in from, and the state it reaches
  in to.
- invariant names a claim that always holds, and the states or the
  boundaries it holds over in constrains.
- failure names one way the system breaks, and the boundary it breaks
  through in crosses.

Give each entry a short id the operator recognises. Reuse no id the model so
far already holds.

The operator ends the interview with the word done. End that turn with one
block that carries every entry you collected. Write no text after it. A turn
the operator did not end takes no block.

<aif-model-proposal-v1>
{"entries":[{"kind":"state","id":"checkout","title":"Checkout","statement":"The buyer pays."}]}
</aif-model-proposal-v1>

Put valid JSON between the markers. Do not quote a block. Do not put a block
in a code fence. Write no text after the last closing marker.
"#;

/// The body of one run skill maintain ticket, before the daemon fills it.
///
/// The eight steps are section 6.3 of the verification toolbelt design
/// record. The daemon fills `{alias}`, `{surface}`, `{skills_dir}`, and
/// `{app_path}` when it creates the ticket, so the refine agent and the
/// implement agent read one complete recipe.
pub const MAINTAIN_BODY: &str = r#"Maintain the run skill of the {surface} surface of {alias}.

The audit sweep found drift between the skill and the code. The application
lives at {app_path}. Every skill file lives under {skills_dir}. Never edit
`theory/verify.toml`.

1. Clean the feature index. Remove an entry that no file or area supports.
2. Read every source path that the Run and Fast sections name. Fix a path
that the code moved.
3. Run the Run section and the Fast section once each. Fix a command that
fails against the live application.
4. Triage every finding. A finding is doc drift, or a harness gap, or a
product gap.
5. Open one `bug` ticket per product gap. Fix no product code in this
ticket.
6. Re-drive every fix against the live application.
7. Ship the repair as one PR. Post a comment instead when the skill needs
no change.
8. Stop when the skill matches the code again.
"#;

/// The placeholders the daemon fills in a stage prompt.
const STAGE_PLACEHOLDERS: &[&str] = &[
    "repo",
    "owner_repo",
    "number",
    "title",
    "body",
    "worktree",
    "tickets",
    "pr_list",
    "pr_numbers",
    "pr_count",
    // The theory placeholders. `prediction`, `comparison`, and `rules`
    // render empty until their chunks land; `why_rule` fills for a
    // shadow-mode repository only.
    "model",
    "prediction",
    "comparison",
    "skills",
    "rules",
    "why_rule",
    // The last ticket check finding, for a refine that follows one.
    "finding",
];

/// The placeholders the daemon fills in the ticket-creation prompt.
const TICKET_PLACEHOLDERS: &[&str] = &["repo", "owner_repo", "worktree"];

/// The placeholders the daemon fills in the ticket chat prompt.
const TICKET_CHAT_PLACEHOLDERS: &[&str] = &[
    "repo",
    "owner_repo",
    "number",
    "title",
    "body",
    "labels",
    "author",
    "assignees",
    "updated_at",
    "github_url",
    "worktree",
];

/// Every role the daemon renders a prompt for, in role order.
///
/// The theory roles are absent. `theory.audit` and `theory.chat` carry no
/// template yet, and each will take one template per task purpose, not one
/// per role. A prompt file for them would sit unread, so the daemon
/// publishes no prompt view for them and the Settings view shows no prompt
/// row on them.
pub const ROLES: [ExecutionRole; 6] = [
    ExecutionRole::Refine,
    ExecutionRole::Implement,
    ExecutionRole::Review,
    ExecutionRole::Release,
    ExecutionRole::TicketCreate,
    ExecutionRole::TicketChat,
];

/// The file name of the prompt template of one role, inside the prompts
/// directory, or `None` for a role with no template.
pub const fn file_name(role: ExecutionRole) -> Option<&'static str> {
    match role {
        ExecutionRole::Refine => Some("refine.md"),
        ExecutionRole::Implement => Some("implement.md"),
        ExecutionRole::Review => Some("review.md"),
        ExecutionRole::Release => Some("release.md"),
        ExecutionRole::TicketCreate => Some("ticket.md"),
        ExecutionRole::TicketChat => Some("ticket-chat.md"),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The built-in template of one role, or `None` for a role with no
/// template.
pub const fn builtin(role: ExecutionRole) -> Option<&'static str> {
    match role {
        ExecutionRole::Refine => Some(REFINE_PROMPT),
        ExecutionRole::Implement => Some(IMPLEMENT_PROMPT),
        ExecutionRole::Review => Some(REVIEW_PROMPT),
        ExecutionRole::Release => Some(RELEASE_PROMPT),
        ExecutionRole::TicketCreate => Some(TICKET_PROMPT),
        ExecutionRole::TicketChat => Some(TICKET_CHAT_PROMPT),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The placeholders the daemon fills for one role, or `None` for a role
/// with no template.
///
/// A template may use any subset of them. A placeholder outside the set is
/// an error, both at save time and at dispatch time.
pub const fn placeholders(role: ExecutionRole) -> Option<&'static [&'static str]> {
    match role {
        ExecutionRole::Refine
        | ExecutionRole::Implement
        | ExecutionRole::Review
        | ExecutionRole::Release => Some(STAGE_PLACEHOLDERS),
        ExecutionRole::TicketCreate => Some(TICKET_PLACEHOLDERS),
        ExecutionRole::TicketChat => Some(TICKET_CHAT_PLACEHOLDERS),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The path of the prompt file of one role, or an error naming a role with
/// no template.
pub fn path(prompts_dir: &Path, role: ExecutionRole) -> Result<PathBuf> {
    Ok(prompts_dir.join(name_of(role)?))
}

/// The file name of one role, or an error that names the role.
fn name_of(role: ExecutionRole) -> Result<&'static str> {
    file_name(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))
}

/// One loaded template and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The template text.
    pub text: String,
    /// True when the text came from the prompt file. False for the built-in.
    pub from_file: bool,
}

/// Read the template file `name` in the prompts directory.
///
/// The file wins when it exists. An absent file yields the `builtin` text.
/// An unreadable file is an error that names the path.
pub fn load_named(prompts_dir: &Path, name: &str, builtin: &str) -> Result<Template> {
    let path = prompts_dir.join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Template {
            text,
            from_file: true,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Template {
            text: builtin.to_string(),
            from_file: false,
        }),
        Err(error) => Err(anyhow!("cannot read {}: {error}", path.display())),
    }
}

/// Read the template of one role.
///
/// The prompt file wins when it exists. An absent file yields the built-in.
/// An unreadable file is an error that names the path. A role with no
/// template is an error that names the role.
pub fn load(prompts_dir: &Path, role: ExecutionRole) -> Result<Template> {
    let builtin = builtin(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))?;
    load_named(prompts_dir, name_of(role)?, builtin)
}

/// Check one template against the placeholder set of its role.
///
/// The error names the first unknown placeholder and lists the known ones.
/// A blank template is an error too: the agent would start with no
/// instructions. A role with no template is an error that names the role.
pub fn check(role: ExecutionRole, text: &str) -> Result<()> {
    let allowed =
        placeholders(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))?;
    if text.trim().is_empty() {
        bail!("the prompt is empty");
    }
    if let Some(token) = scan_placeholders(text)
        .into_iter()
        .find(|token| !allowed.contains(token))
    {
        let known = allowed
            .iter()
            .map(|name| format!("{{{name}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the prompt uses the unknown placeholder {{{token}}}; the {role} prompt knows {known}"
        );
    }
    Ok(())
}

/// Write the prompt file of one role.
///
/// The text goes to a sibling temporary file first, and one rename replaces
/// the destination, so a reader never sees a half-written prompt. A role
/// with no template is an error that names the role.
pub fn save(prompts_dir: &Path, role: ExecutionRole, text: &str) -> Result<()> {
    let name = name_of(role)?;
    fs::create_dir_all(prompts_dir)
        .with_context(|| format!("cannot create {}", prompts_dir.display()))?;
    let destination = prompts_dir.join(name);
    let temporary = prompts_dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = write_all_synced(&temporary, text)
        .with_context(|| format!("cannot write {}", temporary.display()))
        .and_then(|()| {
            fs::rename(&temporary, &destination).with_context(|| {
                format!(
                    "cannot rename {} to {}",
                    temporary.display(),
                    destination.display()
                )
            })
        });
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Write one file and flush it to the storage device.
///
/// The flush matters because a rename follows: without it a crash can
/// leave the renamed destination empty, and an empty prompt would start an
/// agent with no instructions.
fn write_all_synced(path: &Path, text: &str) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Remove the prompt file of one role, so the built-in template applies.
///
/// An absent file is not an error. A role with no template is an error that
/// names the role.
pub fn reset(prompts_dir: &Path, role: ExecutionRole) -> Result<()> {
    let path = path(prompts_dir, role)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow!("cannot remove {}: {error}", path.display())),
    }
}

/// Fill a prompt template.
///
/// Every placeholder must be known; an unknown one is an error that names
/// it, never a silent literal. A filled value stays literal: a `{body}`
/// inside a ticket title is not filled again.
pub fn fill_template(template: &str, values: &[(&str, String)]) -> Result<String> {
    for token in scan_placeholders(template) {
        if !values.iter().any(|(name, _)| *name == token) {
            bail!("the prompt template uses the unknown placeholder {{{token}}}");
        }
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((start, end, token)) = next_span(rest) {
        out.push_str(&rest[..start]);
        match values.iter().find(|(name, _)| *name == token) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// List the `{placeholder}` tokens of a template, in first-seen order.
///
/// A token is placeholder-shaped when it holds only ASCII letters, digits,
/// underscores, and hyphens. Other brace content stays untouched.
///
/// This walk and the one in [`fill_template`] share [`next_span`], so a
/// template that passes [`check`] always fills.
pub fn scan_placeholders(template: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some((_, end, token)) = next_span(rest) {
        if placeholder_shaped(token) && !found.contains(&token) {
            found.push(token);
        }
        rest = &rest[end..];
    }
    found
}

/// The next `{...}` span of `text`: the start byte, the byte after the
/// closing brace, and the text between the braces.
///
/// A `{` inside a span means the true opener is the later one, so
/// `{ and {number}` holds the span `{number}`. One stray brace therefore
/// never hides the placeholder behind it. Every index sits on an ASCII
/// brace, so every slice keeps a character boundary.
fn next_span(text: &str) -> Option<(usize, usize, &str)> {
    let mut from = 0;
    loop {
        let open = from + text[from..].find('{')?;
        let after = &text[open + 1..];
        let close = after.find('}')?;
        match after[..close].rfind('{') {
            Some(inner) => from = open + 1 + inner,
            None => return Some((open, open + close + 2, &after[..close])),
        }
    }
}

/// True when a span between braces may name a placeholder.
fn placeholder_shaped(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty prompts directory under the system temporary root.
    fn temp_prompts_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-prompts-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_role_has_one_file_name_and_its_builtin_template() {
        let names = ROLES.map(|role| file_name(role).expect("a listed role names a file"));
        for (index, name) in names.iter().enumerate() {
            assert!(name.ends_with(".md"), "{name} is not a markdown file");
            assert!(
                !names[..index].contains(name),
                "{name} is the file name of two roles"
            );
        }
        assert_eq!(file_name(ExecutionRole::TicketCreate), Some("ticket.md"));
        assert_eq!(file_name(ExecutionRole::TicketChat), Some("ticket-chat.md"));
        assert_eq!(builtin(ExecutionRole::Refine), Some(REFINE_PROMPT));
        assert_eq!(builtin(ExecutionRole::TicketChat), Some(TICKET_CHAT_PROMPT));
    }

    #[test]
    fn every_builtin_template_passes_the_check_of_its_role() {
        for role in ROLES {
            let text = builtin(role).expect("a listed role has a built-in template");
            check(role, text).unwrap_or_else(|error| {
                panic!("the built-in {role} prompt fails its own check: {error:#}")
            });
            let allowed = placeholders(role).expect("a listed role has a placeholder set");
            for token in scan_placeholders(text) {
                assert!(
                    allowed.contains(&token),
                    "the built-in {role} prompt uses {{{token}}} outside its placeholder set"
                );
            }
            // Every literal `{word}` of the text must reach the scan. A
            // stray brace that hid one would leave it unfilled at dispatch.
            for literal in text.split('{').skip(1).filter_map(|rest| {
                rest.find('}')
                    .map(|end| &rest[..end])
                    .filter(|token| placeholder_shaped(token))
            }) {
                assert!(
                    scan_placeholders(text).contains(&literal),
                    "the built-in {role} prompt hides {{{literal}}} from the scan"
                );
            }
        }
    }

    #[test]
    fn the_teach_prompt_names_exactly_its_six_placeholders() {
        assert_eq!(
            scan_placeholders(TEACH_PROMPT),
            vec!["repo", "worktree", "subject", "history", "model", "skills"]
        );
        let values: Vec<(&str, String)> = scan_placeholders(TEACH_PROMPT)
            .into_iter()
            .map(|name| (name, format!("<{name}>")))
            .collect();
        let filled = fill_template(TEACH_PROMPT, &values).expect("the teach prompt fills");
        assert!(filled.contains("<subject>"));
        assert!(filled.contains("<history>"));
        assert!(
            filled.contains(
                r#"{"kind":"teach","text":"One sentence on the contradiction.","area":"area id"}"#
            ),
            "the event block stays literal:\n{filled}"
        );
    }

    #[test]
    fn the_bootstrap_prompt_names_exactly_its_four_placeholders() {
        assert_eq!(
            scan_placeholders(BOOTSTRAP_PROMPT),
            vec!["area", "repo", "worktree", "model"]
        );
        let values: Vec<(&str, String)> = scan_placeholders(BOOTSTRAP_PROMPT)
            .into_iter()
            .map(|name| (name, format!("<{name}>")))
            .collect();
        let filled = fill_template(BOOTSTRAP_PROMPT, &values).expect("the bootstrap prompt fills");
        assert!(filled.contains("<area>"));
        assert!(filled.contains("<model>"));
        assert!(
            filled.contains(
                r#"{"entries":[{"kind":"state","id":"checkout","title":"Checkout","statement":"The buyer pays."}]}"#
            ),
            "the proposal block stays literal:\n{filled}"
        );
        assert!(
            filled.contains(crate::theory::records::MODEL_PROPOSAL_BLOCK),
            "the prompt names the block tag the daemon parses:\n{filled}"
        );
        for key in ["sides", "paths", "constrains", "from", "to", "crosses"] {
            assert!(
                filled.contains(&format!(" in {key}")),
                "the prompt names the required key {key}:\n{filled}"
            );
        }
    }

    #[test]
    fn the_audit_sweep_prompt_names_exactly_its_four_placeholders() {
        assert_eq!(
            scan_placeholders(AUDIT_SWEEP_PROMPT),
            vec!["repo", "worktree", "model", "skills"]
        );
        assert!(
            AUDIT_SWEEP_PROMPT.contains("dead handles"),
            "the drift paragraph names the dead handles"
        );
        assert!(
            AUDIT_SWEEP_PROMPT.contains("skill-drift"),
            "the drift block carries the skill-drift kind"
        );
        let values: Vec<(&str, String)> = scan_placeholders(AUDIT_SWEEP_PROMPT)
            .into_iter()
            .map(|name| (name, format!("<{name}>")))
            .collect();
        let filled = fill_template(AUDIT_SWEEP_PROMPT, &values).expect("the sweep prompt fills");
        assert!(filled.contains("<model>"));
        assert!(filled.contains(
            r#"{"kind":"skill-drift","text":"The dead paths and the dead handles.","surface":"surface id"}"#
        ), "the drift block stays literal:\n{filled}");
    }

    #[test]
    fn the_audit_card_prompt_names_exactly_its_five_placeholders() {
        assert_eq!(
            scan_placeholders(AUDIT_CARD_PROMPT),
            vec!["repo", "worktree", "card", "answer", "subject"]
        );
        let values: Vec<(&str, String)> = scan_placeholders(AUDIT_CARD_PROMPT)
            .into_iter()
            .map(|name| (name, format!("<{name}>")))
            .collect();
        let filled = fill_template(AUDIT_CARD_PROMPT, &values).expect("the card prompt fills");
        assert!(filled.contains("<card>"));
        assert!(filled.contains("<answer>"));
        assert!(filled.contains("<subject>"));
        assert!(
            filled
                .contains(r#"{"kind":"card","text":"One sentence on the gap.","area":"area id"}"#),
            "the card block stays literal:\n{filled}"
        );
        assert!(
            filled.contains("An answer with no gap ends with no\nblock."),
            "the prompt says a pass opens no event:\n{filled}"
        );
    }

    #[test]
    fn the_maintain_body_fills_like_the_setup_body() {
        assert_eq!(
            scan_placeholders(MAINTAIN_BODY),
            vec!["surface", "alias", "app_path", "skills_dir"]
        );
        let filled = fill_template(
            MAINTAIN_BODY,
            &[
                ("alias", "borsuk".to_string()),
                ("surface", "web".to_string()),
                ("skills_dir", "/tmp/skills/run-web/".to_string()),
                ("app_path", "/srv/app".to_string()),
            ],
        )
        .expect("the maintain body fills");
        assert!(filled.contains("Maintain the run skill of the web surface of borsuk."));
        assert!(filled.contains("/tmp/skills/run-web/"));
        assert!(filled.contains("/srv/app"));
        for step in [
            "1. Clean the feature index",
            "2. Read every source path",
            "3. Run the Run section",
            "4. Triage every finding",
            "5. Open one `bug` ticket",
            "6. Re-drive every fix",
            "7. Ship the repair as one PR",
            "8. Stop when the skill matches",
        ] {
            assert!(filled.contains(step), "the eight steps stay:\n{filled}");
        }
    }

    /// The theory roles carry no template yet. Every prompt entry point
    /// refuses them by name, so nothing writes a file that no task reads.
    #[test]
    fn the_theory_roles_have_no_prompt_and_every_entry_point_refuses_them() {
        assert_eq!(
            ROLES.len() + 2,
            ExecutionRole::ALL.len(),
            "only the two theory roles stay outside ROLES"
        );
        let dir = temp_prompts_dir("theory");
        for role in [ExecutionRole::TheoryAudit, ExecutionRole::TheoryChat] {
            assert!(!ROLES.contains(&role), "{role}");
            assert_eq!(file_name(role), None, "{role}");
            assert_eq!(builtin(role), None, "{role}");
            assert!(placeholders(role).is_none(), "{role}");
            let expected = format!("the {role} role has no prompt template");
            for result in [
                path(&dir, role).map(|_| ()),
                load(&dir, role).map(|_| ()),
                check(role, "anything"),
                save(&dir, role, "anything"),
                reset(&dir, role),
            ] {
                assert_eq!(result.unwrap_err().to_string(), expected);
            }
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no file was written"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_check_rejects_an_unknown_placeholder_and_an_empty_prompt() {
        let error = check(ExecutionRole::Implement, "hello {frobnicate}").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("{frobnicate}"), "{message}");
        assert!(message.contains("stage.implement"), "{message}");
        assert!(message.contains("{number}"), "{message}");

        let error = check(ExecutionRole::TicketCreate, "ticket #{number}").unwrap_err();
        assert!(error.to_string().contains("{number}"));

        let error = check(ExecutionRole::Refine, " \n\t").unwrap_err();
        assert_eq!(error.to_string(), "the prompt is empty");

        check(ExecutionRole::Release, "release {pr_list} now").unwrap();
        check(
            ExecutionRole::Review,
            r#"literal json {"question":"x"} keeps {number}"#,
        )
        .unwrap();
    }

    #[test]
    fn load_prefers_the_file_and_falls_back_to_the_builtin() {
        let dir = temp_prompts_dir("load");
        let missing = dir.join("absent");
        let loaded = load(&missing, ExecutionRole::Review).unwrap();
        assert_eq!(
            loaded,
            Template {
                text: REVIEW_PROMPT.to_string(),
                from_file: false
            }
        );

        save(&missing, ExecutionRole::Review, "custom {number}\n").unwrap();
        assert_eq!(
            fs::read_to_string(missing.join("review.md")).unwrap(),
            "custom {number}\n"
        );
        assert!(
            fs::read_dir(&missing).unwrap().count() == 1,
            "the save leaves no temporary file behind"
        );
        let loaded = load(&missing, ExecutionRole::Review).unwrap();
        assert_eq!(
            loaded,
            Template {
                text: "custom {number}\n".to_string(),
                from_file: true
            }
        );

        reset(&missing, ExecutionRole::Review).unwrap();
        reset(&missing, ExecutionRole::Review).unwrap();
        assert!(!missing.join("review.md").exists());
        assert!(!load(&missing, ExecutionRole::Review).unwrap().from_file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_placeholders_finds_placeholder_shaped_tokens() {
        assert_eq!(scan_placeholders("{a} x {b_1} {a}"), vec!["a", "b_1"]);
        assert!(scan_placeholders("{not a} {} {unclosed").is_empty());
        assert_eq!(scan_placeholders(r#"{"question":"x"} {a}"#), vec!["a"]);
    }

    /// One stray `{` must not hide the placeholder behind it. The scan and
    /// the fill share one span rule, so a template that passes the check
    /// always fills: every placeholder-shaped token the fill would leave
    /// literal is a token the scan reports.
    #[test]
    fn a_stray_brace_never_hides_the_placeholder_behind_it() {
        for template in [
            "Use { as a brace. Ticket {number}.",
            "Ticket {number}. See { and {frobnicate}.",
            "{ {number} }",
            "{unclosed {oops}",
        ] {
            let scanned = scan_placeholders(template);
            let filled = fill_template(
                template,
                &scanned
                    .iter()
                    .map(|name| (*name, format!("<{name}>")))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert!(
                scan_placeholders(&filled).is_empty(),
                "{template:?} left a placeholder unfilled in {filled:?}"
            );
        }

        assert_eq!(
            scan_placeholders("Use { as a brace. Ticket {number}."),
            vec!["number"]
        );
        let error = check(ExecutionRole::Implement, "See { and {frobnicate}.").unwrap_err();
        assert!(error.to_string().contains("{frobnicate}"), "{error:#}");
        assert_eq!(
            fill_template(
                "Use { as a brace. #{number}",
                &[("number", "142".to_string())]
            )
            .unwrap(),
            "Use { as a brace. #142"
        );
    }

    /// The parsers index by byte but cut only on ASCII braces, so a
    /// multi-byte template neither panics nor loses a character.
    #[test]
    fn the_parsers_keep_multibyte_text_whole() {
        let template = "zażółć {number} — gęślą jaźń {title}\n";
        assert_eq!(scan_placeholders(template), vec!["number", "title"]);
        assert_eq!(
            fill_template(
                template,
                &[("number", "142".to_string()), ("title", "łódź".to_string()),],
            )
            .unwrap(),
            "zażółć 142 — gęślą jaźń łódź\n"
        );
        assert!(scan_placeholders("żółw {ą} ok").is_empty());
    }

    #[test]
    fn fill_template_rejects_an_unknown_placeholder_and_fills_known_ones() {
        let error = fill_template("hi {name} {other}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("other"));
        let filled = fill_template("hi {name}", &[("name", "x".to_string())]).unwrap();
        assert_eq!(filled, "hi x");

        let error = fill_template("hi {not-known}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("not-known"));

        let filled = fill_template(
            "title={title}; body={body}",
            &[
                ("title", "keep {body} literal".to_string()),
                ("body", "body text".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(filled, "title=keep {body} literal; body=body text");
    }

    #[test]
    fn fill_template_fills_the_tickets_placeholder() {
        let filled = fill_template(
            "Tickets this PR closes: {tickets}",
            &[("tickets", "#4, #9".to_string())],
        )
        .unwrap();
        assert_eq!(filled, "Tickets this PR closes: #4, #9");

        let error =
            fill_template("hi {tickets} {nope}", &[("tickets", "none".to_string())]).unwrap_err();
        assert!(error.to_string().contains("nope"));
    }

    /// Drop every backtick span from one line.
    ///
    /// A `gh` command inside backticks keeps the GitHub nouns, so the ban test
    /// reads the line without the command text.
    fn strip_backticks(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut inside = false;
        for character in line.chars() {
            match character {
                '`' => inside = !inside,
                _ if !inside => out.push(character),
                _ => {}
            }
        }
        out
    }
    /// Every template must carry the vocabulary.
    const VOCABULARY_PROMPTS: [&str; 6] = [
        REFINE_PROMPT,
        IMPLEMENT_PROMPT,
        REVIEW_PROMPT,
        RELEASE_PROMPT,
        TICKET_PROMPT,
        TICKET_CHAT_PROMPT,
    ];

    #[test]
    fn the_vocabulary_prompts_use_only_ticket_and_pr() {
        for prompt in VOCABULARY_PROMPTS {
            for line in prompt.lines() {
                let bare = strip_backticks(line).to_lowercase();
                assert!(
                    !bare.contains("issue"),
                    "a line breaks the vocabulary with \"issue\": {line}"
                );
                assert!(
                    !bare.contains("pull request"),
                    "a line breaks the vocabulary with \"pull request\": {line}"
                );
            }
        }
    }

    #[test]
    fn backtick_stripping_removes_only_command_text() {
        assert_eq!(strip_backticks("run `gh issue edit 7` now"), "run  now");
        assert_eq!(strip_backticks("no commands here"), "no commands here");
    }

    #[test]
    fn the_docs_copies_match_the_consts_byte_for_byte() {
        assert_eq!(
            REFINE_PROMPT,
            include_str!("../docs/v0.8/prompts/refine.md")
        );
        assert_eq!(
            IMPLEMENT_PROMPT,
            include_str!("../docs/v0.8/prompts/implement.md")
        );
        assert_eq!(
            RELEASE_PROMPT,
            include_str!("../docs/v0.8/prompts/release.md")
        );
        assert_eq!(
            TICKET_PROMPT,
            include_str!("../docs/v0.6/prompts/ticket.md")
        );
        assert_eq!(
            TICKET_CHAT_PROMPT,
            include_str!("../docs/v0.6/prompts/ticket-chat.md")
        );
        assert_eq!(
            REVIEW_PROMPT,
            include_str!("../docs/v0.8/prompts/review.md")
        );
    }

    #[test]
    fn the_implement_prompt_carries_the_contract_and_the_simplest_change() {
        for required in [
            "## Before / After",
            "smallest change",
            "fails with the change reverted",
            "{model}",
            "{skills}",
        ] {
            assert!(
                IMPLEMENT_PROMPT.contains(required),
                "the implement prompt must carry {required}"
            );
        }
    }

    #[test]
    fn the_review_prompt_carries_the_re_drive() {
        for required in [
            "base worktree",
            "only the test files",
            "no criterion needs",
            "{model}",
            "{skills}",
            "{prediction}",
            "Compare the prediction",
            "<aif-delta-v1>",
            "The five slot ids are",
            "ends with no block",
        ] {
            assert!(
                REVIEW_PROMPT.contains(required),
                "the review prompt must carry {required}"
            );
        }
    }

    #[test]
    fn the_rewritten_prompts_pass_the_prose_rules_they_ask_for() {
        for (name, prompt) in [("implement", IMPLEMENT_PROMPT), ("review", REVIEW_PROMPT)] {
            crate::theory::contract::lint_prose(prompt).unwrap_or_else(|finding| {
                panic!("the {name} prompt breaks a prose rule: {finding}")
            });
        }
    }

    #[test]
    fn the_choice_prompts_show_a_block_that_the_ask_parser_accepts() {
        for prompt in [REFINE_PROMPT, IMPLEMENT_PROMPT, REVIEW_PROMPT] {
            let ask = crate::ask::parse_ask_block(prompt)
                .expect("the choice prompt must contain one valid ask block");
            assert_eq!(ask.question, "Which workload mode ships first?");
            assert_eq!(ask.options.len(), 2);
        }
    }

    #[test]
    fn the_refine_prompt_defines_a_parallel_execution_plan() {
        let prompt = REFINE_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Fast | Wave |",
            "Put independent chunks in the same",
            "do not edit the same files",
            "Assign shared files and final integration to one coordinator chunk",
            "use one C1 row",
            "Use at most three subagents",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    /// Requirement R13. The refined ticket carries the R13 sections before
    /// the plan table, one falsifiable line per criterion, and no wording
    /// the v0.7 acceptance section used.
    #[test]
    fn the_refine_prompt_defines_the_criteria_contract() {
        for required in [
            "## Problem",
            "## Grounding",
            "## Decisions",
            "## Repro",
            "## Acceptance criteria",
            "- AC-<n> · <falsifiable statement> · check: <target>",
            "`<feature> drive`",
            "`<feature> fast`",
            "`measure <id>`",
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Fast | Wave |",
            "`new: <feature>`",
        ] {
            assert!(REFINE_PROMPT.contains(required), "missing: {required}");
        }
        assert!(
            !REFINE_PROMPT.contains("Agreed approach"),
            "the v0.7 acceptance wording must not return"
        );
    }

    #[test]
    fn the_refine_prompt_splits_chunks_into_labelled_sub_tickets() {
        let prompt = REFINE_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            // A split creates one sub-ticket per chunk.
            "create one sub-ticket for each chunk with `gh issue create`",
            "Create the sub-tickets in wave order",
            "A sub-ticket must stand alone",
            // The routing labels the tag routes read.
            "`complexity:<level>`",
            "`review-complexity:<level>`",
            "Give each sub-ticket the `refined` label, the `chunk` label",
            "Never give a sub-ticket the `to-refine` label",
            // A missing label would fail `gh issue create`.
            "gh label create <name>",
            // The wave order rides on the blocker parser of the gates module.
            "Blocked by #A and #B",
            // The parent never enters the implement stage, and it closes
            // when the final chunk merges. Both rules keep it from going
            // stale as an open ticket no agent ever finishes.
            "No agent implements the parent",
            "Never give the parent the `refined` label",
            "--remove-label to-refine --add-label epic",
            "add a second `Closes #{number}` line to the PR body",
            "the parent closes when the PR of the final chunk merges",
            // A second refine run must not duplicate the sub-tickets.
            "do not create the sub-tickets a second time",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    /// The parent of a split ticket closes through the PR of its final
    /// chunk, so the implement prompt must carry that rule.
    #[test]
    fn the_implement_prompt_closes_the_parent_of_a_final_chunk() {
        let prompt = IMPLEMENT_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(prompt.contains(
            "When the ticket body names a parent ticket and marks this ticket \
as the final chunk, add a second `Closes` line for the parent number, so the \
merge closes the parent too."
        ));
    }

    #[test]
    fn the_implement_prompt_consumes_parallel_waves_safely() {
        let prompt = IMPLEMENT_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "Use the ticket implementation plan as the execution schedule",
            "start all agents for that wave in one tool turn",
            "Use at most three subagents at once",
            "avoid all git and `gh` writes",
            "Never give two concurrent writers the same file",
            "If subagents are unavailable, execute the chunks directly",
            "The coordinator owns shared files, integration, git operations, and GitHub operations",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    #[test]
    fn the_review_prompt_mandates_a_repair_a_push_and_the_ready_flip() {
        let prompt = REVIEW_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "You repair every finding yourself",
            "ready for review, or labelled `needs-human`",
            "gh pr view {number} --json isCrossRepository --jq .isCrossRepository",
            "Never push a fork repair to `origin`",
            "Take the human path when the PR comes from a fork",
            "prove that this worktree holds the PR head",
            "Push once, at the end of the run",
            "git push origin HEAD:$(gh pr view {number} --json headRefName --jq .headRefName) && gh pr ready {number}",
            "Never pass `--force`. Never merge the PR.",
            "GitHub refuses a formal review of your own PR",
            "add the `needs-human` label to the PR",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    #[test]
    fn the_stage_prompts_run_without_the_operator() {
        let paragraph = "Run without the operator. No person reads your text during \
the run. Do not ask for approval of a plan, a design, or a change. Do not \
stop to report a plan, and do not end a turn with a question. Decide with \
the facts you have and act. Stop early only through the escape this prompt \
names.";
        for (prompt, opening_end) in [
            (REFINE_PROMPT, "work only in this one."),
            (IMPLEMENT_PROMPT, "work only in this one."),
            (REVIEW_PROMPT, "work only in this one."),
            (RELEASE_PROMPT, "work only in this one."),
        ] {
            let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            let position = normalized
                .find(paragraph)
                .expect("the stage prompt holds the autonomy paragraph");
            let before = normalized[..position].trim_end();
            assert!(
                before.ends_with(opening_end),
                "the autonomy paragraph does not follow the opening paragraph: {before}"
            );
        }
        for prompt in [TICKET_PROMPT, TICKET_CHAT_PROMPT] {
            let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                !normalized.contains(paragraph),
                "a ticket prompt must not hold the autonomy paragraph"
            );
        }
    }
}
