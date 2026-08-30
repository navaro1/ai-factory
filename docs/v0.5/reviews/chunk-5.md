# Task 5 review - Stage gates

## Acceptance criteria

| Acceptance criterion | Status | Test evidence |
|---|---|---|
| Each predicate has a positive test and a negative test. | MET | `refine_takes_open_issues_labelled_to_refine`, `implement_takes_refined_issues_without_to_refine`, and `review_takes_open_drafts_and_release_takes_ready_ones` cover all four predicates. |
| A steady label produces exactly one `ReadyWork` across two polls. | MET | `a_steady_label_fires_once` checks one result and then no result. |
| A removed and restored label produces a second `ReadyWork`. | MET | `removing_and_readding_a_label_fires_again` checks both edges. |
| A new draft PR head SHA produces a second result. An unchanged SHA does not. | MET | `a_new_push_retriggers_review_but_an_unchanged_draft_does_not` checks both cases. |
| The blocker parser covers all phrases, lists, and text that must not match. | MET | Six parser tests cover all required phrases, list forms, stop rules, and negative text. |
| An open dependency keeps the implement gate shut. | MET | `an_implement_gate_stays_shut_while_a_dependency_is_open` checks two blocked polls and the later edge. |

## Findings and fixes

### Important - Adjacent issue tokens bypassed the separator rule

The parser read `blocked by #1#2` as dependencies 1 and 2.
The brief permits only commas, `and`, or plain spaces between numbers.
This defect could keep the implement gate shut because of issue 2.

I added `a_list_requires_a_separator_between_numbers` before the code fix.
The test failed with `[1, 2]` instead of `[1]`.
I added parser state that requires a separator after the first number.

### Important - A tab acted as an unapproved separator

The parser read `blocked by #1\t#2` as dependencies 1 and 2.
The brief permits plain spaces, but it does not permit tabs.
This defect could create an incorrect dependency on issue 2.

I added `a_tab_does_not_separate_numbers` before the code fix.
The test failed with `[1, 2]` instead of `[1]`.
I removed tab support from the number and `and` separator logic.

I found no critical findings.
I found no forbidden dependencies or concurrency mechanisms.
I found no task creation or release queue access.
I found no out-of-scope source changes.

## Final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-5)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.78s
== test ==
   Compiling aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-5)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.87s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 59 tests
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_returns_steps_in_order ... ok
test gates::tests::a_list_requires_a_separator_between_numbers ... ok
test gates::tests::a_list_stops_at_the_first_foreign_token ... ok
test config::tests::example_file_parses_with_every_override ... ok
test gates::tests::a_phrase_takes_a_list_of_numbers ... ok
test gates::tests::a_new_push_retriggers_review_but_an_unchanged_draft_does_not ... ok
test gates::tests::a_tab_does_not_separate_numbers ... ok
test gates::tests::a_steady_label_fires_once ... ok
test gates::tests::a_refined_issue_moves_from_the_refine_gate_to_the_implement_gate ... ok
test gates::tests::blocked_by_collects_numbers_across_a_body ... ok
test gates::tests::an_implement_gate_stays_shut_while_a_dependency_is_open ... ok
test gates::tests::blocked_by_ignores_bare_numbers_and_loose_text ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test gates::tests::blocked_by_parses_all_three_phrasings_in_any_case ... ok
test gates::tests::forget_drops_memory_so_the_next_poll_fires_again ... ok
test gates::tests::implement_takes_refined_issues_without_to_refine ... ok
test gates::tests::implement_waits_for_open_dependencies ... ok
test gates::tests::release_ready_pull_requests_are_reported_once ... ok
test gates::tests::repositories_are_tracked_independently ... ok
test gates::tests::refine_takes_open_issues_labelled_to_refine ... ok
test gates::tests::review_takes_open_drafts_and_release_takes_ready_ones ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test gates::tests::removing_and_readding_a_label_fires_again ... ok
test gates::tests::two_phrases_each_take_their_own_list ... ok
test gates::tests::a_vanished_item_is_forgotten_and_can_fire_again_on_return ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::apply_never_touches_another_repository ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok

test result: ok. 59 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aifd_run_help_lists_the_config_option ... ok
test aifd_run_accepts_a_config_path ... ok
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aif_help_lists_all_subcommands ... ok
test aif_subcommands_print_placeholders ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/exec_contract.rs (target/debug/deps/exec_contract-efcaecd164461c8b)

running 1 test
test script_exec_enforces_order_and_records_calls_outside_the_crate ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests aif

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

all checks passed
```

## Deliberately unchanged items

I read `src/model.rs` and `src/exec.rs` but did not change them.
The worktree note freezes both files.

I did not change `ReadyWork.head_sha` for the release stage.
The brief restricts the review key, but it does not restrict the report field.

I did not change the supplied task inputs or the old v0.4 tree.
Neither area belongs to this review.

## Final verdict

PASS
