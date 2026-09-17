---
name: run-daemon
description: Drive the factory daemon scheduler over fake runners in tests.
surface: daemon
driver: cargo
tier: terminal
---

The daemon surface is the scheduler of `aifd`. The tests build a `Daemon` over
fake runners and a scripted command runner, so no real agent process starts.

## Run

cargo test --lib

## Fast

cargo test daemon_keeps_stage_limit_across_repositories

## Drive

Run one named test with `cargo test` plus the test name. The rig of
`src/daemon.rs` builds a daemon over temporary repositories. `Rig::make_in`
takes the scripted git steps and the config tweak that shape the scenario.
A poll posts one snapshot per repository with `Inbound::Poll`.

## Gotchas

The stage limit counts running tasks over every repository together. A lane
reservation of one repository never raises the limit of its stage. The
live-process limit of a stage is global in the same way.
