# Task 19 review

## Acceptance criteria

| Acceptance criterion | Status | Evidence |
| --- | --- | --- |
| Each action key sends the exact action. The `x` and `g` keys require confirmation. | MET | Fake sender tests cover all action keys. Shell tests cover both confirmation gates. |
| A selection-dependent key does nothing without a selection. It shows no toast. | MET | `a_key_without_a_selection_changes_nothing` covers every selection-dependent key. The global `P` key is exempt by design. |
| A stage limit never falls below 1. | MET | The limit tests check the lower bound. They also check `usize::MAX`. |

## Other requirement coverage

| Requirement | Status | Evidence |
| --- | --- | --- |
| Drive the session with `show`, `on_redraw`, `poll`, `handle_key`, and `draw`. | MET | Shell tests cover task changes, file polls, chat input, and rendered output. |
| Drive the inbox with `observe`, `draw`, and `handle_key`. | MET | The shell calls all three interfaces. The inbox integration tests pass. |
| Show the inbox badge in every view. | MET | `the_inbox_badge_shows_in_every_view` checks all three views. |
| Make `!` global and select the oldest decision. | MET | Tests cover the pipeline and session views. The handler runs before all local handlers. |
| Open a task session for `InboxOutcome::OpenSession`. | MET | `enter_on_an_inbox_row_opens_the_task_session` checks the selected task. |
| Use the shared theme for the six transcript styles. | MET | All six transcript style functions use `THEME` values. |
| List PageUp, PageDown, and End in help. | MET | The help render test checks all three keys. |
| Keep a typed `q` in focused text input. | MET | Session and inbox tests check the input text and the active loop. |
| Use `+` and `-` for limits and lanes. | MET | Fake sender tests cover stage and repository rows. |
| Use `p` for the selected scope and `P` for the global scope. | MET | Tests cover stage, repository, ticket, resume, and global cases. |
| Use `r` to refine and `n` to create a ticket. | MET | Tests check both actions and both session transitions. |
| Use `x` to abort and `R` to retry. | MET | Tests check confirmation, cancellation, failure state, and the final actions. |
| Use space, `g`, and `s` in the release group. | MET | Tests check stack changes, release confirmation, pull request lists, and the policy cycle. |
| Show a specific toast for every sent pipeline action. | MET | Direct and confirmed action tests check the full toast text. |

## Findings and fixes

### Important - The session transcript did not refresh

The main loop waited forever after each message.
It never called `SessionView::poll`.

I added a session-only channel deadline.
The loop now calls `poll` at most once per 200 ms.
It draws only when the log adds visible data.
It does not use a tick thread.

The regression test appends a log line without a shell message.
It also proves that an unchanged log causes no draw.

### Important - The session consumed the global inbox key

The session handler received `!` before the global handler.
The key entered the chat buffer.

I moved the inbox key before all local handlers.
The key also closes the help or confirmation overlay.
The session regression test checks the oldest selected decision.

### Important - Inbox rendering hid a clock error

The inbox path changed a clock error to zero.
It did not report the error.

I changed shell rendering to return `anyhow::Result`.
The clock error now reaches the top-level TUI call.
An injected-clock test checks the full error context.

### Important - Pipeline countdown rendering hid a clock error

The release countdown used another zero fallback.
This older line still violated the global error rule.

I removed the local clock fallback.
The shell now passes a checked Unix time to the pipeline renderer.
A second injected-clock test checks this path.

### Important - New task actions left the old session active

The `r` and `n` keys changed the target id.
They left the prior `SessionView` active until the next state push.

A quick chat message could reach the wrong task.
I clear the local session while the shell waits for the new task.

Two regression tests try to chat during this wait.
They prove that only the requested action crosses the fake sender.

### Important - Modified confirmation keys sent destructive actions

A Ctrl-Y key confirmed an abort or a release.
The confirmation handler checked only the character.

I now require a plain press of `y`.
A regression test proves that Ctrl-Y sends nothing.

### Important - Repeated action keys used stale pushed state

Two quick action keys read the same daemon snapshot.
They could send repeated limits, pauses, retries, stacks, and policies.

The prior fix commit updates the local view after each sent action.
The next daemon push still replaces that local view.
Regression tests cover repeated keys and release confirmation data.

### Important - Large limits could wrap during an adjustment

The first implementation converted a `usize` limit to `isize`.
A large limit could become negative and then become 1.

The prior fix commit uses saturating `usize` operations.
The limit test checks `usize::MAX`.

### Important - A terminal error could hide a render error

The terminal draw path returned before it checked the render result.
A rare frame could lose one of two errors.

I added one result combiner.
Its test proves that a dual failure reports both messages.

### Minor - Resume toasts used the wrong operation

The first implementation used the word `pause` for both states.
The prior fix selects `pause` or `resume` from the sent value.
Tests cover stage, repository, and global resume actions.

### Minor - Modified action keys triggered plain actions

The first pipeline handler accepted control-modified action keys.
The prior fix accepts only an unmodified key or a Shift key.
A regression test checks Ctrl-P.

### Minor - The session exposed a test-only input getter

Only one shell test called `SessionView::input_text`.
The rendered input bar already exposes the required behavior.

I changed the test to inspect the rendered input.
I removed the unused public getter.

## Constraint review

The chunk adds no dependency.
It adds no asynchronous code.
It adds no tick thread.
It adds no journal or domain-state lock.

