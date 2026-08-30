# Task 13 review

## Acceptance criteria

| Acceptance criterion | Result | Evidence |
|---|---|---|
| Each ID rule is asserted. | Met | The tests cover permission, question, stuck attempts, issue and pull request items, and release gates. |
| Two pushes for one condition produce one open decision. | Met | `pushing_one_condition_twice_keeps_one_row` covers all five kinds. The refresh test also checks current row data. |
| The full response matrix has one table test. | Met | The 5 by 7 table accepts all nine legal combinations. It refuses all 26 illegal combinations. |
| `drop_for_task` removes only the specified task rows. | Met | The test retains another task, a `NeedsHuman` row, and a `ReleaseGate` row. |

## Findings and fixes

### Important: A public constructor made a wrong stuck ID

`Decision::new` always used attempt one for `Stuck`. A later task attempt could therefore collide with the first attempt.

I reproduced attempt two as `stuck:...:1`. I replaced the generic constructor with kind-specific constructors.

The task constructors now get the repository, stage, task ID, and attempt from `Task`. The final stuck tests cover attempts one and two.

### Important: A duplicate push kept stale row data

`Decisions::push` ignored all new data for an existing ID. A release row could therefore show an obsolete pull request list.

I added a failing test with lists `[7]` and `[7, 9]`. The first run retained `[7]`.

The queue now refreshes the row data. It keeps the initial open time and does not add a second row.

### Important: Two tests claimed side effects that they did not test

Two test names claimed GitHub comment and label actions. Their bodies only called `validate` and checked for no error.

Chunk 15 owns those daemon actions. I removed the false tests and retained the complete validation table.

### Minor: The public queue field bypassed the duplicate rule

External code could add rows directly through `Decisions::open`. The `open()` method already supplies read access.

I made the field private. All queue changes now use `push`, `take`, or `drop_for_task`.

### Minor: Some comments did not use Simplified Technical English

The comments used informal terms and long sentences. Some queue comments also promised an oldest-first order that the code did not calculate.

I used short, direct comments. The queue now documents its actual push order.

