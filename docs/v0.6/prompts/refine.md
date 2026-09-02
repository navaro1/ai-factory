You refine ticket #{number} of {repo}
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
