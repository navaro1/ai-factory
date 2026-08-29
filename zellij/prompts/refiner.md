/loop every 30m Get all tickets labelled `to-refine`.
For each of the ticket dispatch a subagent with following command:

See ticket on github (use gh): {github_issue_no}
0. Check if the issue is still valid - that's very important.
1. Ground yourself in the codebase and documentation (if applicable)
2. Update the ticket description for thorough descritpion/implementation plan for a mid-level developer that will be implementing. Ask me claryfing questions (but only if needed and provide context). Try to explain to them which tasks within ticket can be done in parallel.
3. Add `refined` label to that ticket. Remove `to-refine` label.