## Exact final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-13)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.07s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.08s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 143 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test decisions::tests::different_conditions_open_separate_rows ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test decisions::tests::drop_for_task_removes_only_that_tasks_rows ... ok
test decisions::tests::needs_human_and_gate_ids_derive_from_repo_and_item ... ok
test decisions::tests::needs_human_refuses_retry_because_the_label_can_outlive_its_task ... ok
test decisions::tests::open_lists_rows_in_push_order ... ok
test decisions::tests::pushing_one_condition_twice_keeps_one_row ... ok
test config::tests::example_file_parses_with_every_override ... ok
test decisions::tests::permission_and_question_ids_derive_from_task_and_request ... ok
test decisions::tests::decisions_and_responses_round_trip_through_json ... ok
test decisions::tests::pushing_one_condition_again_refreshes_its_data ... ok
test decisions::tests::stuck_ids_derive_from_task_and_attempt ... ok
test decisions::tests::take_removes_the_row_and_a_repeat_push_reopens_it ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test decisions::tests::the_table_accepts_every_legal_pair_and_refuses_every_other ... ok
test gates::tests::a_list_requires_a_separator_between_numbers ... ok
test gates::tests::a_list_stops_at_the_first_foreign_token ... ok
test exec::tests::script_returns_steps_in_order ... ok
test gates::tests::a_new_push_retriggers_review_but_an_unchanged_draft_does_not ... ok
test gates::tests::a_phrase_takes_a_list_of_numbers ... ok
test gates::tests::a_tab_does_not_separate_numbers ... ok
test gates::tests::a_steady_label_fires_once ... ok
test gates::tests::a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate ... ok
test gates::tests::an_implement_gate_stays_shut_while_a_dependency_is_open ... ok
test gates::tests::a_vanished_item_is_forgotten_and_can_fire_again_on_return ... ok
test gates::tests::blocked_by_collects_numbers_across_a_body ... ok
test gates::tests::blocked_by_ignores_bare_numbers_and_loose_text ... ok
test gates::tests::blocked_by_parses_all_three_phrasings_in_any_case ... ok
test gates::tests::forget_drops_memory_so_the_next_poll_fires_again ... ok
test gates::tests::implement_takes_refined_issues_without_to_refine ... ok
test gates::tests::implement_waits_for_open_dependencies ... ok
test gates::tests::refine_takes_open_issues_labelled_to_refine ... ok
test gates::tests::release_ready_pull_requests_are_reported_once ... ok
test gates::tests::removing_and_readding_a_label_fires_again ... ok
test gates::tests::repositories_are_tracked_independently ... ok
test gates::tests::review_takes_open_drafts_and_release_takes_ready_ones ... ok
test gates::tests::two_phrases_each_take_their_own_list ... ok
test gh::tests::a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page ... ok
test gh::tests::a_304_body_is_never_parsed ... ok
test gh::tests::a_304_page_known_to_be_short_ends_the_walk ... ok
test gh::tests::a_304_without_a_cached_page_is_an_error ... ok
test gh::tests::a_429_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_403_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_command_failure_without_a_response_head_keeps_stderr ... ok
test gh::tests::a_pull_without_a_draft_field_is_rejected ... ok
test gh::tests::add_label_posts_to_the_labels_endpoint ... ok
test gh::tests::a_pull_without_a_head_sha_is_rejected ... ok
test gh::tests::an_http_500_is_an_error_naming_the_status ... ok
test gh::tests::an_etag_from_one_repository_is_not_sent_to_another_repository ... ok
test gh::tests::an_unknown_item_state_is_rejected ... ok
test gh::tests::an_issue_without_labels_is_rejected ... ok
test gh::tests::an_issue_with_a_pull_request_key_never_appears_in_issues ... ok
test gh::tests::create_issue_returns_the_created_issue ... ok
test gh::tests::fetch_issues_runs_the_exact_gh_call_and_maps_the_items ... ok
test gh::tests::remove_label_sends_a_delete ... ok
test gh::tests::fetch_pulls_maps_draft_and_head_sha ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test tasks::tests::a_requeued_task_moves_to_the_back_of_the_order ... ok
test tasks::tests::a_transition_stamps_updated_ms_and_keeps_created_ms ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test tasks::tests::an_unknown_task_id_is_an_error ... ok
test tasks::tests::cancelling_a_queued_task_removes_it_from_active_tasks ... ok
test proc::tests::every_raw_stdout_line_reaches_the_log_byte_for_byte ... ok
test proc::tests::write_to_a_dead_child_returns_an_error ... ok
test tasks::tests::counts_by_stage_count_running_tasks_only ... ok
test tasks::tests::cancelling_records_the_cancelled_reason ... ok
test tasks::tests::counts_by_stage_repo_count_per_repository_and_stage ... ok
test gh::tests::a_304_page_without_a_cached_next_link_ends_pagination ... ok
test tasks::tests::new_builds_the_id_per_the_naming_rules ... ok
test proc::tests::stderr_lines_reach_the_log_with_a_prefix_and_no_events ... ok
test tasks::tests::running_and_active_keep_the_insertion_order ... ok
test tasks::tests::every_transition_in_the_matrix_follows_the_rules ... ok
test tasks::tests::task_state_round_trips_through_json ... ok
test tasks::tests::retries_count_attempts_up_to_the_limit ... ok
test tasks::tests::retry_past_max_attempts_is_refused_with_a_clear_message ... ok
test tasks::tests::upsert_inserts_new_tasks_in_insertion_order ... ok
test tasks::tests::upsert_keys_on_repo_stage_kind_and_number ... ok
test tasks::tests::upsert_replaces_a_terminal_task_with_a_fresh_queued_task ... ok
test tasks::tests::upsert_refuses_while_the_existing_task_is_active ... ok
test gh::tests::a_304_page_with_a_cached_next_page_does_not_end_the_walk ... ok
test worktree::tests::default_base_propagates_an_unexpected_git_failure ... ok
test worktree::tests::branch_lookup_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_rejects_an_empty_reference ... ok
test gh::tests::pagination_merges_two_pages_into_one_map ... ok
test proc::tests::spawn_creates_the_log_parent_and_appends_to_the_log ... ok
test proc::tests::exit_code_and_success_flag_are_reported_exactly_once ... ok
test worktree::tests::ensure_issue_adds_an_existing_branch_without_b ... ok
test worktree::tests::ensure_issue_builds_the_documented_commands ... ok
test worktree::tests::marker_write_failure_preserves_the_marker_and_removes_the_temporary_path ... ok
test worktree::tests::remove_issue_runs_remove_then_branch_delete ... ok
test worktree::tests::exists_issue_reports_registered_worktrees_only ... ok
test worktree::tests::marker_write_reports_a_temporary_cleanup_failure ... ok
test worktree::tests::ensure_train_resets_an_existing_worktree_through_the_documented_commands ... ok
test worktree::tests::markers_round_trip_and_leave_no_temporary_file ... ok
test proc::tests::write_after_exit_fails_when_a_descendant_keeps_stdin_open ... ok
test proc::tests::write_line_reaches_the_child_and_close_stdin_ends_it ... ok
test worktree::tests::ensure_issue_falls_back_to_head_without_origin ... ok
test proc::tests::terminate_runs_kill_term_through_the_injected_exec ... ok
test proc::tests::the_protocol_interrupt_can_stop_the_child_before_any_signal ... ok
test proc::tests::a_polite_child_dies_at_sigterm ... ok
test worktree::tests::ensure_issue_creates_a_worktree_at_the_documented_path ... ok
test worktree::tests::ensure_issue_twice_reuses_in_place ... ok
test worktree::tests::remove_issue_with_proof_removes_the_worktree_and_the_branch ... ok
test worktree::tests::ensure_issue_reuses_a_branch_whose_worktree_was_removed ... ok
test worktree::tests::the_aif_directory_is_invisible_to_git ... ok
test worktree::tests::ensure_train_creates_and_resets_to_the_default_branch ... ok
test proc::tests::a_protocol_interrupt_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::the_initial_grace_allows_a_natural_exit_without_a_protocol_interrupt ... ok
test proc::tests::a_sigterm_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::a_child_that_ignores_sigterm_reaches_sigkill ... ok
test proc::tests::stop_gracefully_does_not_block_the_caller ... ok

test result: ok. 143 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aifd_run_accepts_a_config_path ... ok
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aif_help_lists_all_subcommands ... ok
test aifd_run_help_lists_the_config_option ... ok
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

## Deliberately unchanged work

I did not add the GitHub comment or label actions. Chunk 15 owns those actions.

I did not edit `src/model.rs`, `src/exec.rs`, or another source file. I did not edit the old v0.4 directories.

## Final verdict

PASS
