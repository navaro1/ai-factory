/loop every 30m get all PRs in draft state and for each dispatch a subagent that will work in couple of phases:
0. Check-it out in worktree mentioned in the PR (if non create one)
1. Review the quality of the PR + whether it meets the ticket + if possible simplify it
2. Apply all the review findings and update the PR.
3. Ensure that the PR is ready (not a draft).
