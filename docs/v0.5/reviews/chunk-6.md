# Task 6 review

## Acceptance criteria

| Acceptance criterion | Result | Evidence |
| --- | --- | --- |
| A table test accepts every legal transition. It rejects a representative illegal transition from each state. | MET | `every_transition_in_the_matrix_follows_the_rules` checks all 25 state pairs. It accepts 9 legal pairs and rejects 16 illegal pairs. It also checks the resulting state, the error text, and the unchanged task after rejection. |
| A retry past `MAX_ATTEMPTS` is refused with a clear message. | MET | `retry_past_max_attempts_is_refused_with_a_clear_message` checks the task id, both states, attempt 3, the limit, and unchanged task data. |
| A duplicate task is refused while the first task is active. It is allowed after a terminal state. | MET | `upsert_refuses_while_the_existing_task_is_active` covers all three active states. `upsert_replaces_a_terminal_task_with_a_fresh_queued_task` covers `Done` and `Failed`. |
| The count helpers have test coverage. | MET | The tests cover running counts by stage and by repository. They also cover zero counts, insertion order, and the active-state definition. |

## Findings and fixes

| Severity | Finding | Fix |
| --- | --- | --- |
| important | Both count helpers counted all active tasks. Queued tasks then used scheduler capacity before dispatch. The scheduler specification calculates capacity from running tasks. | I changed both helpers to count only `TaskState::Running`. I changed the count tests first. The focused test failed with a count of 1 instead of 0. It passed after the fix. |
| minor | The transition matrix checked only `Result::is_ok()`. It did not detect a legal transition with no state change. It also did not check the required state names in errors. | I added checks for the target state, the full transition text, and unchanged task data after rejection. A temporary error-text mutation made the test fail. The restored code made it pass. |
| minor | `Task::log_file_name` was an unused public API. The brief does not require this helper. No source file called it. | I removed the helper and its source-only test. `Task::new` still builds the required task id. The caller still supplies the required log path. |
| minor | The `attempt` field comment said it counted started runs. A queued task already has attempt 1. A retry increments the field before the next run starts. | I changed the comment to describe the current attempt number. |

## Constraint review

- The chunk adds no dependency.
- The chunk adds no asynchronous code, thread, loop, journal, or domain-state lock.
- Production code contains no `unwrap()` or `expect()`.
- Tests use no network or external tool.
- Every test contains an assertion on behavior or a checked result.
- Every public type, field, constant, and function has a documentation comment.
- The diff changes no file in the old v0.4 tree.

## Deliberately left alone

- The source layout lists `Task` and `TaskState` in `src/model.rs`.
- The Task 2 specification freezes `src/model.rs`.
- The Task 6 brief assigns all Task 6 work to `src/tasks.rs`.
- The worktree instructions also freeze `src/model.rs`.
- I treated the Task 6 brief and the worktree rule as the specific requirements.
- The Task 6 section in `docs/v0.5/SPEC.md` omits `Queued` to `Failed`.
- The current Task 6 brief requires that transition for cancellation and trigger removal.
- I did not edit either documentation conflict because this chunk does not own those files.
- I did not change the other untracked task files. They are review inputs and logs.

## Final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-6)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.75s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.10s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 55 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_returns_steps_in_order ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::apply_never_touches_another_repository ... ok
test config::tests::example_file_parses_with_every_override ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test tasks::tests::a_requeued_task_moves_to_the_back_of_the_order ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test tasks::tests::a_transition_stamps_updated_ms_and_keeps_created_ms ... ok
test tasks::tests::an_unknown_task_id_is_an_error ... ok
test tasks::tests::cancelling_records_the_cancelled_reason ... ok
test tasks::tests::counts_by_stage_count_running_tasks_only ... ok
test tasks::tests::retries_count_attempts_up_to_the_limit ... ok
test tasks::tests::new_builds_the_id_per_the_naming_rules ... ok
test tasks::tests::counts_by_stage_repo_count_per_repository_and_stage ... ok
test tasks::tests::retry_past_max_attempts_is_refused_with_a_clear_message ... ok
test tasks::tests::running_and_active_keep_the_insertion_order ... ok
test tasks::tests::task_state_round_trips_through_json ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test tasks::tests::every_transition_in_the_matrix_follows_the_rules ... ok
test tasks::tests::cancelling_a_queued_task_removes_it_from_active_tasks ... ok
test tasks::tests::upsert_inserts_new_tasks_in_insertion_order ... ok
test tasks::tests::upsert_keys_on_repo_stage_kind_and_number ... ok
test tasks::tests::upsert_refuses_while_the_existing_task_is_active ... ok
test tasks::tests::upsert_replaces_a_terminal_task_with_a_fresh_queued_task ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok

test result: ok. 55 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aifd_run_accepts_a_config_path ... ok
test aifd_run_help_lists_the_config_option ... ok
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aif_help_lists_all_subcommands ... ok
test aif_subcommands_print_placeholders ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/exec_contract.rs (target/debug/deps/exec_contract-efcaecd164461c8b)

running 1 test
test script_exec_enforces_order_and_records_calls_outside_the_crate ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests aif

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

all checks passed
```

## Final verdict

PASS