The session deadline uses the main channel and `recv_timeout`.
This pattern follows the global event-loop rule.
The loop draws no unchanged session frame.

All tests run offline.
The new tests use fake senders, local files, and an injected clock.
Each new test has an assertion.
Production code adds no `unwrap` or `expect`.

The source changes do not touch `ui/console/`, `zellij/`, or `bin/`.

## Scope decisions

The fixes in this pass change `mod.rs`, `pipeline.rs`, and `session.rs`.
These files are inside the corrected integration scope.

The implementer also changed `transcript.rs`.
The six required style functions live in that file.
I kept only their direct bindings to `theme.rs`.

The direct theme requirement conflicts with the narrower file list.
I chose the functional requirement because removal would fail an explicit integration requirement.

## Deliberately left alone

The implementer report does not exist at the required path.
I did not create a false report for the implementer.
I verified the brief, the code, the commits, and the tests directly.

I did not change the old v0.4 tree.
I did not push or open a pull request.

## Exact final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-19)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.06s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.06s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 435 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test config::tests::example_file_parses_with_every_override ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test daemon::tests::a_failed_chat_does_not_extend_the_idle_deadline ... ok
test daemon::tests::a_failed_session_answer_keeps_the_decision_open ... ok
test daemon::tests::a_closed_implement_gate_cancels_its_running_task ... ok
test daemon::tests::a_gate_admits_work_and_a_second_drive_dispatches_nothing ... ok
test daemon::tests::fill_template_rejects_an_unknown_placeholder_and_fills_known_ones ... ok
test daemon::tests::a_needs_human_answer_comments_then_unlabels ... ok
test daemon::tests::a_failed_state_write_retries_after_the_path_recovers ... ok
test daemon::tests::a_question_answer_reaches_the_runner_verbatim ... ok
test daemon::tests::a_live_chat_marks_the_parked_task_running ... ok
test daemon::tests::a_claude_turn_completes_a_one_shot_task_before_process_exit ... ok
test daemon::tests::a_release_answer_must_match_the_gate_snapshot ... ok
test daemon::tests::a_new_head_replaces_only_the_superseded_review ... ok
test daemon::tests::a_session_marker_error_stops_and_retries_the_task ... ok
test daemon::tests::a_reaped_chat_waits_for_a_live_process_slot ... ok
test daemon::tests::a_restart_restores_trains_decisions_and_worktrees ... ok
test daemon::tests::invalid_runtime_limits_and_policies_are_refused ... ok
test daemon::tests::a_failing_task_requeues_then_the_third_failure_opens_stuck ... ok
test daemon::tests::go_fires_the_train_and_takes_the_gate_row ... ok
test daemon::tests::scan_placeholders_finds_placeholder_shaped_tokens ... ok
test daemon::tests::an_opencode_step_does_not_complete_its_task ... ok
test daemon::tests::an_unrelated_poll_change_does_not_cancel_a_draft_review ... ok
test daemon::tests::permission_asks_route_to_the_live_session ... ok
test daemon::tests::next_deadline_picks_the_earliest_and_none_when_idle ... ok
test daemon::tests::a_review_marker_error_requeues_the_review ... ok
test daemon::tests::making_a_pull_request_ready_cancels_its_draft_review ... ok
test decisions::tests::different_conditions_open_separate_rows ... ok
test daemon::tests::stop_shuts_the_loop_down_and_drive_becomes_a_no_op ... ok
test daemon::tests::zero_removes_an_absent_lane_without_persisting_an_override ... ok
test daemon::tests::a_manual_release_retry_keeps_the_exact_batch ... ok
test daemon::tests::question_text_becomes_a_chat_line ... ok
test decisions::tests::decisions_and_responses_round_trip_through_json ... ok
test decisions::tests::needs_human_refuses_retry_because_the_label_can_outlive_its_task ... ok
test daemon::tests::retry_refuses_a_completed_task ... ok
test daemon::tests::one_dispatch_error_does_not_block_another_stage ... ok
test decisions::tests::drop_for_task_removes_only_that_tasks_rows ... ok
test daemon::tests::a_stuck_task_retries_from_attempt_one_and_an_abort_cancels ... ok
test daemon::tests::ticket_create_uses_the_interactive_runner ... ok
test daemon::tests::the_gate_row_tracks_the_stacked_set ... ok
test decisions::tests::permission_and_question_ids_derive_from_task_and_request ... ok
test decisions::tests::pushing_one_condition_again_refreshes_its_data ... ok
test decisions::tests::open_lists_rows_in_push_order ... ok
test daemon::tests::an_interval_fire_survives_a_restart ... ok
test daemon::tests::ticket_create_starts_one_ticket_session ... ok
test decisions::tests::pushing_one_condition_twice_keeps_one_row ... ok
test decisions::tests::needs_human_and_gate_ids_derive_from_repo_and_item ... ok
test daemon::tests::turn_end_parks_a_session_and_the_reaper_frees_the_slot ... ok
test daemon::tests::removing_an_issue_cancels_its_running_tasks ... ok
test decisions::tests::stuck_ids_derive_from_task_and_attempt ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test decisions::tests::take_removes_the_row_and_a_repeat_push_reopens_it ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_returns_steps_in_order ... ok
test gates::tests::a_list_requires_a_separator_between_numbers ... ok
test gates::tests::a_list_stops_at_the_first_foreign_token ... ok
test daemon::tests::overrides_persist_across_a_restart ... ok
test decisions::tests::the_table_accepts_every_legal_pair_and_refuses_every_other ... ok
test gates::tests::a_new_push_retriggers_review_but_an_unchanged_draft_does_not ... ok
test gates::tests::a_phrase_takes_a_list_of_numbers ... ok
test gates::tests::a_steady_label_fires_once ... ok
test gates::tests::a_tab_does_not_separate_numbers ... ok
test gates::tests::a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate ... ok
test daemon::tests::unknown_repository_actions_change_no_domain_state ... ok
test gates::tests::a_vanished_item_is_forgotten_and_can_fire_again_on_return ... ok
test gates::tests::blocked_by_ignores_bare_numbers_and_loose_text ... ok
test gates::tests::blocked_by_collects_numbers_across_a_body ... ok
test gates::tests::blocked_by_parses_all_three_phrasings_in_any_case ... ok
test gates::tests::forget_drops_memory_so_the_next_poll_fires_again ... ok
test gates::tests::an_implement_gate_stays_shut_while_a_dependency_is_open ... ok
test gates::tests::implement_takes_refined_issues_without_to_refine ... ok
test daemon::tests::prompt_rendering_fills_every_placeholder_and_reports_an_unknown_one ... ok
test gates::tests::refine_takes_open_issues_labelled_to_refine ... ok
test gates::tests::implement_waits_for_open_dependencies ... ok
test gates::tests::release_ready_pull_requests_are_reported_once ... ok
test gates::tests::removing_and_readding_a_label_fires_again ... ok
test gates::tests::repositories_are_tracked_independently ... ok
test gates::tests::review_takes_open_drafts_and_release_takes_ready_ones ... ok
test gates::tests::two_phrases_each_take_their_own_list ... ok
test gh::tests::a_304_without_a_cached_page_is_an_error ... ok
test gh::tests::a_403_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_429_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page ... ok
test gh::tests::a_304_body_is_never_parsed ... ok
test gh::tests::a_pull_without_a_draft_field_is_rejected ... ok
test gh::tests::a_304_page_known_to_be_short_ends_the_walk ... ok
test gh::tests::a_command_failure_without_a_response_head_keeps_stderr ... ok
test gh::tests::add_label_posts_to_the_labels_endpoint ... ok
test gh::tests::an_http_500_is_an_error_naming_the_status ... ok
test gh::tests::an_etag_from_one_repository_is_not_sent_to_another_repository ... ok
test gh::tests::an_issue_with_a_pull_request_key_never_appears_in_issues ... ok
test gh::tests::a_pull_without_a_head_sha_is_rejected ... ok
test gh::tests::an_issue_without_labels_is_rejected ... ok
test gh::tests::create_issue_returns_the_created_issue ... ok
test gh::tests::an_unknown_item_state_is_rejected ... ok
test gh::tests::fetch_pulls_maps_draft_and_head_sha ... ok
test gh::tests::remove_label_sends_a_delete ... ok
test gh::tests::fetch_issues_runs_the_exact_gh_call_and_maps_the_items ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test daemon::tests::review_success_writes_reviewed_sha_and_failure_does_not ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test poll::tests::daemon_msg_has_the_shutdown_variant ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test poll::tests::the_backoff_doubles_and_caps ... ok
test poll::tests::a_closed_daemon_channel_returns_the_send_error ... ok
test poll::tests::a_wake_forces_an_early_pass_and_merges_the_unchanged_pages ... ok
test poll::tests::the_wake_map_holds_one_sender_per_repository ... ok
test proc::tests::every_raw_stdout_line_reaches_the_log_byte_for_byte ... ok
test proc::tests::stderr_lines_reach_the_log_with_a_prefix_and_no_events ... ok
test runner::claude::tests::a_can_use_tool_request_defaults_the_human_flag_to_false ... ok
test runner::claude::tests::a_can_use_tool_request_parses_into_a_tool_ask ... ok
test proc::tests::write_line_reaches_the_child_and_close_stdin_ends_it ... ok
test proc::tests::exit_code_and_success_flag_are_reported_exactly_once ... ok
test proc::tests::write_to_a_dead_child_returns_an_error ... ok
test gh::tests::a_304_page_without_a_cached_next_link_ends_pagination ... ok
test gh::tests::a_304_page_with_a_cached_next_page_does_not_end_the_walk ... ok
test gh::tests::pagination_merges_two_pages_into_one_map ... ok
test runner::claude::tests::a_non_control_line_cannot_create_a_tool_ask ... ok
test runner::claude::tests::a_child_that_exits_during_the_handshake_fails_the_job ... ok
test runner::claude::tests::a_fake_child_drives_the_full_happy_path ... ok
test runner::claude::tests::a_matching_response_without_success_fails_the_handshake ... ok
test runner::claude::tests::a_human_question_is_never_auto_answered_even_under_yolo ... ok
test runner::claude::tests::an_ordinary_ask_reaches_the_caller_and_a_deny_goes_out ... ok
test runner::claude::tests::an_unknown_request_id_is_an_error_and_a_plain_allow_echoes_the_input ... ok
test runner::claude::tests::fixture_replay_produces_the_expected_run_events ... ok
test runner::claude::tests::dropping_a_session_stops_its_child_with_an_interrupt ... ok
test runner::claude::tests::stop_clears_every_pending_ask ... ok
test runner::claude::tests::the_argument_vector_matches_the_verified_invocation ... ok
test runner::claude::tests::stop_writes_the_interrupt_line_before_any_signal ... ok
test runner::claude::tests::the_initialize_request_matches_the_verified_shape ... ok
test runner::claude::tests::the_interrupt_line_carries_a_fresh_uuid_request_id ... ok
test runner::claude::tests::the_resume_argument_vector_carries_resume_and_no_session_id ... ok
test runner::claude::tests::the_resume_job_resumes_without_minting_a_session_id ... ok
test runner::claude::tests::tool_summaries_use_command_path_and_truncated_json ... ok
test proc::tests::the_protocol_interrupt_can_stop_the_child_before_any_signal ... ok
test proc::tests::terminate_runs_kill_term_through_the_injected_exec ... ok
test proc::tests::write_after_exit_fails_when_a_descendant_keeps_stdin_open ... ok
test runner::opencode::tests::a_malformed_line_is_skipped_and_the_run_continues ... ok
test runner::claude::tests::the_session_id_callback_fires_once_with_the_minted_id ... ok
test runner::claude::tests::an_error_control_response_fails_the_handshake ... ok
test runner::opencode::tests::a_step_finish_with_an_error_reason_fails_the_turn ... ok
test runner::opencode::tests::a_tool_part_without_a_title_falls_back_to_the_truncated_part ... ok
test runner::opencode::tests::a_tool_part_yields_a_tool_event_whatever_the_line_type ... ok
test runner::opencode::tests::an_unknown_line_type_is_ignored_without_stopping_the_run ... ok
test runner::opencode::tests::a_tool_use_line_without_a_tool_part_is_ignored ... ok
test runner::opencode::tests::an_invalid_top_level_session_id_does_not_hide_the_part_session_id ... ok
test runner::claude::tests::user_lines_reach_the_child_in_the_exact_wire_shape ... ok
test runner::opencode::tests::only_the_first_step_start_emits_started ... ok
test runner::opencode::tests::the_argument_vector_matches_the_verified_invocation ... ok
test runner::claude::tests::yolo_auto_allows_an_ordinary_ask_in_the_verified_shape ... ok
test runner::opencode::tests::the_argument_vector_carries_the_variant_when_set ... ok
test runner::opencode::tests::the_session_id_comes_from_the_first_line_that_carries_one ... ok
test runner::tests::the_default_session_methods_refuse_steering ... ok
test runner::opencode::tests::fixture_replay_produces_the_expected_run_events ... ok
test sched::tests::a_paused_stage_leaves_other_stages_free ... ok
test sched::tests::a_repository_without_reservation_uses_all_remaining_capacity ... ok
test sched::tests::an_empty_table_yields_no_dispatch ... ok
test sched::tests::a_reserved_slot_stays_free_while_another_repository_has_queued_work ... ok
test sched::tests::awaiting_user_tasks_hold_no_scheduler_slot ... ok
test sched::tests::excessive_runtime_lane_values_still_produce_a_warning ... ok
test proc::tests::spawn_creates_the_log_parent_and_appends_to_the_log ... ok
test sched::tests::excessive_runtime_lane_values_still_block_unreserved_work ... ok
test sched::tests::can_start_names_the_reasons_in_order ... ok
test sched::tests::dispatch_preserves_insertion_order_and_never_starves_the_head_task ... ok
test sched::tests::next_dispatch_skips_tasks_that_are_not_queued ... ok
test sched::tests::reservations_covering_the_limit_produce_a_warning ... ok
test sched::tests::pausing_blocks_dispatch_and_reports_the_right_reason ... ok
test sched::tests::the_reserving_repository_uses_its_slot_at_once ... ok
test sched::tests::limits_build_from_a_config ... ok
test sock::tests::a_client_cannot_connect_to_a_missing_daemon ... ok
test sock::tests::a_state_view_round_trips_through_json ... ok
test sock::tests::a_server_drop_preserves_a_replacement_at_the_socket_path ... ok
test runner::opencode::tests::a_fake_opencode_child_drives_the_full_run ... ok
test sock::tests::a_server_drop_removes_the_socket_file_and_closes_clients ... ok
test sock::tests::bind_creates_the_socket_directory ... ok
test sock::tests::a_client_that_goes_away_does_not_harm_the_daemon ... ok
test sock::tests::bind_refuses_a_path_with_a_live_daemon ... ok
test sock::tests::bind_creates_the_socket_with_mode_0600 ... ok
test runner::opencode::tests::stop_kills_the_child ... ok
test sock::tests::broadcast_skips_the_initial_push_that_a_subscriber_already_received ... ok
test sock::tests::bind_refuses_to_replace_a_plain_file_at_the_socket_path ... ok
test sock::tests::every_action_round_trips_through_one_json_line ... ok
test sock::tests::the_wire_shapes_use_the_documented_tags ... ok
test state::tests::a_corrupt_state_file_loads_the_defaults ... ok
test state::tests::a_missing_state_file_loads_the_defaults ... ok
test sock::tests::the_view_build_rejects_a_missing_ordered_task ... ok
test sock::tests::bind_replaces_a_stale_socket_file ... ok
test sock::tests::the_view_build_describes_every_merged_module ... ok
test state::tests::a_state_file_with_an_invalid_limit_loads_the_defaults ... ok
test tasks::tests::a_requeued_task_moves_to_the_back_of_the_order ... ok
test tasks::tests::a_transition_stamps_updated_ms_and_keeps_created_ms ... ok
test tasks::tests::an_unknown_task_id_is_an_error ... ok
test tasks::tests::cancelling_a_queued_task_removes_it_from_active_tasks ... ok
test tasks::tests::cancelling_records_the_cancelled_reason ... ok
test state::tests::an_empty_state_still_round_trips ... ok
test tasks::tests::counts_by_stage_count_running_tasks_only ... ok
test tasks::tests::counts_by_stage_repo_count_per_repository_and_stage ... ok
test tasks::tests::new_builds_the_id_per_the_naming_rules ... ok
test state::tests::the_state_survives_a_round_trip ... ok
test tasks::tests::retries_count_attempts_up_to_the_limit ... ok
test tasks::tests::retry_past_max_attempts_is_refused_with_a_clear_message ... ok
test tasks::tests::task_state_round_trips_through_json ... ok
test tasks::tests::upsert_inserts_new_tasks_in_insertion_order ... ok
test tasks::tests::running_and_active_keep_the_insertion_order ... ok
test tasks::tests::every_transition_in_the_matrix_follows_the_rules ... ok
test tasks::tests::upsert_keys_on_repo_stage_kind_and_number ... ok
test tasks::tests::upsert_refuses_while_the_existing_task_is_active ... ok
test trains::tests::a_draft_pr_does_not_return_after_an_in_flight_failure ... ok
test tasks::tests::upsert_replaces_a_terminal_task_with_a_fresh_queued_task ... ok
test trains::tests::a_failed_train_refuses_a_different_retry_set ... ok
test trains::tests::a_failed_train_returns_its_prs_and_a_retry_reuses_the_same_set ... ok
test trains::tests::a_label_error_keeps_the_train_in_flight_for_a_finish_retry ... ok
test trains::tests::a_saturated_interval_fires_at_its_returned_deadline ... ok
test trains::tests::a_poll_keeps_an_in_flight_label_for_success_cleanup ... ok
test trains::tests::a_stacked_subset_fires_instead_of_the_whole_queue ... ok
test trains::tests::a_second_fire_while_in_flight_is_refused ... ok
test trains::tests::a_successful_train_drains_the_batch_and_clears_the_labels ... ok
test trains::tests::a_successful_unstacked_train_makes_no_label_call ... ok
test trains::tests::a_threshold_policy_fires_when_the_count_is_reached ... ok
test trains::tests::a_threshold_train_does_not_fire_again_while_in_flight ... ok
test trains::tests::an_interval_policy_fires_only_at_or_after_its_deadline ... ok
test trains::tests::an_interval_train_that_never_fired_is_due_now ... ok
test trains::tests::an_interval_train_with_an_empty_queue_has_no_deadline ... ok
test trains::tests::enqueue_adds_a_ready_pr_once ... ok
test trains::tests::dequeue_removes_a_pr_from_the_queue_and_the_cache ... ok
test trains::tests::finish_without_a_train_touches_nothing ... ok
test trains::tests::firing_a_duplicate_pr_is_refused ... ok
test trains::tests::firing_a_pr_outside_the_queue_is_refused ... ok
test trains::tests::firing_an_empty_set_is_refused ... ok
test trains::tests::manual_never_fires_but_fire_works ... ok
test trains::tests::rebuild_stacked_keeps_only_queued_prs_in_queue_order ... ok
test trains::tests::stacking_a_pr_that_is_not_queued_is_refused ... ok
test trains::tests::stack_adds_the_github_label_and_updates_the_cache ... ok
test trains::tests::the_next_deadline_is_the_interval_fire_moment ... ok
test trains::tests::stacking_twice_makes_one_label_call ... ok
test trains::tests::the_task_id_names_the_lowest_pr_of_the_batch ... ok
test trains::tests::unstack_removes_the_github_label_and_the_cache_entry ... ok
test trains::tests::unstacking_an_absent_pr_is_a_no_op ... ok
test tui::inbox::tests::a_digit_beyond_the_options_changes_nothing ... ok
test tui::inbox::tests::a_push_that_changes_the_input_row_kind_cancels_that_input ... ok
test tui::inbox::tests::a_push_that_closes_the_input_row_cancels_that_input ... ok
test tui::inbox::tests::a_time_before_the_unix_epoch_returns_an_error ... ok
test tui::inbox::tests::a_row_summary_replaces_line_breaks_with_spaces ... ok
test tui::inbox::tests::a_gate_repush_drops_checks_for_absent_pull_requests ... ok
test tui::inbox::tests::each_kind_has_an_exact_immediate_answer_key_map ... ok
test tui::inbox::tests::exclamation_selects_the_oldest_row ... ok
test tui::inbox::tests::a_push_that_changes_question_options_clears_stale_picks ... ok
test tui::inbox::tests::an_empty_state_renders_a_placeholder_and_no_rows ... ok
test tui::inbox::tests::gate_space_and_g_fire_the_whole_batch ... ok
test tui::inbox::tests::j_k_and_the_arrow_keys_move_the_selection ... ok
test tui::inbox::tests::needs_human_t_comments_c_cancels_and_no_key_retries ... ok
test tui::inbox::tests::oldest_decision_names_the_row_with_the_smallest_opened_ms ... ok
test tui::inbox::tests::permission_n_takes_a_reason_and_sends_deny ... ok
test tui::inbox::tests::permission_answers_require_a_plain_key_press ... ok
test tui::inbox::tests::permission_y_sends_allow_a_sends_nothing_and_enter_opens_the_session ... ok
test tui::inbox::tests::question_digits_pick_enter_submits_the_answers ... ok
test tui::inbox::tests::an_empty_text_input_is_blocked_with_a_hint ... ok
test tui::inbox::tests::question_digits_toggle_options_and_answers_carry_lists_when_multi_select ... ok
test tui::inbox::tests::question_i_takes_a_free_answer_and_sends_text ... ok
test tui::inbox::tests::gate_digits_narrow_the_batch_and_g_refuses_an_empty_one ... ok
test tui::inbox::tests::stuck_r_retries_c_cancels_and_enter_opens_the_session ... ok
test tui::inbox::tests::question_enter_without_a_pick_is_blocked_with_a_hint ... ok
test tui::inbox::tests::the_answer_key_prefers_the_header_and_falls_back_to_the_question_text ... ok
test tui::inbox::tests::the_badge_shows_the_pushed_open_count ... ok
test tui::inbox::tests::question_enter_without_a_multi_select_pick_is_blocked_with_a_hint ... ok
test tui::inbox::tests::rows_render_age_repo_stage_and_summary ... ok
test tui::inbox::tests::the_age_derives_from_opened_ms_so_a_late_connect_and_a_repush_never_reset_it ... ok
test tui::inbox::tests::the_selected_question_row_expands_to_numbered_options ... ok
test tui::inbox::tests::the_selection_follows_its_row_across_a_repush_and_prunes_gone_rows ... ok
test tui::inbox::tests::the_socket_client_sends_an_inbox_action ... ok
test tui::inbox::tests::the_selected_row_remains_visible_below_the_viewport ... ok
test tui::pipeline::tests::a_control_modified_action_key_is_a_no_op ... ok
test tui::inbox::tests::the_selected_gate_row_expands_to_checkboxes ... ok
test tui::pipeline::tests::a_key_without_a_selection_changes_nothing ... ok
test tui::pipeline::tests::confirmed_actions_toast_the_exact_operation ... ok
test tui::pipeline::tests::every_direct_action_toast_names_what_was_sent ... ok
test tui::pipeline::tests::format_countdown_formats_each_range ... ok
test proc::tests::a_polite_child_dies_at_sigterm ... ok
test tui::inbox::tests::typing_edits_the_buffer_and_esc_cancels ... ok
test tui::pipeline::tests::g_only_opens_the_release_confirmation ... ok
test tui::pipeline::tests::move_selection_walks_and_clamps ... ok
test tui::pipeline::tests::n_creates_a_ticket_for_the_selected_repository ... ok
test tui::inbox::tests::the_footer_shows_the_key_map_of_the_selected_kind ... ok
test tui::pipeline::tests::pause_keys_toggle_the_current_scope_and_name_the_operation ... ok
test tui::pipeline::tests::p_pauses_the_selected_scope_and_p_capital_pauses_all ... ok
test tui::pipeline::tests::empty_state_shows_every_stage_header ... ok
test tui::pipeline::tests::plus_and_minus_change_the_lane_of_a_repository_row ... ok
test tui::pipeline::tests::plus_and_minus_change_the_stage_limit ... ok
test tui::pipeline::tests::policy_label_covers_every_policy ... ok
test tui::pipeline::tests::r_capital_retries_only_a_failed_task ... ok
test tui::pipeline::tests::r_refines_the_selected_ticket_and_follows_the_new_task ... ok
test tui::pipeline::tests::release_confirmation_includes_a_just_stacked_pull_request ... ok
test tui::pipeline::tests::repeated_amount_keys_use_the_previous_request_and_saturate ... ok
test tui::pipeline::tests::repeated_r_capital_sends_one_retry_for_one_failure ... ok
test tui::pipeline::tests::repeated_s_completes_the_full_policy_cycle ... ok
test tui::pipeline::tests::repeated_space_toggles_the_same_queue_entry ... ok
test tui::pipeline::tests::rows_order_stages_then_repositories_then_tickets ... ok
test tui::pipeline::tests::s_cycles_the_release_policy ... ok
test tui::pipeline::tests::full_state_shows_stage_counts_and_tickets ... ok
test tui::pipeline::tests::space_stacks_the_first_queue_entry_of_a_train ... ok
test tui::pipeline::tests::x_only_opens_the_abort_confirmation ... ok
test tui::session::tests::a_missing_log_file_is_quiet_until_it_appears ... ok
test tui::session::tests::a_partial_line_waits_for_its_newline ... ok
test tui::session::tests::an_empty_or_blank_input_sends_nothing ... ok
test tui::session::tests::ask_questions_reads_the_recorded_payload_shape ... ok
test tui::session::tests::ctrl_x_aborts_the_shown_task ... ok
test tui::session::tests::keys_without_a_shown_task_send_nothing ... ok
test tui::session::tests::tail_following_resumes_with_end_after_scrolling_up ... ok
test tui::session::tests::showing_a_different_task_resets_the_transcript ... ok
test tui::session::tests::the_draw_fits_into_a_one_row_area ... ok
test tui::session::tests::the_draw_shows_a_pending_ask_for_this_task_only ... ok
test tui::session::tests::the_draw_shows_the_header_the_transcript_and_the_input_bar ... ok
test tui::pipeline::tests::full_state_shows_the_release_group ... ok
test tui::session::tests::the_ring_buffer_never_exceeds_its_bound ... ok
test tui::session::tests::the_session_view_tails_the_log_file ... ok
test tui::session::tests::the_poll_reads_at_most_once_per_interval ... ok
test tui::pipeline::tests::the_selected_row_carries_the_marker ... ok
test tui::tests::a_frame_reports_both_terminal_and_render_errors ... ok
test tui::session::tests::typing_and_enter_sends_one_chat_action ... ok
test tui::tests::a_modified_y_does_not_confirm_a_destructive_action ... ok
test tui::pipeline::tests::paused_stages_show_the_pause_mark ... ok
test tui::tests::a_state_push_connects_and_clamps_the_selection ... ok
test tui::tests::an_alternate_screen_failure_restores_the_terminal ... ok
test tui::tests::a_pipeline_clock_error_propagates_from_render ... ok
test tui::session::tests::the_draw_marks_an_older_hidden_history ... ok
test tui::tests::backoff_grows_from_one_second_to_ten ... ok
test tui::tests::bang_enters_the_inbox_and_selects_the_oldest_row ... ok
test tui::tests::bang_enters_the_inbox_from_the_session_view ... ok
test tui::tests::banner_text_covers_every_state ... ok
test tui::tests::enter_on_an_inbox_row_opens_the_task_session ... ok
test tui::tests::enter_sends_the_session_chat_action ... ok
test tui::tests::esc_cancels_the_abort_so_y_sends_nothing ... ok
test tui::tests::a_disconnected_app_keeps_the_last_state_under_the_banner ... ok
test tui::tests::keys_switch_views_and_toggle_help ... ok
test tui::tests::n_cannot_send_chat_to_the_previous_session_while_it_waits ... ok
test tui::tests::n_follows_the_ticket_create_task_on_the_next_push ... ok
test tui::tests::an_inbox_clock_error_propagates_from_render ... ok
test tui::tests::a_fresh_toast_shows_and_an_expired_one_does_not ... ok
test tui::tests::r_cannot_send_chat_to_the_previous_session_while_it_waits ... ok
test tui::tests::r_follows_the_new_refine_task_on_the_next_push ... ok
test tui::tests::terminal_restore_attempts_both_steps_and_reports_both_errors ... ok
test tui::tests::an_unconnected_app_shows_the_connecting_banner ... ok
test tui::tests::q_types_into_the_session_input_and_never_quits ... ok
test tui::tests::q_types_into_the_inbox_reason_and_never_quits ... ok
test tui::tests::the_header_shows_the_pause_flag_and_socket_state ... ok
test tui::tests::the_loop_stops_on_q_without_a_last_draw ... ok
test tui::tests::the_quit_chord_stops_the_loop_from_anywhere ... ok
test tui::tests::the_help_overlay_lists_the_keys_of_this_chunk ... ok
test tui::tests::y_confirms_the_aborting_of_the_selected_task ... ok
test tui::tests::y_confirms_the_release_of_the_stacked_batch ... ok
test tui::transcript::tests::a_claude_assistant_text_line_renders_as_plain_text ... ok
test tui::transcript::tests::a_claude_failed_tool_result_is_marked ... ok
test tui::transcript::tests::a_claude_result_line_shows_the_outcome_and_the_cost ... ok
test tui::transcript::tests::a_claude_tool_use_line_renders_dim_and_prefixed ... ok
test tui::transcript::tests::a_claude_user_message_renders_as_the_human_voice ... ok
test tui::transcript::tests::a_failed_claude_result_line_is_marked ... ok
test tui::transcript::tests::a_known_type_with_an_unusable_payload_falls_back_to_raw ... ok
test tui::transcript::tests::a_malformed_line_renders_as_a_dim_raw_line_and_is_never_dropped ... ok
test tui::transcript::tests::a_subagent_line_is_skipped ... ok
test tui::transcript::tests::a_very_narrow_width_wraps_every_line_inside_the_width ... ok
test tui::transcript::tests::an_empty_line_parses_to_nothing ... ok
test tui::transcript::tests::an_opencode_step_finish_renders_per_step ... ok
test tui::transcript::tests::an_opencode_text_line_renders_the_text ... ok
test tui::transcript::tests::an_opencode_tool_line_matches_on_the_part_and_marks_errors ... ok
test tui::transcript::tests::an_opencode_tool_part_does_not_depend_on_the_outer_line_type ... ok
test tui::transcript::tests::prefixed_and_wide_text_stays_inside_the_pane_width ... ok
test tui::transcript::tests::system_and_control_lines_render_as_dim_notes ... ok
test tui::transcript::tests::wrap_respects_the_width_and_splits_long_words ... ok
test worktree::tests::branch_lookup_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_rejects_an_empty_reference ... ok
test worktree::tests::ensure_issue_adds_an_existing_branch_without_b ... ok
test worktree::tests::ensure_issue_builds_the_documented_commands ... ok
test tui::tests::the_inbox_badge_shows_in_every_view ... ok
test sock::tests::a_round_trip_connects_answers_and_pushes_again ... ok
test daemon::tests::an_idle_loop_waits_for_a_message_and_stop_returns ... ok
test worktree::tests::ensure_issue_falls_back_to_head_without_origin ... ok
test poll::tests::failures_back_off_and_the_thread_stays_alive ... ok
test worktree::tests::ensure_train_resets_an_existing_worktree_through_the_documented_commands ... ok
test worktree::tests::exists_issue_reports_registered_worktrees_only ... ok
test worktree::tests::marker_write_failure_preserves_the_marker_and_removes_the_temporary_path ... ok
test worktree::tests::marker_write_reports_a_temporary_cleanup_failure ... ok
test worktree::tests::markers_round_trip_and_leave_no_temporary_file ... ok
test worktree::tests::remove_issue_runs_remove_then_branch_delete ... ok
test runner::claude::tests::idle_for_grows_between_events_and_resets_on_one ... ok
test worktree::tests::ensure_issue_creates_a_worktree_at_the_documented_path ... ok
test tui::tests::the_loop_draws_once_per_message_and_not_during_a_quiet_interval ... ok
test worktree::tests::ensure_issue_twice_reuses_in_place ... ok
test worktree::tests::ensure_issue_reuses_a_branch_whose_worktree_was_removed ... ok
test proc::tests::stop_gracefully_does_not_block_the_caller ... ok
test proc::tests::a_protocol_interrupt_error_is_reported_when_the_child_later_exits ... ok
test proc::tests::a_sigterm_error_is_reported_when_the_child_later_exits ... ok
test poll::tests::a_wake_reaches_only_its_own_repository ... ok
test proc::tests::the_initial_grace_allows_a_natural_exit_without_a_protocol_interrupt ... ok
test proc::tests::a_child_that_ignores_sigterm_reaches_sigkill ... ok
test worktree::tests::ensure_train_creates_and_resets_to_the_default_branch ... ok
test worktree::tests::the_aif_directory_is_invisible_to_git ... ok
test worktree::tests::remove_issue_with_proof_removes_the_worktree_and_the_branch ... ok
test runner::claude::tests::an_unrelated_control_response_does_not_finish_the_handshake ... ok
test runner::claude::tests::a_missing_control_response_fails_the_job_naming_the_handshake ... ok
test runner::claude::tests::a_noisy_child_cannot_postpone_the_handshake_timeout ... ok
test sock::tests::rapid_publishes_coalesce_into_few_pushes_and_the_last_one_wins ... ok
test tui::tests::the_session_poll_draws_new_log_text_without_an_input_message ... ok
test tui::tests::every_reconnect_failure_reaches_the_main_loop ... ok
test sock::tests::a_subscriber_that_never_reads_is_dropped_and_the_daemon_keeps_running ... ok

