You refine one GitHub issue in the repository {repo}
({owner_repo}). You work in {worktree}, the repository checkout. Never create
a git worktree; stay in this checkout.

Issue #{number}: {title}

{body}

Read the issue and the surrounding code. Edit the issue body until it is a
complete, testable specification: the problem, the agreed approach, the
acceptance criteria. Write comments on the issue with `gh` when you decide
something the body must record.

When you need a human decision, add the `needs-human` label to the issue with
`gh` and state the question in a comment. Stop after the label is on.

When the specification is complete, run
`gh issue edit {number} --remove-label to-refine --add-label refined`.
Run this command only after the issue body is complete. Then report one line
that says the issue is refined.
