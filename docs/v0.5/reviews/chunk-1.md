# Task 1 review — crate scaffold

## Acceptance criteria

| Acceptance criterion | Final status | Verification |
|---|---|---|
| `cargo build` succeeds at the repository root. | Met | The command exited with status 0. |
| `./check.sh` passes. | Met | The final command exited with status 0. The exact output follows below. |
| `cargo run --bin aif -- --help` lists `tui`, `stop`, and `doctor`. | Met | The command listed all three names. An offline integration test also checks them. |
| `cargo run --bin aifd -- run --help` shows `--config`. | Met | The command showed `--config <CONFIG>`. An offline integration test also checks it. |
| The old Rust console builds, and no file under it changed. | Met | Its `cargo build` command exited with status 0. The base diff contains no protected path. |

## Findings and fixes

### F1 — Important — The CLI behavior had no automated tests

The first `cargo test` run ran zero tests. The command behavior had no check in the quality gate.

I added five offline integration tests in `tests/cli.rs`. They check both help screens, all placeholders, and the default `tui` command.

I temporarily changed the `doctor` command name to `health`. The help test failed on the missing `doctor` name.

I restored the correct name. All five tests then passed.

### F2 — Important — The source skeleton omitted the `exec` module

The current source layout includes `src/exec.rs`. Task 1 requires an empty file and a declaration for every module.

I added the documented stub. I also added `pub mod exec;` to `src/lib.rs`.

## Exact final `./check.sh` output

```text
== fmt ==
== clippy ==
    Checking aif v0.5.0 (/home/navaro/Workplace/ai-factory)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.14s
== test ==
   Compiling aif v0.5.0 (/home/navaro/Workplace/ai-factory)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.22s
     Running unittests src/lib.rs (target/debug/deps/aif-f159babf175e1f8d)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

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

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests aif

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

all checks passed
```

## Items left unchanged

I left `Cargo.lock` unchanged. Cargo uses this file to keep dependency versions stable for this application.

I left the old v0.4 tree unchanged. Global Constraint 11 forbids changes before Task 22.

Final verdict: PASS