test result: ok. 435 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.80s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 34 tests
test doctor::tests::a_date_word_and_a_fourth_component_stay_rejected ... ok
test doctor::tests::a_daemon_socket_access_error_is_a_failure ... ok
test doctor::tests::a_missing_config_still_reports_the_tools_and_the_daemon ... ok
test doctor::tests::a_tool_version_failure_keeps_the_command_diagnostic ... ok
test doctor::tests::daemon_program_ends_with_aifd ... ok
test doctor::tests::has_failures_decides_the_doctor_exit_code ... ok
test doctor::tests::claude_at_the_floor_passes ... ok
test doctor::tests::a_claude_below_the_floor_fails_and_names_the_floor ... ok
test doctor::tests::a_scheduler_lane_warning_is_reported ... ok
test doctor::tests::a_repository_that_is_not_a_git_checkout_fails ... ok
test doctor::tests::clean_asks_and_aborts_on_a_refusal ... ok
test doctor::tests::a_full_report_passes_with_injected_answers ... ok
test doctor::tests::the_floor_comparison_is_component_wise ... ok
test doctor::tests::the_real_gh_describe_output_passes_tool_check ... ok
test doctor::tests::version_parse_reads_the_real_version_lines_of_this_machine ... ok
test doctor::tests::version_parse_takes_the_first_version_word_and_skips_dates ... ok
test doctor::tests::start_detached_reports_a_systemd_run_failure_without_a_fallback ... ok
test doctor::tests::start_detached_reports_a_systemd_run_execution_error_without_a_fallback ... ok
test doctor::tests::stop_round_trip_waits_for_the_socket_to_disappear ... ok
test doctor::tests::wait_socket_gone_is_true_without_any_socket ... ok
test doctor::tests::clean_propagates_a_confirmation_error_without_a_removal ... ok
test doctor::tests::a_worktree_directory_read_error_is_reported_as_a_failure ... ok
test doctor::tests::clean_never_removes_an_open_pull_request_worktree ... ok
test doctor::tests::clean_keeps_everything_when_one_item_state_is_unknown ... ok
test doctor::tests::clean_removes_a_merged_pull_request_worktree ... ok
test doctor::tests::clean_proceeds_on_confirmation ... ok
test doctor::tests::clean_keeps_a_closed_pull_request_without_a_merge ... ok
test doctor::tests::clean_removes_only_the_worktree_of_the_closed_issue ... ok
test doctor::tests::start_detached_falls_back_when_systemd_run_is_missing ... ok
test doctor::tests::spawn_detached_runs_a_real_child ... ok
test doctor::tests::start_detached_runs_systemd_run_with_the_exact_argv_and_skips_the_fallback ... ok
test doctor::tests::wait_socket_gone_is_false_while_a_stale_socket_file_remains ... ok
test doctor::tests::wait_for_socket_waits_for_a_late_listener ... ok
test doctor::tests::start_detached_times_out_when_no_daemon_opens_the_socket ... ok

test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aifd_run_accepts_a_config_path ... ok
test aif_doctor_help_lists_the_clean_options ... ok
test aif_help_lists_all_subcommands ... ok
test aifd_run_help_lists_the_config_option ... ok
test aif_stop_without_a_daemon_fails_with_a_clear_message ... ok

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

