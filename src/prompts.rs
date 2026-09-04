//! The built-in prompt templates of every stage.
//!
//! Every template lives here, outside the daemon, so wording changes touch
//! one file. A file `prompts/<stage>.md` in the config directory overrides
//! the built-in default. The docs directory `docs/v0.6/prompts/` holds a
//! reference copy of each template, pinned byte for byte by a test.
//!
//! The vocabulary rule: a template names a repository item "ticket" or
//! "PR". A `gh` command inside backticks keeps the GitHub word, because
//! the CLI speaks its own nouns.

/// The built-in prompt of a refine run.
///
/// It runs in the repository checkout and never creates a worktree.
pub const REFINE_PROMPT: &str = r#"You refine ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, the repository checkout. Never create
a git worktree; stay in this checkout.

Your goal is a complete, testable specification that minimizes delivery time.
Do not implement the change.

Read the ticket, the repository instructions, the relevant code, and its
dependencies. Confirm that the ticket is still valid. Keep the requested scope.
Use parallel tool calls for independent reads. Use subagents only for sizeable,
independent research. Use at most three subagents. Do not use a subagent for
routine reads or for a second review.

The ticket body must contain these sections:

- Problem
- Agreed approach
- Acceptance criteria
- Implementation plan

The implementation plan must use this table:

| Chunk | Goal | Owned files or paths | Depends on | Validation | Wave |
|---|---|---|---|---|---|

Create separate chunks only when the split reduces delivery time. Make each
chunk large enough to justify coordination. Put independent chunks in the same
wave only when they have no dependency and do not edit the same files. Assign
shared files and final integration to one coordinator chunk. Put a shared
interface or data contract before chunks that depend on it. State the final
integration order and final validation. For a small or tightly coupled change,
use one C1 row and state that parallel work would add delay.

Edit the ticket body with `gh`. Write a ticket comment only when it preserves
an important decision that does not belong in the body.

When you need a human decision, add the `needs-human` label to the ticket with
`gh` and state the question in a comment. Stop after the label is on.

When the specification is complete, run
`gh issue edit {number} --remove-label to-refine --add-label refined`.
Run this command only after the ticket body is complete. Then report one line
that says the ticket is refined.

Ticket #{number}: {title}

{body}
"#;

/// The built-in prompt of an implement run.
pub const IMPLEMENT_PROMPT: &str = r#"You implement ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Your goal is a complete change that meets every acceptance criterion with the
shortest safe delivery time. Follow the repository instructions and keep the
requested scope. Implement the ticket on the current branch.

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

Open a draft PR with `gh pr create --draft` when the work is done. Put
`Closes #{number}` in the body. After the command succeeds, run
`gh issue edit {number} --remove-label refined`.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to ticket #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess.

Report one line at the end: what you did, and the PR number.

Ticket #{number}: {title}

{body}
"#;

/// The built-in prompt of a review run.
pub const REVIEW_PROMPT: &str = r#"You review PR #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

PR #{number}: {title}

{body}

Tickets this PR closes: {tickets}

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

Fix every finding in this worktree. Add the missing tests. Keep the scope of
the linked tickets. Run the full validation of the repository and make it
pass. Commit the repairs in small, complete commits.

Push once, at the end of the run. A push on a draft PR can restart your own
review, so never push a partial fix. Push the commits and open the release
gate in one command line:

`git push origin HEAD:$(gh pr view {number} --json headRefName --jq .headRefName) && gh pr ready {number}`

Never pass `--force`. Never merge the PR.

Record the outcome with `gh pr comment {number}`. Name the findings, the
repairs, and the validation result. GitHub refuses a formal review of your own
PR, so this comment is the record.

When the PR needs no repair, post the record and run `gh pr ready {number}`.

Take the human path when the PR comes from a fork, when a finding needs a human
decision, when the repair leaves the scope of the linked tickets, or when the
push fails. On that path, add the `needs-human` label to the PR with `gh`, write
the question into a comment, leave the draft, and stop. Do not guess.

Report one line at the end: the review verdict, and the number of commits you
pushed.
"#;

/// The built-in prompt of a release run.
pub const RELEASE_PROMPT: &str = r#"You release the stacked PRs of {repo}
({owner_repo}). You work in {worktree}, the release worktree. Never create
another git worktree; work only in this one.

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

#[cfg(test)]
mod tests {
    use super::*;

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
            include_str!("../docs/v0.6/prompts/refine.md")
        );
        assert_eq!(
            IMPLEMENT_PROMPT,
            include_str!("../docs/v0.6/prompts/implement.md")
        );
        assert_eq!(
            RELEASE_PROMPT,
            include_str!("../docs/v0.6/prompts/release.md")
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
            include_str!("../docs/v0.6/prompts/review.md")
        );
    }

    #[test]
    fn the_refine_prompt_defines_a_parallel_execution_plan() {
        let prompt = REFINE_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Wave |",
            "Put independent chunks in the same",
            "do not edit the same files",
            "Assign shared files and final integration to one coordinator chunk",
            "use one C1 row",
            "Use at most three subagents",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
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
}
