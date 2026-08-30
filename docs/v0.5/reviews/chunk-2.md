# Task 2 Review

## Acceptance criteria

| Acceptance criterion | Initial state | Final state | Evidence |
|---|---|---|---|
| `Snapshot::apply` isolates each repository. | Not met. The test compared only part of the other repository. | Met. | The test now compares the complete other `RepoSnapshot`. |
| Label, draft, head SHA, add, and remove changes are correct. Text-only edits produce no change. | Not met. Label order used set comparison. A later addition had no test. | Met. | Tests cover direct label comparison, all change kinds, stored text edits, and text-only results. |
| `ScriptExec` returns steps in order, records calls, and rejects an unexpected call. | Not met. The executor searched all remaining matchers. | Met. | The executor checks only the next step. An external integration test proves the public contract. |
| Unit tests parse the example and check every default and override. | Not met. The test omitted several fields. The parser also invented model and runner defaults. | Met. | The example test checks every stage field and repository field. A separate test checks all specified defaults. |
| `parse_owner_repo` covers three required forms and one rejected value. | Met. | Met. | The unit test covers all required inputs and rejected inputs. |
| One test covers each validation rule and checks the bad key. | Met. | Met. | Tests cover paths, aliases, limits, lanes, release counts, and release intervals. |
| A lane sum above the stage limit is rejected. | Met. | Met. | The existing limit test passes. A new overflow test also prevents a panic. |
| The example file has the specified shape and parses. | Met. | Met. | The example test parses the repository file and checks its complete content. |

## Findings and corrections

No critical finding remained after the review.

### Important - `ScriptExec` did not enforce script order

The executor searched all remaining matchers. A command could skip an earlier script step.

I changed the executor to check only the next step. A mismatch now returns an error without step consumption.

I added `tests/exec_contract.rs`. This test proves that an external crate test can use the full test-double API.

### Important - Configuration tests ran the real `git` program

`Config::load` fixed `RealExec` inside the resolver. One unit test also started the installed `git` program directly.

I added a private executor seam. Production still uses `RealExec`, and unit tests now use `ScriptExec`.

No task 2 test now starts `git`, `gh`, `claude`, or `opencode`.

### Important - The parser invented model and runner defaults

The specification lists defaults for limits, yolo, variant, lanes, and release policy. It lists no model or runner default.

The old parser accepted absent stage tables and absent agent keys. This behavior could hide an incomplete production configuration.

I now require all four stage tables. I also require each model key and runner key.

I corrected the example comment and added error tests that name each missing key.

### Important - Label comparison changed the specified data model

The old code converted label vectors to sets. This conversion ignored order changes and duplicate changes.

The specification says to compare the `Vec<String>` label field. I now compare each label vector directly.

### Important - Lane totals could panic on integer overflow

The old iterator sum could overflow `usize`. A large valid TOML input caused a debug-build panic.

I replaced the sum with checked addition. The error now names the stage key.

### Important - Acceptance tests did not prove the complete contracts

The example test omitted several defaults and overrides. The snapshot tests omitted a later addition and complete repository equality.

I expanded the tests. They now check all example fields, all specified defaults, later additions, removals, and stored text edits.

### Minor - The chunk added unnecessary public APIs

`ItemKind` added parsing and long-form display behavior. Neither behavior belongs to this chunk.

`ScriptExec::expect_program` accepted every argument vector. This helper could hide an incorrect command.

I removed these APIs. The required `ItemKind::as_str` and matcher API remain.

### Minor - Test cleanup errors were discarded

Several tests ignored temporary directory removal errors. This behavior violated the error-handling constraint.

I now check every cleanup result. The missing-file test also removes its temporary directory.

## Final `./check.sh` output

```text
== fmt ==
== clippy ==
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.09s
== test ==
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.08s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 38 tests
test config::tests::a_bad_alias_names_the_repo_key ... ok
test config::tests::a_directory_without_git_names_the_key ... ok
test config::tests::a_lane_sum_equal_to_the_limit_is_accepted ... ok
test config::tests::a_missing_config_file_names_where_to_create_it_and_the_example ... ok
test config::tests::a_lane_sum_over_the_stage_limit_is_rejected ... ok
test config::tests::a_git_failure_names_the_repo ... ok
test config::tests::a_missing_model_names_the_stage_key ... ok
test config::tests::a_missing_runner_names_the_stage_key ... ok
test config::tests::a_missing_stage_table_names_the_stage_key ... ok
test config::tests::a_missing_path_names_the_repo_key ... ok
test config::tests::a_lane_sum_overflow_names_the_stage_key ... ok
test config::tests::a_missing_repository_path_names_the_key ... ok
test config::tests::a_zero_limit_names_the_stage_key ... ok
test config::tests::absent_optional_keys_take_the_specified_defaults ... ok
test config::tests::an_interval_below_one_minute_names_the_key ... ok
test config::tests::a_threshold_count_below_one_names_the_key ... ok
test config::tests::parse_owner_repo_covers_the_git_forms ... ok
test config::tests::an_unknown_stage_section_names_the_key ... ok
test config::tests::an_unknown_lane_stage_names_the_lane_key ... ok
test config::tests::path_helpers_follow_the_naming_rules ... ok
test config::tests::release_policy_survives_a_json_round_trip ... ok
test exec::tests::script_fails_on_an_unmatched_command ... ok
test exec::tests::script_returns_steps_in_order ... ok
test exec::tests::script_records_every_call_with_its_exact_argument_vector ... ok
test exec::tests::script_fails_once_its_steps_are_used_up ... ok
test model::tests::a_label_vector_order_change_is_modified ... ok
test model::tests::an_open_flip_is_a_change ... ok
test config::tests::example_file_parses_with_every_override ... ok
test model::tests::apply_never_touches_another_repository ... ok
test model::tests::item_kind_gives_task_id_names ... ok
test model::tests::change_round_trips_through_json ... ok
test model::tests::later_additions_and_removals_report_the_item_identity ... ok
test model::tests::first_poll_reports_every_item_as_added ... ok
test model::tests::stage_names_round_trip_through_str ... ok
test model::tests::text_and_node_edits_are_stored_without_a_change ... ok
test model::tests::tracked_field_changes_are_reported_and_title_edits_are_not ... ok
test config::tests::load_resolves_owner_repo_through_the_executor ... ok
test exec::tests::real_exec_reports_a_missing_program_as_an_error ... ok

test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

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

## Deliberate decisions

- I left the `Mutex` values inside `ScriptExec`.
  The trait requires `Send + Sync`, and these values hold only test script state.
- I left the added `Copy`, `Eq`, `Ord`, and `Hash` derives.
  Later specified maps and tracker keys need these traits.
- I left first-poll items as `Added` changes.
  No prior repository entry exists, so each fresh item is an addition.
- I left `CmdOut::ok` as a small test constructor.
  It creates the required `CmdOut` value and adds no production behavior.

Final verdict: PASS
