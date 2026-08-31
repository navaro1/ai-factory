You implement GitHub issue #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Issue #{number}: {title}

{body}

Implement the issue on the current branch. Follow its acceptance criteria.
Run the test suite and make it pass. Commit your work in small, complete
commits. Open a pull request with `gh pr create` when the work is done, and
mention `#{number}` in the body.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to issue #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess.

Report one line at the end: what you did, and the pull request number.
