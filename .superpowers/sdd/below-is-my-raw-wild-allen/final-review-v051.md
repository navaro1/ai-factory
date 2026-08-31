# AI Factory v0.5.1 final review

## Summary

The feature now has consistent input policy across the wire, daemon, task table, and process runners.

OpenCode messages become a later turn of the same session. Claude messages use the live channel when that channel works.

The daemon now keeps each accepted message until a runner accepts it. The daemon also prevents duplicate task launches.

I found four defects. I fixed each defect and added regression tests.

## Findings and fixes

### Important — A resumed message could exceed the live process limit

`resume_pending_chats` checked the running-task limit only. It did not check parked live processes.

A reaped Claude task could start while another parked Claude process held the only live slot.

I added the live process limit check before the scheduler check. A regression test covers this seam.

### Important — The wire and daemon could disagree about message acceptance

The wire closed input when a sibling task ran. The daemon could still send a message to a live Claude process.

The daemon also queued messages for terminal Claude tasks whose input policy was closed.

I moved the sibling check before the live send. I also made `chat` enforce the same closed-input result.

Regression tests cover a live sibling conflict and a spent terminal session.

### Important — A failed live send could lose an accepted message

The daemon logged a Claude send error and discarded the message. The user interface already reported message acceptance.

Several terminal paths could also delete or strand a queued message. These paths included turn completion and GitHub poll completion.

I now store the message for a resumed turn after a live send error. I keep its session marker and session identifier.

I centralized the terminal reopen action in task completion and task failure paths.

Regression tests cover send failure, turn completion, poll completion, and terminal task recovery.

### Minor — A socket test had a process inheritance race

A parallel child process could briefly inherit the listener before program start. The stale-socket test could fail during that interval.

I added a one-second deadline and a short poll interval. The test still fails if a real listener remains.

## Task 28 open question

The required disk-marker case already has coverage.

`the_input_mode_closes_a_running_task_that_has_no_session_yet` writes a session marker through a `Started` event.

The test then removes the in-memory session identifier. It confirms that the wire returns `NextTurn`.

## Constraint audit

The branch adds no dependency, asynchronous runtime, clock loop, journal, or domain-state mutex.

Production code adds no `unwrap` or `expect` call. New tests use local fakes and bounded waits.

The tests do not use a network or a real agent tool. Each new test contains a state or effect assertion.

The OpenCode runner still closes standard input. It does not attempt mid-turn steering.

The Claude runner still keeps standard input open for prompts and live messages.

## Deliberately unchanged

I did not change the measured OpenCode and Claude behavior. The machine evidence defines that behavior.

I did not change historical review files. Task 28 and Task 29 have reports, but no separate review files.

The progress record explains that the orchestrator reviewed Task 28 directly. Task 29 only produced the final report.

## Exact final `./check.sh` output

Command: `CARGO_TERM_QUIET=true ./check.sh`

Exit code: `0`

```text
== fmt ==
== clippy ==
== test ==

running 501 tests
....................................................................................... 87/501
....................................................................................... 174/501
....................................................................................... 261/501
....................................................................................... 348/501
....................................................................................... 435/501
..................................................................
test result: ok. 501 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.80s


running 45 tests
.............................................
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.17s


running 4 tests
....
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 8 tests
........
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s


running 1 test
.
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

== install ==
installer test passed
all checks passed
```

## Final verdict

PASS
