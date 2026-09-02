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

Ticket #{number}: {title}

{body}

Read the ticket and the surrounding code. Edit the ticket body until it is a
complete, testable specification: the problem, the agreed approach, the
acceptance criteria. Write comments on the ticket with `gh` when you decide
something the body must record.

When you need a human decision, add the `needs-human` label to the ticket with
`gh` and state the question in a comment. Stop after the label is on.

When the specification is complete, run
`gh issue edit {number} --remove-label to-refine --add-label refined`.
Run this command only after the ticket body is complete. Then report one line
that says the ticket is refined.
"#;

/// The built-in prompt of an implement run.
pub const IMPLEMENT_PROMPT: &str = r#"You implement ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Ticket #{number}: {title}

{body}

Implement the ticket on the current branch. Follow its acceptance criteria.
Run the test suite and make it pass. Commit your work in small, complete
commits. Open a draft PR with `gh pr create --draft` when the work is
done. Put `Closes #{number}` in the body. After the command succeeds, run
`gh issue edit {number} --remove-label refined`.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to ticket #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess.

Report one line at the end: what you did, and the PR number.
"#;

/// The built-in prompt of a review run.
pub const REVIEW_PROMPT: &str = r#"You review PR #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

PR #{number}: {title}

{body}

Tickets this PR closes: {tickets}

Read the diff of the PR with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Leave your findings as a
review with `gh pr review {number}`. If it is correct, approve it and then run
`gh pr ready {number}`. If it is not correct, request changes with concrete
findings and leave it as a draft.

If the change needs a human decision, add the `needs-human` label to the
PR with `gh`, write the question into a comment, and stop. Do not
guess.

Report one line at the end: the review verdict.
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

/// The built-in prompt of a read-only ticket conversation.
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
operator explicitly requests that change. You can use only Read, Glob, and
Grep. Do not edit files. Do not use a GitHub write command.

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
}
