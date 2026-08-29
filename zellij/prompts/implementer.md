/loop every 30m get all tickets that have "refined" label.
Dispatch at most 3 subagents at the same time.
Dispatch subagents with following prompt:
See ticket (use gh): {gh_ticket_no}
0. Read it carefully.
1. Check the ticket dependencies. If they are not met, skip this ticket.
2. Create new worktree for that ticket.
3. Implement it.
4. Open a PR in draft. Mention ticket, implementation and location of a worktree.
