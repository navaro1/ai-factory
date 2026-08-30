# Task 3 review

## Acceptance criteria

| Acceptance criterion | Status | Evidence |
| --- | --- | --- |
| Tests use a scripted `Exec` and do not use the network. | Met | All 22 `gh` tests use `ScriptExec` with fixed response text. |
| A `304` response keeps the cached snapshot and does not parse its body. | Met | `a_304_body_is_never_parsed` compares the complete maps and supplies invalid body text. |
| A `200` ETag is sent on the next request for that page. | Met | `a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page` checks the exact second command. |
| Two pages produce one merged map. | Met | `pagination_merges_two_pages_into_one_map` checks all 103 entries. |
| An issue with a `pull_request` key does not appear in `issues`. | Met | `an_issue_with_a_pull_request_key_never_appears_in_issues` checks the filtered map. |
| A `403` response with `Retry-After: 60` names 60 seconds. | Met | `a_403_with_retry_after_names_the_wait_in_seconds` checks the message and the `RateLimited` value. |

## Findings and corrections

| Severity | Finding | Correction |
| --- | --- | --- |
| critical | The page cache stored only an ETag and two flags. A `304` page contributed no items. A mixed `304` and `200` walk returned a partial snapshot. | I cache the mapped items for each page. Each fetch now returns one complete map. The tests also prove that changed pages remove stale entries. |
| important | The cache key omitted the repository name. One repository could receive another repository's ETag. A matching `304` could reuse the wrong entries. | I added the repository name to each issue and pull request cache key. A test checks repository isolation. |
| important | The three write helpers omitted `-i`. They then tried to parse response headers. The real `gh` command supplies those headers only with `-i`. | I added `-i` to `add_label`, `remove_label`, and `create_issue`. Their tests check the exact commands. |
| important | A `304` response without a cached page returned an empty successful result. That result could erase a repository snapshot. | I return an error that names the missing cached page. A test checks this error. |
| important | The JSON mapper replaced invalid required data with empty or false values. This affected labels, draft state, head SHA, body, and item state. | I now reject missing or invalid required values. I keep an explicit null body as an empty string. Four tests cover these cases. |
| minor | A command failure without an HTTP head discarded the first command error line. | I added one response check function. It keeps the command status and first error line. |
| minor | Two public ETag access functions existed only for internal tests. Their page-only interface also became ambiguous across repositories. | I removed both functions. The tests now verify conditional request commands instead. |

## Scope

I left `src/model.rs` and `src/exec.rs` unchanged. The task freezes these files.

I left `ui/console/`, `zellij/`, and `bin/` unchanged. The specification protects these directories.

I found no later chunk work in the tracked diff. I left all supplied review input files unchanged.

## Final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-3)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.61s
== test ==
   Compiling aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-3)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.77s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 60 tests
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_returns_steps_in_order ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test gh::tests::a_304_page_known_to_be_short_ends_the_walk ... ok
test gh::tests::a_200_etag_is_stored_and_sent_on_the_next_call_for_that_page ... ok
test gh::tests::a_403_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::a_304_body_is_never_parsed ... ok
test gh::tests::a_304_without_a_cached_page_is_an_error ... ok
test config::tests::example_file_parses_with_every_override ... ok
test gh::tests::a_command_failure_without_a_response_head_keeps_stderr ... ok
test gh::tests::a_429_with_retry_after_names_the_wait_in_seconds ... ok
test gh::tests::add_label_posts_to_the_labels_endpoint ... ok
test gh::tests::a_pull_without_a_draft_field_is_rejected ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test gh::tests::a_pull_without_a_head_sha_is_rejected ... ok
test gh::tests::an_http_500_is_an_error_naming_the_status ... ok
test gh::tests::an_issue_without_labels_is_rejected ... ok
test gh::tests::create_issue_returns_the_created_issue ... ok
test gh::tests::fetch_pulls_maps_draft_and_head_sha ... ok
test gh::tests::fetch_issues_runs_the_exact_gh_call_and_maps_the_items ... ok
test gh::tests::remove_label_sends_a_delete ... ok
test gh::tests::an_issue_with_a_pull_request_key_never_appears_in_issues ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test gh::tests::an_etag_from_one_repository_is_not_sent_to_another_repository ... ok
test model::tests::an_open_flip_is_a_change ... ok
test gh::tests::an_unknown_item_state_is_rejected ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test gh::tests::pagination_merges_two_pages_into_one_map ... ok
test gh::tests::a_304_page_with_a_cached_next_page_does_not_end_the_walk ... ok
test gh::tests::a_304_page_without_a_cached_next_link_ends_pagination ... ok

test result: ok. 60 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

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

## Final verdict

PASS
