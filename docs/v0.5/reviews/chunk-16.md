# Task 16 review - Control socket

## Acceptance criteria

| Acceptance criterion | Status | Evidence |
|---|---|---|
| A real socket round trip receives initial state, sends an action, and receives its effect. | MET | The round-trip test uses a temporary directory. It adds the queued task before the next push. |
| A subscriber that never reads is dropped. The daemon continues. | MET | The slow-client test fills the bounded channel. It rejects a timeout and confirms that a healthy client receives the final state. |
| Many rapid changes produce far fewer pushes. | MET | Twenty changes span about 100 ms. The test permits four pushes and verifies the newest state. |
| The socket file mode is 0600. | MET | The mode test reads the real socket metadata and checks the permission bits. |
| A stale socket file is replaced. | MET | The stale-socket test drops a real listener, binds again, and receives the initial state. |

## Findings and fixes

| Severity | Finding | Fix |
|---|---|---|
| Important | Subscriber removal dropped only a message sender. The server reader kept the socket open. The original shutdown test passed after a five-second timeout. | I added a shutdown handle to each subscriber. Subscriber removal now closes both socket directions. The tests now reject a read timeout. |
| Important | Separate current and pending state slots allowed stale or duplicate initial pushes during connection races. | I replaced both slots with one shared snapshot. The registry lock now orders initial and later pushes. Each subscriber skips its initial snapshot once. |
| Important | Bind removed any existing file after a failed connection. Server drop also removed a replacement at the socket path. | Bind now refuses a non-socket path. Server drop compares the saved file identity before removal. Both cases have regression tests. |
| Important | The implementation added the task 17 socket wait. Its 100 ms polling loop violated the global constraint and exceeded this chunk. | I removed the wait loop. The stop path now sends Action::Stop only. I removed the task 17 acceptance test. |
| Important | StateInput::build silently omitted an ordered task when the task map lacked its ID. | The function now returns anyhow::Result. It reports the missing task ID. A regression test covers the error. |
| Important | Several socket errors were discarded. A failed shared lock also received no report. | The code now logs write, flush, read, delivery, wake, cleanup, and lock errors. A full message queue still causes subscriber removal. |
| Important | Three acceptance tests were too weak. The round trip did not show an action effect. The coalescing test allowed almost every push. The slow-client test accepted a timeout. | I added a visible queued-task effect. I set a strict coalescing limit. I made timeout a failure for the dropped client. |
| Minor | The wake message queue had no size limit. Rapid state changes could queue useless wake messages and duplicate large snapshots. | I use a one-slot wake queue and one shared snapshot. New publishes replace the current snapshot. |
| Minor | The stale-socket fixture used unnecessary unsafe Rust code. | The test now drops the UnixListener. The socket path remains for the stale bind test. |

## Exact final ./check.sh output

