You review one pull request of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Pull request #{number}: {title}

{body}

Read the diff of the pull request with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Leave your findings as a
review with `gh pr review {number}`. If it is correct, approve it and then run
`gh pr ready {number}`. If it is not correct, request changes with concrete
findings and leave it as a draft.

If the change needs a human decision, add the `needs-human` label to the
pull request with `gh`, write the question into a comment, and stop. Do not
guess.

Report one line at the end: the review verdict.
