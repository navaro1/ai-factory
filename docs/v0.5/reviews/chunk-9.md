# Task 9 review

## Acceptance criteria

| Acceptance criterion | Result | Evidence |
| --- | --- | --- |
| Tests use temporary shell scripts that print, echo stdin, or sleep. | MET | Fifteen process tests use scripts under unique temporary directories. |
| The log gets each raw stdout line byte for byte, including invalid JSON. | MET | The raw-line test compares all log bytes. It includes invalid JSON, a blank line, and a final line without a newline. |
| The process reports the exit code and success value one time. | MET | The exit test checks successful and failed exits. It drains the channel until all senders close. |
| A child that ignores SIGTERM reaches SIGKILL within the test limit. | MET | The test uses a TERM trap and a fake TERM executor. It checks SIGKILL and a total time below one second. |
| A write after process exit returns an error. | MET | Two tests cover this case. One test keeps the stdin pipe open in a descendant process. |

## Findings and fixes

| ID | Severity | Finding | Fix |
| --- | --- | --- | --- |
| F1 | important | The stop sequence skipped the first 10-second wait when `protocol_interrupt` was false. | I made the first wait unconditional. A regression test checks a natural exit before SIGTERM. |
| F2 | important | The handle kept stdin open after it sent `ProcEvent::Exit`. A descendant could keep the pipe valid. | The waiter now closes shared stdin before it sends the exit event. A descendant test proves the error result. |
| F3 | important | Hook and signal errors disappeared when the child later exited. | The supervisor now sends `ProcEvent::Error` at once. Tests cover hook and SIGTERM errors. |
| F4 | important | Stop tests ran the real `kill` command. This violated the external command test constraint. | Stop tests now inject fake executors. One test checks the exact `kill -TERM` argument vector. |
| F5 | important | Some tests used open-ended receive loops. The exact-once test stopped after a short drain period. | All event waits now use a deadline. The exact-once test drains until channel closure. |
| F6 | minor | Public constants, getters, and test seams exceeded this chunk scope. | I made test seams private. I removed the public constants and unused getters. |
| F7 | minor | A production retry loop handled an `ETXTBSY` test fixture race. | I removed the retry loop. Tests now pass each new script to `/bin/sh`. |
| F8 | minor | Two log threads could both pass the broken-sink check before one write failed. | The sink now checks its broken state again while it holds the file lock. |
| F9 | minor | No test checked log parent creation or append mode. | I added one test that starts two children and checks the same nested log. |
| F10 | minor | `LogError` became an incorrect name after the supervisor reported stop errors. | I renamed the event to `ProcEvent::Error` and documented its full purpose. |

## Exact final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-9)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.75s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.12s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 53 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test exec::tests::script_returns_steps_in_order ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test config::tests::example_file_parses_with_every_override ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test proc::tests::write_after_exit_fails_when_a_descendant_keeps_stdin_open ... ok
test proc::tests::every_raw_stdout_line_reaches_the_log_byte_for_byte ... ok
test proc::tests::write_to_a_dead_child_returns_an_error ... ok
test proc::tests::spawn_creates_the_log_parent_and_appends_to_the_log ... ok
test proc::tests::the_protocol_interrupt_can_stop_the_child_before_any_signal ... ok
test proc::tests::write_line_reaches_the_child_and_close_stdin_ends_it ... ok
test proc::tests::terminate_runs_kill_term_through_the_injected_exec ... ok
test proc::tests::stderr_lines_reach_the_log_with_a_prefix_and_no_events ... ok
test proc::tests::exit_code_and_success_flag_are_reported_exactly_once ... ok
test proc::tests::a_polite_child_dies_at_sigterm ... ok
test proc::tests::a_protocol_interrupt_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::a_sigterm_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::a_child_that_ignores_sigterm_reaches_sigkill ... ok
test proc::tests::stop_gracefully_does_not_block_the_caller ... ok
test proc::tests::the_initial_grace_allows_a_natural_exit_without_a_protocol_interrupt ... ok

test result: ok. 53 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aifd_run_help_lists_the_config_option ... ok
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aifd_run_accepts_a_config_path ... ok
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

## Deliberate exclusions

- I left the mutexes around the file, child, stdin, and hook. They protect operating-system resources, not domain state.
- I left the short wait after SIGKILL. This wait lets the waiter confirm process exit before it reports the stop outcome.
- I did not change any frozen or old-tree file. I also left the other untracked review input files unchanged.

Final verdict: PASS
