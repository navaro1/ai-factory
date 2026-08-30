# Task 20 review

## Acceptance criteria

| Acceptance criterion | Status | Evidence |
|---|---|---|
| A claude assistant text line has a renderer unit test. | MET | `a_claude_assistant_text_line_renders_as_plain_text` tests parsed text and visible style. |
| A claude `tool_use` line has a renderer unit test. | MET | `a_claude_tool_use_line_renders_dim_and_prefixed` tests the prefix, summary, and dim style. |
| An opencode text line has a renderer unit test. | MET | `an_opencode_text_line_renders_the_text` tests the verified `part.text` shape. |
| A malformed line has a renderer unit test. | MET | `a_malformed_line_renders_as_a_dim_raw_line_and_is_never_dropped` tests content and dim style. |
| A very narrow width has a renderer unit test. | MET | `a_very_narrow_width_wraps_every_line_inside_the_width` tests widths 5 and 0. The test now uses terminal widths. |
| The ring buffer never exceeds its bound. | MET | `the_ring_buffer_never_exceeds_its_bound` pushes 2500 items into a 2000-item ring. |
| `End` resumes tail following after upward scroll. | MET | `tail_following_resumes_with_end_after_scrolling_up` tests PageUp, PageDown, and End. |
| Typing and Enter send one `Action::Chat` with the typed text. | MET | `typing_and_enter_sends_one_chat_action` tests the task, exact text, and one action. |

## Findings and fixes

### Important: Opencode tool detection depended on the outer line type

The verified protocol requires a match on `part.type == "tool"`.
The parser first required the outer type to equal `tool_use`.
Thus, another outer type rendered the tool event as raw text.

I moved the tool-part check before the outer type match.
I added a regression test with an unknown outer type.

### Important: Wrapped text could exceed the pane width

The renderer counted Rust characters instead of terminal columns.
CJK text could exceed the pane width.
A two-column prefix also exceeded widths 0, 1, and 2.
The original narrow-width test permitted this overflow at width 0.

I now use Ratatui terminal widths.
I split wide text by terminal columns.
I render a clipped prefix on a separate line when necessary.
I strengthened the narrow-width test and added CJK coverage.

### Important: Claude subagent output violated the verified skip rule

The verified facts require the renderer to skip assistant blocks with a non-null `parent_tool_use_id`.
The implementation showed the nested content as a system note.

I now exclude the complete line.
I changed the test to assert that the parser returns no entry.

### Minor: Chat input changed the typed text

The input handler trimmed leading and trailing spaces before it built `Action::Chat`.
The acceptance criterion requires the typed text.

I now use trimming only to reject blank input.
The action carries the exact nonblank input.
The test now covers leading and trailing spaces.

## Constraint and scope review

The diff adds no dependency, asynchronous code, polling loop, tick thread, journal, or domain-state lock.
The tests use no network or real external tool.
Each new test has a behavior assertion.
Production code has no `unwrap()` or `expect()` call.

The implementation changed only `src/tui/session.rs` and `src/tui/transcript.rs`.
These are the two files in the task brief.
The review changed those files and this required review file.

I did not edit `src/model.rs` or `src/exec.rs`.
I did not edit `ui/console/`, `zellij/`, or `bin/`.

## Deliberately left alone

I left `src/tui/mod.rs` integration unchanged because chunk 18 owns that file.
This isolated branch can test the session component but cannot test the final shell connection.
I found no unresolved defect in the owned files.

## Exact final `./check.sh` output

