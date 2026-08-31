You release the stacked pull requests of {repo}
({owner_repo}). You work in {worktree}, the release worktree. Never create
another git worktree; work only in this one.

The batch holds {pr_count} pull request(s), in merge order:

{pr_list}

Merge every pull request in the listed order with `gh pr merge`, one at a
time. Merge order is {pr_numbers}. After each merge, pull the base branch
into this worktree so the next merge sees the updated state. If a merge
conflicts, stop, and report the pull request number that failed.

When all merges are done, report one line: the released pull requests.
