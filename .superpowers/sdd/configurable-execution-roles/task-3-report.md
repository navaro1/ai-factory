# Task 3 report: daemon configuration lifecycle

## Result

Status: DONE

The daemon stores each resolved role before its first runner start.
The state file keeps the binding through retry, resume, and restart.
New logical tasks clear the old binding and use the active configuration.

The daemon now owns the resolved `factory.toml` path and its file revision.
Settings saves use `toml_edit`, a sibling temporary file, and an atomic rename.
The daemon rejects stale saves before it changes the file or live configuration.
It keeps the old live configuration after invalid or topology-changing reloads.

## Tests

The initial save regression test was red before the action handler existed.
It failed because the daemon sent no settings result push.

Focused tests passed after the implementation:

- `a_settings_save_updates_the_file_live_config_and_result_push`
- `a_stale_settings_save_changes_neither_the_file_nor_live_config`
- `an_invalid_reload_keeps_the_live_config`
- `a_topology_reload_keeps_the_old_live_topology`
- `a_reloaded_role_applies_to_a_logically_new_task_only`
- Binding retry, parked-resume, restart, and new-task tests.
- Configuration comment, atomic write, and revision tests.
- Socket settings identity and field-source tests.
- State binding persistence test.

`cargo test` passed: 602 library tests, 45 `aif` tests, 4 `aifd` tests,
8 CLI tests, 13 role tests, 1 executor test, and doc tests.

`./check.sh` passed. It ran format, Clippy, all tests, and the installer test.

## Review fixes

The review found three defects. This change fixes each defect.

- A completed task no longer keeps its binding in the saved state.
- A failed task keeps its binding for a retry after a daemon restart.
- A rowless binding stays safe during the first poll after a restart.
- A role edit keeps field comments and inline comments on changed values.
- An atomic save gives the new file the permissions of the old file.

The new tests failed before these fixes. All new tests pass after the fixes.
The final `./check.sh` run passed 605 library tests and the installer test.

## Self review

- The binding save occurs before `RunnerFactory::build` and `Runner::start`.
- Binding removal occurs only when the daemon creates a new logical task.
- The save path checks the disk revision before it edits or writes the file.
- Invalid and restart-required reload paths keep the active configuration unchanged.
- Result pushes preserve the request identity and operation type.
- The daemon pushes the active file revision in each state view.
- The `Inbound::Act` payload uses `Box<Action>` to keep the event enum small.
- The doctor reader ignores settings result pushes and continues to wait for state.

## Concerns

None.

## Commit

`Add daemon config lifecycle`
