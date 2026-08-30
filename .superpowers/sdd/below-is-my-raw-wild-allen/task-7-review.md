# Task 7 review — Worktree manager

## Acceptance criteria

| Acceptance criterion | Status | Evidence |
|---|---|---|
| Real temporary Git repository covers create, reuse, marker round trip, and removal without network access. | MET | Real Git tests create an initial commit. They cover all four actions. The bare origin uses a local path. |
| A second create call succeeds and returns the same path. | MET | `ensure_issue_twice_reuses_in_place` checks the path, commit, and session marker. |
| A simulated marker failure exposes no partial marker and leaves no `.tmp` path. | MET | The new failure test preserves the old marker. It also checks that the helper removes `session.tmp`. |
| `remove_issue` requires `Cleanable`, and the merged path succeeds. | MET | The method signature requires the proof value. The real Git removal test uses `MergedOrClosed`. |
| Git excludes `.aif` inside the issue worktree. | MET | The real Git status test shows only an unrelated untracked file. |

## Findings and fixes

| Severity | Finding | Fix |
|---|---|---|
| important | The marker tests covered only successful writes. This did not meet the simulated failure criterion. | I added a failure test. It checks the old marker and the temporary path. |
| important | Marker cleanup ignored removal errors. A temporary directory could remain after a failed write. | I added strict cleanup. It removes a file or an empty directory. It includes a cleanup failure in the returned error. |
| important | The branch lookup treated every nonzero Git status as a missing branch. This hid repository and executor failures. | I accept status 1 as absent. I propagate all other nonzero statuses. A regression test covers status 128. |
| important | Default branch resolution treated every Git failure and an empty successful response as a valid `HEAD` fallback. | I use `symbolic-ref --quiet`. Status 1 selects `HEAD`. Other failures and empty responses now return errors. |
| important | `exists_issue` silently changed a registration error to `false`. This violated the global error rule. | The method now writes the full error to standard error. It still returns the required `bool`. |

I found no critical or minor findings.

## Final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-7)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.84s
== test ==
   Compiling aif v0.5.0 (/home/navaro/Workplace/ai-factory-wt/chunk-7)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.99s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 56 tests
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test exec::tests::script_returns_steps_in_order ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test model::tests::an_open_flip_is_a_change ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test config::tests::example_file_parses_with_every_override ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test worktree::tests::branch_lookup_propagates_an_unexpected_git_failure ... ok
test worktree::tests::default_base_rejects_an_empty_reference ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok
test worktree::tests::exists_issue_reports_registered_worktrees_only ... ok
test worktree::tests::default_base_propagates_an_unexpected_git_failure ... ok
test worktree::tests::marker_write_reports_a_temporary_cleanup_failure ... ok
test worktree::tests::remove_issue_runs_remove_then_branch_delete ... ok
test worktree::tests::marker_write_failure_preserves_the_marker_and_removes_the_temporary_path ... ok
test worktree::tests::ensure_issue_builds_the_documented_commands ... ok
test worktree::tests::ensure_train_resets_an_existing_worktree_through_the_documented_commands ... ok
test worktree::tests::markers_round_trip_and_leave_no_temporary_file ... ok
test worktree::tests::ensure_issue_adds_an_existing_branch_without_b ... ok
test worktree::tests::ensure_issue_falls_back_to_head_without_origin ... ok
test worktree::tests::the_aif_directory_is_invisible_to_git ... ok
test worktree::tests::remove_issue_with_proof_removes_the_worktree_and_the_branch ... ok
test worktree::tests::ensure_issue_creates_a_worktree_at_the_documented_path ... ok
test worktree::tests::ensure_issue_twice_reuses_in_place ... ok
test worktree::tests::ensure_train_creates_and_resets_to_the_default_branch ... ok
test worktree::tests::ensure_issue_reuses_a_branch_whose_worktree_was_removed ... ok

test result: ok. 56 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.18s

     Running unittests src/bin/aif.rs (target/debug/deps/aif-f0ca71ec60e9b643)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/bin/aifd.rs (target/debug/deps/aifd-35a6c985229294c1)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (target/debug/deps/cli-edee68cff850d4a6)

running 5 tests
test aif_without_a_subcommand_starts_the_tui_placeholder ... ok
test aifd_run_accepts_a_config_path ... ok
test aifd_run_help_lists_the_config_option ... ok
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

## Deliberate nonchanges

- I kept the shared `.git/info/exclude` update. Git 2.34.1 uses this file, and the real Git test confirms the required result.
- I kept `git worktree remove --force` and `git branch -D`. The `Cleanable` proof protects this destructive operation.
- I kept `exists_issue` as a `bool`. The brief requires this type. The method now reports an error before it returns `false`.
- I did not change `src/model.rs` or `src/exec.rs`. Both files are frozen for this review.
- I did not change the old v0.4 directories or any later chunk file.

## Final verdict

PASS
