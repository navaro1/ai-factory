You implement ticket #{number} of {repo}
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
