---
area: scheduler
fast: cargo test daemon_keeps_stage_limit_across_repositories
---

## Sub-features

- The global stage limit across repositories.
- The lane reservations of one repository.

## How to get to it (user POV)

Set `limit = 3` under `[stage.implement]` in `factory.toml`. Run the daemon
over two repositories that both hold queued implement tickets. The pipeline
header shows at most 3 running tasks of the stage over both repositories
together.

## Driving it

cargo test daemon_keeps_stage_limit_across_repositories

The test builds a daemon over two repositories with three ready refine tickets
each and a refine limit of 2. It asserts 2 started runs and 4 queued tasks.

## Gotchas

`capacity_verdict` counts the running tasks of a stage over every repository.
`counts_by_stage_repo` feeds the lane reservations only. A reservation lowers
the free slots of a stage; it never raises the limit.