```text
== fmt ==
== clippy ==
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.06s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.06s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 227 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test decisions::tests::different_conditions_open_separate_rows ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test decisions::tests::open_lists_rows_in_push_order ... ok
test decisions::tests::pushing_one_condition_again_refreshes_its_data ... ok
test decisions::tests::permission_and_question_ids_derive_from_task_and_request ... ok
test decisions::tests::needs_human_and_gate_ids_derive_from_repo_and_item ... ok
test decisions::tests::drop_for_task_removes_only_that_tasks_rows ... ok
test decisions::tests::stuck_ids_derive_from_task_and_attempt ... ok
test decisions::tests::needs_human_refuses_retry_because_the_label_can_outlive_its_task ... ok
test decisions::tests::take_removes_the_row_and_a_repeat_push_reopens_it ... ok
test decisions::tests::pushing_one_condition_twice_keeps_one_row ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_returns_steps_in_order ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test gates::tests::a_list_requires_a_separator_between_numbers ... ok
test decisions::tests::decisions_and_responses_round_trip_through_json ... ok
test decisions::tests::the_table_accepts_every_legal_pair_and_refuses_every_other ... ok
test gates::tests::a_list_stops_at_the_first_foreign_token ... ok
test gates::tests::a_new_push_retriggers_review_but_an_unchanged_draft_does_not ... ok
test gates::tests::a_phrase_takes_a_list_of_numbers ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test gates::tests::a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate ... ok
test gates::tests::a_tab_does_not_separate_numbers ... ok
test gates::tests::a_steady_label_fires_once ... ok
test gates::tests::a_vanished_item_is_forgotten_and_can_fire_again_on_return ... ok
test gates::tests::blocked_by_ignores_bare_numbers_and_loose_text ... ok
test gates::tests::blocked_by_parses_all_three_phrasings_in_any_case ... ok
test gates::tests::blocked_by_collects_numbers_across_a_body ... ok
test gates::tests::an_implement_gate_stays_shut_while_a_dependency_is_open ... ok
test config::tests::example_file_parses_with_every_override ... ok
test gates::tests::forget_drops_memory_so_the_next_poll_fires_again ... ok
test gates::tests::refine_takes_open_issues_labelled_to_refine ... ok
test gates::tests::implement_takes_refined_issues_without_to_refine ... ok
test gates::tests::release_ready_pull_requests_are_reported_once ... ok
test gates::tests::removing_and_readding_a_label_fires_again ... ok
test gates::tests::implement_waits_for_open_dependencies ... ok
test gh::tests::a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page ... ok
test gates::tests::repositories_are_tracked_independently ... ok
test gates::tests::review_takes_open_drafts_and_release_takes_ready_ones ... ok
test gh::tests::a_304_body_is_never_parsed ... ok
test gh::tests::a_304_page_known_to_be_short_ends_the_walk ... ok
test gates::tests::two_phrases_each_take_their_own_list ... ok
test gh::tests::a_304_without_a_cached_page_is_an_error ... ok
test gh::tests::a_403_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_429_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_command_failure_without_a_response_head_keeps_stderr ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test gh::tests::a_pull_without_a_draft_field_is_rejected ... ok
test gh::tests::a_pull_without_a_head_sha_is_rejected ... ok
test gh::tests::add_label_posts_to_the_labels_endpoint ... ok
test gh::tests::an_http_500_is_an_error_naming_the_status ... ok
test gh::tests::an_etag_from_one_repository_is_not_sent_to_another_repository ... ok
test gh::tests::an_issue_without_labels_is_rejected ... ok
test gh::tests::an_issue_with_a_pull_request_key_never_appears_in_issues ... ok
test gh::tests::an_unknown_item_state_is_rejected ... ok
test gh::tests::create_issue_returns_the_created_issue ... ok
test gh::tests::fetch_issues_runs_the_exact_gh_call_and_maps_the_items ... ok
test gh::tests::fetch_pulls_maps_draft_and_head_sha ... ok
test gh::tests::remove_label_sends_a_delete ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test poll::tests::a_closed_daemon_channel_returns_the_send_error ... ok
test poll::tests::daemon_msg_has_the_shutdown_variant ... ok
test poll::tests::the_backoff_doubles_and_caps ... ok
test gh::tests::a_304_page_without_a_cached_next_link_ends_pagination ... ok
test poll::tests::a_wake_forces_an_early_pass_and_merges_the_unchanged_pages ... ok
test proc::tests::stderr_lines_reach_the_log_with_a_prefix_and_no_events ... ok
test proc::tests::every_raw_stdout_line_reaches_the_log_byte_for_byte ... ok
test gh::tests::pagination_merges_two_pages_into_one_map ... ok
test proc::tests::write_to_a_dead_child_returns_an_error ... ok
test poll::tests::the_wake_map_holds_one_sender_per_repository ... ok
test runner::opencode::tests::a_malformed_line_is_skipped_and_the_run_continues ... ok
test runner::opencode::tests::a_step_finish_with_an_error_reason_fails_the_turn ... ok
test runner::opencode::tests::a_tool_part_without_a_title_falls_back_to_the_truncated_part ... ok
test runner::opencode::tests::a_tool_use_line_without_a_tool_part_is_ignored ... ok
test runner::opencode::tests::an_invalid_top_level_session_id_does_not_hide_the_part_session_id ... ok
test runner::opencode::tests::a_tool_part_yields_a_tool_event_whatever_the_line_type ... ok
test runner::opencode::tests::an_unknown_line_type_is_ignored_without_stopping_the_run ... ok
test gh::tests::a_304_page_with_a_cached_next_page_does_not_end_the_walk ... ok
test runner::opencode::tests::only_the_first_step_start_emits_started ... ok
test proc::tests::exit_code_and_success_flag_are_reported_exactly_once ... ok
test runner::opencode::tests::the_argument_vector_carries_the_variant_when_set ... ok
test runner::opencode::tests::the_argument_vector_matches_the_verified_invocation ... ok
test runner::opencode::tests::the_session_id_comes_from_the_first_line_that_carries_one ... ok
test runner::opencode::tests::fixture_replay_produces_the_expected_run_events ... ok
test proc::tests::write_line_reaches_the_child_and_close_stdin_ends_it ... ok
test proc::tests::spawn_creates_the_log_parent_and_appends_to_the_log ... ok
test runner::tests::the_default_session_methods_refuse_steering ... ok
test sched::tests::a_paused_stage_leaves_other_stages_free ... ok
test sched::tests::an_empty_table_yields_no_dispatch ... ok
test sched::tests::a_repository_without_reservation_uses_all_remaining_capacity ... ok
test sched::tests::a_reserved_slot_stays_free_while_another_repository_has_queued_work ... ok
test sched::tests::can_start_names_the_reasons_in_order ... ok
test sched::tests::awaiting_user_tasks_hold_no_scheduler_slot ... ok
test sched::tests::excessive_runtime_lane_values_still_block_unreserved_work ... ok
test sched::tests::dispatch_preserves_insertion_order_and_never_starves_the_head_task ... ok
test sched::tests::excessive_runtime_lane_values_still_produce_a_warning ... ok
test sched::tests::next_dispatch_skips_tasks_that_are_not_queued ... ok
test sched::tests::pausing_blocks_dispatch_and_reports_the_right_reason ... ok
test sched::tests::reservations_covering_the_limit_produce_a_warning ... ok
test sched::tests::the_reserving_repository_uses_its_slot_at_once ... ok
test sched::tests::limits_build_from_a_config ... ok
test sock::tests::a_state_view_round_trips_through_json ... ok
test sock::tests::a_client_cannot_connect_to_a_missing_daemon ... ok
test sock::tests::bind_creates_the_socket_with_mode_0600 ... ok
test runner::opencode::tests::a_fake_opencode_child_drives_the_full_run ... ok
test runner::opencode::tests::stop_kills_the_child ... ok
test sock::tests::bind_replaces_a_stale_socket_file ... ok
test sock::tests::broadcast_skips_the_initial_push_that_a_subscriber_already_received ... ok
test sock::tests::every_action_round_trips_through_one_json_line ... ok
test proc::tests::the_protocol_interrupt_can_stop_the_child_before_any_signal ... ok
test sock::tests::the_view_build_describes_every_merged_module ... ok
test sock::tests::the_view_build_rejects_a_missing_ordered_task ... ok
test sock::tests::the_wire_shapes_use_the_documented_tags ... ok
test tasks::tests::a_requeued_task_moves_to_the_back_of_the_order ... ok
test tasks::tests::a_transition_stamps_updated_ms_and_keeps_created_ms ... ok
test tasks::tests::an_unknown_task_id_is_an_error ... ok
test tasks::tests::cancelling_a_queued_task_removes_it_from_active_tasks ... ok
test tasks::tests::cancelling_records_the_cancelled_reason ... ok
test tasks::tests::counts_by_stage_count_running_tasks_only ... ok
test tasks::tests::counts_by_stage_repo_count_per_repository_and_stage ... ok
test tasks::tests::every_transition_in_the_matrix_follows_the_rules ... ok
test tasks::tests::new_builds_the_id_per_the_naming_rules ... ok
test tasks::tests::retries_count_attempts_up_to_the_limit ... ok
test tasks::tests::retry_past_max_attempts_is_refused_with_a_clear_message ... ok
test tasks::tests::running_and_active_keep_the_insertion_order ... ok
test tasks::tests::task_state_round_trips_through_json ... ok
test tasks::tests::upsert_inserts_new_tasks_in_insertion_order ... ok
test tasks::tests::upsert_keys_on_repo_stage_kind_and_number ... ok
test tasks::tests::upsert_refuses_while_the_existing_task_is_active ... ok
test tasks::tests::upsert_replaces_a_terminal_task_with_a_fresh_queued_task ... ok
test trains::tests::a_draft_pr_does_not_return_after_an_in_flight_failure ... ok
test trains::tests::a_failed_train_refuses_a_different_retry_set ... ok
test trains::tests::a_failed_train_returns_its_prs_and_a_retry_reuses_the_same_set ... ok
test trains::tests::a_label_error_keeps_the_train_in_flight_for_a_finish_retry ... ok
test trains::tests::a_poll_keeps_an_in_flight_label_for_success_cleanup ... ok
test trains::tests::a_saturated_interval_fires_at_its_returned_deadline ... ok
test trains::tests::a_second_fire_while_in_flight_is_refused ... ok
test trains::tests::a_stacked_subset_fires_instead_of_the_whole_queue ... ok
test trains::tests::a_successful_train_drains_the_batch_and_clears_the_labels ... ok
test trains::tests::a_successful_unstacked_train_makes_no_label_call ... ok
test trains::tests::a_threshold_policy_fires_when_the_count_is_reached ... ok
test trains::tests::a_threshold_train_does_not_fire_again_while_in_flight ... ok
test trains::tests::an_interval_policy_fires_only_at_or_after_its_deadline ... ok
test trains::tests::an_interval_train_that_never_fired_is_due_now ... ok
test trains::tests::an_interval_train_with_an_empty_queue_has_no_deadline ... ok
test trains::tests::dequeue_removes_a_pr_from_the_queue_and_the_cache ... ok
test trains::tests::enqueue_adds_a_ready_pr_once ... ok
test trains::tests::finish_without_a_train_touches_nothing ... ok
test trains::tests::firing_a_duplicate_pr_is_refused ... ok
test trains::tests::firing_a_pr_outside_the_queue_is_refused ... ok
test trains::tests::firing_an_empty_set_is_refused ... ok
test trains::tests::manual_never_fires_but_fire_works ... ok
test trains::tests::rebuild_stacked_keeps_only_queued_prs_in_queue_order ... ok
test trains::tests::stack_adds_the_github_label_and_updates_the_cache ... ok
test trains::tests::stacking_a_pr_that_is_not_queued_is_refused ... ok
test trains::tests::stacking_twice_makes_one_label_call ... ok
test trains::tests::the_next_deadline_is_the_interval_fire_moment ... ok
test trains::tests::the_task_id_names_the_lowest_pr_of_the_batch ... ok
test trains::tests::unstack_removes_the_github_label_and_the_cache_entry ... ok
test trains::tests::unstacking_an_absent_pr_is_a_no_op ... ok
test worktree::tests::branch_lookup_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_rejects_an_empty_reference ... ok
test sock::tests::bind_creates_the_socket_directory ... ok
test sock::tests::bind_refuses_to_replace_a_plain_file_at_the_socket_path ... ok
test sock::tests::a_client_that_goes_away_does_not_harm_the_daemon ... ok
test worktree::tests::ensure_issue_adds_an_existing_branch_without_b ... ok
test sock::tests::bind_refuses_a_path_with_a_live_daemon ... ok
test sock::tests::a_server_drop_preserves_a_replacement_at_the_socket_path ... ok
test sock::tests::a_server_drop_removes_the_socket_file_and_closes_clients ... ok
test worktree::tests::ensure_issue_builds_the_documented_commands ... ok
test proc::tests::write_after_exit_fails_when_a_descendant_keeps_stdin_open ... ok
test worktree::tests::exists_issue_reports_registered_worktrees_only ... ok
test worktree::tests::ensure_train_resets_an_existing_worktree_through_the_documented_commands ... ok
test worktree::tests::marker_write_failure_preserves_the_marker_and_removes_the_temporary_path ... ok
test worktree::tests::marker_write_reports_a_temporary_cleanup_failure ... ok
test worktree::tests::remove_issue_runs_remove_then_branch_delete ... ok
test worktree::tests::markers_round_trip_and_leave_no_temporary_file ... ok
test proc::tests::terminate_runs_kill_term_through_the_injected_exec ... ok
test sock::tests::a_round_trip_connects_answers_and_pushes_again ... ok
test worktree::tests::ensure_issue_falls_back_to_head_without_origin ... ok
test worktree::tests::the_aif_directory_is_invisible_to_git ... ok
test proc::tests::a_polite_child_dies_at_sigterm ... ok
test worktree::tests::ensure_issue_creates_a_worktree_at_the_documented_path ... ok
test worktree::tests::remove_issue_with_proof_removes_the_worktree_and_the_branch ... ok
test worktree::tests::ensure_issue_twice_reuses_in_place ... ok
test worktree::tests::ensure_train_creates_and_resets_to_the_default_branch ... ok
test worktree::tests::ensure_issue_reuses_a_branch_whose_worktree_was_removed ... ok
test poll::tests::failures_back_off_and_the_thread_stays_alive ... ok
test poll::tests::a_wake_reaches_only_its_own_repository ... ok
test proc::tests::a_protocol_interrupt_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::the_initial_grace_allows_a_natural_exit_without_a_protocol_interrupt ... ok
test proc::tests::stop_gracefully_does_not_block_the_caller ... ok
test proc::tests::a_child_that_ignores_sigterm_reaches_sigkill ... ok
test proc::tests::a_sigterm_error_is_reported_when_the_child_later_exits ... ok
test sock::tests::rapid_publishes_coalesce_into_few_pushes_and_the_last_one_wins ... ok
test sock::tests::a_subscriber_that_never_reads_is_dropped_and_the_daemon_keeps_running ... ok

test result: ok. 227 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.75s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aif_help_lists_all_subcommands ... ok
test aifd_run_accepts_a_config_path ... ok
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

## Deliberate exclusions

- I left src/model.rs and src/exec.rs unchanged because the worktree instructions freeze them.
- I left the old v0.4 tree unchanged because chunk 22 owns those files.
- I left the task 17 stop-wait behavior for chunk 17. Task 16 now sends the stop action only.
- I found no unresolved defect in the owned scope.

PASS