```text
== fmt ==
== clippy ==
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.06s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.06s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 288 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test decisions::tests::different_conditions_open_separate_rows ... ok
test decisions::tests::needs_human_and_gate_ids_derive_from_repo_and_item ... ok
test decisions::tests::needs_human_refuses_retry_because_the_label_can_outlive_its_task ... ok
test decisions::tests::open_lists_rows_in_push_order ... ok
test decisions::tests::drop_for_task_removes_only_that_tasks_rows ... ok
test decisions::tests::pushing_one_condition_again_refreshes_its_data ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test decisions::tests::permission_and_question_ids_derive_from_task_and_request ... ok
test decisions::tests::stuck_ids_derive_from_task_and_attempt ... ok
test decisions::tests::pushing_one_condition_twice_keeps_one_row ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test decisions::tests::take_removes_the_row_and_a_repeat_push_reopens_it ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_returns_steps_in_order ... ok
test gates::tests::a_list_requires_a_separator_between_numbers ... ok
test gates::tests::a_steady_label_fires_once ... ok
test gates::tests::a_phrase_takes_a_list_of_numbers ... ok
test gates::tests::a_list_stops_at_the_first_foreign_token ... ok
test gates::tests::blocked_by_collects_numbers_across_a_body ... ok
test gates::tests::an_implement_gate_stays_shut_while_a_dependency_is_open ... ok
test gates::tests::a_new_push_retriggers_review_but_an_unchanged_draft_does_not ... ok
test decisions::tests::decisions_and_responses_round_trip_through_json ... ok
test gates::tests::a_tab_does_not_separate_numbers ... ok
test gates::tests::a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate ... ok
test gates::tests::blocked_by_ignores_bare_numbers_and_loose_text ... ok
test gates::tests::blocked_by_parses_all_three_phrasings_in_any_case ... ok
test gates::tests::forget_drops_memory_so_the_next_poll_fires_again ... ok
test gates::tests::implement_takes_refined_issues_without_to_refine ... ok
test gates::tests::a_vanished_item_is_forgotten_and_can_fire_again_on_return ... ok
test gates::tests::refine_takes_open_issues_labelled_to_refine ... ok
test gates::tests::removing_and_readding_a_label_fires_again ... ok
test gates::tests::release_ready_pull_requests_are_reported_once ... ok
test gates::tests::review_takes_open_drafts_and_release_takes_ready_ones ... ok
test decisions::tests::the_table_accepts_every_legal_pair_and_refuses_every_other ... ok
test gates::tests::repositories_are_tracked_independently ... ok
test gates::tests::implement_waits_for_open_dependencies ... ok
test gates::tests::two_phrases_each_take_their_own_list ... ok
test gh::tests::a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page ... ok
test gh::tests::a_304_page_known_to_be_short_ends_the_walk ... ok
test gh::tests::a_304_body_is_never_parsed ... ok
test gh::tests::a_304_without_a_cached_page_is_an_error ... ok
test config::tests::example_file_parses_with_every_override ... ok
test gh::tests::a_403_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::add_label_posts_to_the_labels_endpoint ... ok
test gh::tests::a_pull_without_a_draft_field_is_rejected ... ok
test gh::tests::a_pull_without_a_head_sha_is_rejected ... ok
test gh::tests::an_http_500_is_an_error_naming_the_status ... ok
test gh::tests::an_issue_without_labels_is_rejected ... ok
test gh::tests::an_unknown_item_state_is_rejected ... ok
test gh::tests::an_etag_from_one_repository_is_not_sent_to_another_repository ... ok
test gh::tests::an_issue_with_a_pull_request_key_never_appears_in_issues ... ok
test gh::tests::a_429_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::create_issue_returns_the_created_issue ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test gh::tests::a_command_failure_without_a_response_head_keeps_stderr ... ok
test gh::tests::fetch_pulls_maps_draft_and_head_sha ... ok
test gh::tests::remove_label_sends_a_delete ... ok
test gh::tests::fetch_issues_runs_the_exact_gh_call_and_maps_the_items ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test poll::tests::daemon_msg_has_the_shutdown_variant ... ok
test poll::tests::a_closed_daemon_channel_returns_the_send_error ... ok
test poll::tests::the_backoff_doubles_and_caps ... ok
test poll::tests::a_wake_forces_an_early_pass_and_merges_the_unchanged_pages ... ok
test poll::tests::the_wake_map_holds_one_sender_per_repository ... ok
test proc::tests::stderr_lines_reach_the_log_with_a_prefix_and_no_events ... ok
test proc::tests::write_after_exit_fails_when_a_descendant_keeps_stdin_open ... ok
test runner::claude::tests::a_can_use_tool_request_defaults_the_human_flag_to_false ... ok
test proc::tests::every_raw_stdout_line_reaches_the_log_byte_for_byte ... ok
test proc::tests::exit_code_and_success_flag_are_reported_exactly_once ... ok
test runner::claude::tests::a_can_use_tool_request_parses_into_a_tool_ask ... ok
test gh::tests::pagination_merges_two_pages_into_one_map ... ok
test gh::tests::a_304_page_without_a_cached_next_link_ends_pagination ... ok
test gh::tests::a_304_page_with_a_cached_next_page_does_not_end_the_walk ... ok
test runner::claude::tests::a_non_control_line_cannot_create_a_tool_ask ... ok
test proc::tests::spawn_creates_the_log_parent_and_appends_to_the_log ... ok
test proc::tests::write_to_a_dead_child_returns_an_error ... ok
test runner::claude::tests::a_child_that_exits_during_the_handshake_fails_the_job ... ok
test runner::claude::tests::an_error_control_response_fails_the_handshake ... ok
test runner::claude::tests::a_human_question_is_never_auto_answered_even_under_yolo ... ok
test runner::claude::tests::fixture_replay_produces_the_expected_run_events ... ok
test runner::claude::tests::an_ordinary_ask_reaches_the_caller_and_a_deny_goes_out ... ok
test runner::claude::tests::an_unknown_request_id_is_an_error_and_a_plain_allow_echoes_the_input ... ok
test runner::claude::tests::dropping_a_session_stops_its_child_with_an_interrupt ... ok
test runner::claude::tests::the_argument_vector_matches_the_verified_invocation ... ok
test runner::claude::tests::the_initialize_request_matches_the_verified_shape ... ok
test runner::claude::tests::the_interrupt_line_carries_a_fresh_uuid_request_id ... ok
test runner::claude::tests::the_resume_argument_vector_carries_resume_and_no_session_id ... ok
test runner::claude::tests::stop_clears_every_pending_ask ... ok
test runner::claude::tests::stop_writes_the_interrupt_line_before_any_signal ... ok
test runner::claude::tests::tool_summaries_use_command_path_and_truncated_json ... ok
test runner::claude::tests::the_resume_job_resumes_without_minting_a_session_id ... ok
test runner::claude::tests::the_session_id_callback_fires_once_with_the_minted_id ... ok
test runner::claude::tests::user_lines_reach_the_child_in_the_exact_wire_shape ... ok
test runner::opencode::tests::a_malformed_line_is_skipped_and_the_run_continues ... ok
test runner::opencode::tests::a_step_finish_with_an_error_reason_fails_the_turn ... ok
test runner::opencode::tests::a_tool_part_without_a_title_falls_back_to_the_truncated_part ... ok
test runner::opencode::tests::a_tool_part_yields_a_tool_event_whatever_the_line_type ... ok
test runner::opencode::tests::a_tool_use_line_without_a_tool_part_is_ignored ... ok
test runner::opencode::tests::an_invalid_top_level_session_id_does_not_hide_the_part_session_id ... ok
test runner::opencode::tests::an_unknown_line_type_is_ignored_without_stopping_the_run ... ok
test runner::opencode::tests::fixture_replay_produces_the_expected_run_events ... ok
test runner::opencode::tests::only_the_first_step_start_emits_started ... ok
test runner::claude::tests::a_fake_child_drives_the_full_happy_path ... ok
test runner::opencode::tests::the_argument_vector_carries_the_variant_when_set ... ok
test runner::opencode::tests::the_argument_vector_matches_the_verified_invocation ... ok
test runner::claude::tests::a_matching_response_without_success_fails_the_handshake ... ok
test runner::opencode::tests::the_session_id_comes_from_the_first_line_that_carries_one ... ok
test proc::tests::write_line_reaches_the_child_and_close_stdin_ends_it ... ok
test runner::tests::the_default_session_methods_refuse_steering ... ok
test sched::tests::a_paused_stage_leaves_other_stages_free ... ok
test sched::tests::a_repository_without_reservation_uses_all_remaining_capacity ... ok
test sched::tests::an_empty_table_yields_no_dispatch ... ok
test sched::tests::a_reserved_slot_stays_free_while_another_repository_has_queued_work ... ok
test runner::opencode::tests::a_fake_opencode_child_drives_the_full_run ... ok
test sched::tests::awaiting_user_tasks_hold_no_scheduler_slot ... ok
test sched::tests::can_start_names_the_reasons_in_order ... ok
test sched::tests::excessive_runtime_lane_values_still_block_unreserved_work ... ok
test sched::tests::excessive_runtime_lane_values_still_produce_a_warning ... ok
test sched::tests::dispatch_preserves_insertion_order_and_never_starves_the_head_task ... ok
test sched::tests::next_dispatch_skips_tasks_that_are_not_queued ... ok
test runner::claude::tests::yolo_auto_allows_an_ordinary_ask_in_the_verified_shape ... ok
test sched::tests::pausing_blocks_dispatch_and_reports_the_right_reason ... ok
test sched::tests::reservations_covering_the_limit_produce_a_warning ... ok
test sched::tests::the_reserving_repository_uses_its_slot_at_once ... ok
test sched::tests::limits_build_from_a_config ... ok
test sock::tests::a_client_cannot_connect_to_a_missing_daemon ... ok
test runner::opencode::tests::stop_kills_the_child ... ok
test sock::tests::a_state_view_round_trips_through_json ... ok
test sock::tests::a_server_drop_preserves_a_replacement_at_the_socket_path ... ok
test sock::tests::a_client_that_goes_away_does_not_harm_the_daemon ... ok
test sock::tests::bind_creates_the_socket_with_mode_0600 ... ok
test sock::tests::a_server_drop_removes_the_socket_file_and_closes_clients ... ok
test sock::tests::bind_creates_the_socket_directory ... ok
test sock::tests::broadcast_skips_the_initial_push_that_a_subscriber_already_received ... ok
test sock::tests::bind_refuses_a_path_with_a_live_daemon ... ok
test sock::tests::bind_refuses_to_replace_a_plain_file_at_the_socket_path ... ok
test sock::tests::every_action_round_trips_through_one_json_line ... ok
test sock::tests::bind_replaces_a_stale_socket_file ... ok
test sock::tests::the_wire_shapes_use_the_documented_tags ... ok
test tasks::tests::a_requeued_task_moves_to_the_back_of_the_order ... ok
test sock::tests::the_view_build_describes_every_merged_module ... ok
test sock::tests::the_view_build_rejects_a_missing_ordered_task ... ok
test tasks::tests::a_transition_stamps_updated_ms_and_keeps_created_ms ... ok
test tasks::tests::an_unknown_task_id_is_an_error ... ok
test tasks::tests::cancelling_a_queued_task_removes_it_from_active_tasks ... ok
test tasks::tests::cancelling_records_the_cancelled_reason ... ok
test tasks::tests::counts_by_stage_count_running_tasks_only ... ok
test tasks::tests::counts_by_stage_repo_count_per_repository_and_stage ... ok
test tasks::tests::new_builds_the_id_per_the_naming_rules ... ok
test tasks::tests::retries_count_attempts_up_to_the_limit ... ok
test tasks::tests::every_transition_in_the_matrix_follows_the_rules ... ok
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
test tui::session::tests::a_missing_log_file_is_quiet_until_it_appears ... ok
test tui::session::tests::an_empty_or_blank_input_sends_nothing ... ok
test tui::session::tests::a_partial_line_waits_for_its_newline ... ok
test tui::session::tests::ask_questions_reads_the_recorded_payload_shape ... ok
test tui::session::tests::keys_without_a_shown_task_send_nothing ... ok
test tui::session::tests::ctrl_x_aborts_the_shown_task ... ok
test tui::session::tests::tail_following_resumes_with_end_after_scrolling_up ... ok
test tui::session::tests::the_draw_fits_into_a_one_row_area ... ok
test tui::session::tests::showing_a_different_task_resets_the_transcript ... ok
test tui::session::tests::the_draw_shows_the_header_the_transcript_and_the_input_bar ... ok
test tui::session::tests::the_poll_reads_at_most_once_per_interval ... ok
test tui::session::tests::the_ring_buffer_never_exceeds_its_bound ... ok
test tui::session::tests::the_draw_shows_a_pending_ask_for_this_task_only ... ok
test tui::session::tests::typing_and_enter_sends_one_chat_action ... ok
test tui::session::tests::the_session_view_tails_the_log_file ... ok
test tui::transcript::tests::a_claude_assistant_text_line_renders_as_plain_text ... ok
test tui::transcript::tests::a_claude_failed_tool_result_is_marked ... ok
test tui::transcript::tests::a_claude_result_line_shows_the_outcome_and_the_cost ... ok
test tui::transcript::tests::a_claude_tool_use_line_renders_dim_and_prefixed ... ok
test tui::transcript::tests::a_claude_user_message_renders_as_the_human_voice ... ok
test tui::transcript::tests::a_failed_claude_result_line_is_marked ... ok
test tui::transcript::tests::a_malformed_line_renders_as_a_dim_raw_line_and_is_never_dropped ... ok
test tui::transcript::tests::a_known_type_with_an_unusable_payload_falls_back_to_raw ... ok
test tui::transcript::tests::a_subagent_line_is_skipped ... ok
test tui::transcript::tests::an_empty_line_parses_to_nothing ... ok
test tui::transcript::tests::an_opencode_step_finish_renders_per_step ... ok
test tui::transcript::tests::a_very_narrow_width_wraps_every_line_inside_the_width ... ok
test tui::transcript::tests::an_opencode_text_line_renders_the_text ... ok
test tui::transcript::tests::an_opencode_tool_part_does_not_depend_on_the_outer_line_type ... ok
test tui::transcript::tests::prefixed_and_wide_text_stays_inside_the_pane_width ... ok
test tui::transcript::tests::an_opencode_tool_line_matches_on_the_part_and_marks_errors ... ok
test tui::transcript::tests::system_and_control_lines_render_as_dim_notes ... ok
test tui::transcript::tests::wrap_respects_the_width_and_splits_long_words ... ok
test worktree::tests::branch_lookup_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_rejects_an_empty_reference ... ok
test worktree::tests::ensure_issue_builds_the_documented_commands ... ok
test worktree::tests::ensure_issue_adds_an_existing_branch_without_b ... ok
test tui::session::tests::the_draw_marks_an_older_hidden_history ... ok
test proc::tests::the_protocol_interrupt_can_stop_the_child_before_any_signal ... ok
test proc::tests::a_polite_child_dies_at_sigterm ... ok
test worktree::tests::ensure_issue_falls_back_to_head_without_origin ... ok
test worktree::tests::ensure_train_resets_an_existing_worktree_through_the_documented_commands ... ok
test worktree::tests::exists_issue_reports_registered_worktrees_only ... ok
test worktree::tests::marker_write_failure_preserves_the_marker_and_removes_the_temporary_path ... ok
test worktree::tests::marker_write_reports_a_temporary_cleanup_failure ... ok
test worktree::tests::markers_round_trip_and_leave_no_temporary_file ... ok
test worktree::tests::remove_issue_runs_remove_then_branch_delete ... ok
test proc::tests::terminate_runs_kill_term_through_the_injected_exec ... ok
test sock::tests::a_round_trip_connects_answers_and_pushes_again ... ok
test worktree::tests::ensure_issue_creates_a_worktree_at_the_documented_path ... ok
test worktree::tests::ensure_issue_twice_reuses_in_place ... ok
test worktree::tests::ensure_issue_reuses_a_branch_whose_worktree_was_removed ... ok
test worktree::tests::ensure_train_creates_and_resets_to_the_default_branch ... ok
test poll::tests::failures_back_off_and_the_thread_stays_alive ... ok
test worktree::tests::remove_issue_with_proof_removes_the_worktree_and_the_branch ... ok
test worktree::tests::the_aif_directory_is_invisible_to_git ... ok
test runner::claude::tests::idle_for_grows_between_events_and_resets_on_one ... ok
test poll::tests::a_wake_reaches_only_its_own_repository ... ok
test proc::tests::a_sigterm_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::the_initial_grace_allows_a_natural_exit_without_a_protocol_interrupt ... ok
test proc::tests::a_protocol_interrupt_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::a_child_that_ignores_sigterm_reaches_sigkill ... ok
test proc::tests::stop_gracefully_does_not_block_the_caller ... ok
test runner::claude::tests::a_missing_control_response_fails_the_job_naming_the_handshake ... ok
test runner::claude::tests::a_noisy_child_cannot_postpone_the_handshake_timeout ... ok
test runner::claude::tests::an_unrelated_control_response_does_not_finish_the_handshake ... ok
test sock::tests::rapid_publishes_coalesce_into_few_pushes_and_the_last_one_wins ... ok
test sock::tests::a_subscriber_that_never_reads_is_dropped_and_the_daemon_keeps_running ... ok

test result: ok. 288 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.80s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aif_help_lists_all_subcommands ... ok
test aifd_run_help_lists_the_config_option ... ok
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aifd_run_accepts_a_config_path ... ok
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

Final verdict: PASS
