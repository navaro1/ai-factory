# GitHub closing keywords (fetched 2026-09-02)

Source: https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/linking-a-pull-request-to-an-issue

Facts that the aif link parser follows:

- The supported keywords are: `close`, `closes`, `closed`, `fix`, `fixes`,
  `fixed`, `resolve`, `resolves`, `resolved`.
- The keywords work in the pull request description and in commit messages.
- Case does not matter. A colon may follow the keyword.
  Examples: `Closes #10`, `closes: #10`, `CLOSES: #10`.
- Same repository syntax: `KEYWORD #ISSUE-NUMBER`, for example `Closes #10`.
- Cross repository syntax: `KEYWORD OWNER/REPOSITORY#ISSUE-NUMBER`, for
  example `Fixes octo-org/octo-repo#100`.
- Multiple issues need the full syntax for each one, for example
  `Resolves #10, resolves #123`.
- GitHub closes the linked issues only when the pull request merges into the
  repository default branch. A pull request that targets another branch
  creates no links on GitHub, and the keywords are ignored there.
- A keyword in a pull request comment links two pull requests; the aif
  parser does not read comments.
- A keyword in a commit message closes the issue at merge time, but GitHub
  does not list that pull request as linked. The aif parser reads pull
  request bodies only.

aif decisions on top of these facts:

- The parser reads pull request bodies only, and same-repository `#n`
  references only. Cross-repository references are out of scope.
- The parser counts keywords on every pull request, also when the base
  branch is not the default branch. GitHub itself ignores those keywords.
  The factory accepts this divergence, because the badge shows the author's
  recorded intent. The case is rare in this pipeline.

# Pull request head refs (fetched 2026-09-02)

Source: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/checking-out-pull-requests-locally

Facts that the aif pr worktree follows:

- GitHub stores every pull request head as the read-only ref
  `refs/pull/<ID>/head`. The documented fetch is
  `git fetch origin pull/ID/head:BRANCH_NAME`.
- The `refs/pull/` namespace is read-only. A push there is rejected.
- `gh pr checkout PULL-REQUEST` also checks a pull request out locally, but
  it switches the current branch of the checkout, so it does not fit a
  dedicated worktree that must stay on its own branch.

aif decision: the daemon fetches `pull/<n>/head` into `FETCH_HEAD` and runs
`git reset --hard FETCH_HEAD` in the pr worktree. A fetch into the checked
out branch directly is refused by git, so the two-step form is used.

