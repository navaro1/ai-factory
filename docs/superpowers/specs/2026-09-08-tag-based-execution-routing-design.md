# Tag-Based Execution Routing

Date: 2026-09-08

Status: Review

## 1. Objective

AI Factory shall select implementation and review settings from GitHub complexity tags.

Each selected route contains the complete harness configuration. The configuration includes the harness, model, effort, permissions, tools, and arguments.

The Settings panel shall show and edit every route. A valid save shall activate the new routes without a daemon restart.

## 2. Scope

This design includes these functions:

- `complexity:*` selects the implementation route.
- `review-complexity:*` selects the review route.
- The system supports `low`, `medium`, `high`, and `very-high`.
- Global routes provide balanced defaults.
- Each repository can override each global route.
- The Settings panel edits global routes and repository overrides.
- The daemon stores the selected route with each started task.
- The task view shows the selected level, harness, model, and effort.

This design does not change the refine, release, ticket, or theory routes.

This design does not add arbitrary tag rules. It does not add one-task manual overrides.

This design does not create, remove, or copy GitHub tags.

## 3. Current System

`Config` stores typed settings for eight execution roles. It supports global settings and repository overrides.

`RoleSettings` contains all harness fields. `RoleOverride` contains optional values for the same fields.

The parser validates fields against Claude, OpenCode, or Codex. A harness change requires a model and clears old harness fields.

`Daemon::bind_task_role` resolves settings before the first run. The daemon stores the result in `state.json`.

Retries, resumed sessions, and daemon restarts use the stored result. This behavior prevents a route change during one task.

`Issue` and `Pr` already contain GitHub tag names. `Links` already connects a pull request to its linked issues.

The Settings panel edits role fields. The daemon validates each save and writes `factory.toml` with a file revision check.

The daemon activates normal role changes without a restart. Repository path, lane, remote, and release changes still require a restart.

The worktree contains an existing local change in `src/tui/mod.rs`. This feature must preserve that change.

## 4. Approved Decisions

The operator approved these decisions:

- Implementation uses `complexity:*`.
- Review uses `review-complexity:*`.
- Refine and release use their repository defaults.
- Missing tags select `medium`.
- Multiple valid tags select the highest level.
- A Settings save affects queued and future tasks.
- A started task keeps its stored route.
- The design uses a typed route matrix.
- The default matrix balances cost and quality.
- The Settings panel supports all route fields.

## 5. Default Matrix

The built-in matrix uses stable OpenAI model identifiers. Every route uses the Codex harness by default.

| Level | Implementation | Review |
|---|---|---|
| `low` | `codex`, `gpt-5.6-luna`, `high` | `codex`, `gpt-5.6-luna`, `xhigh` |
| `medium` | `codex`, `gpt-5.6-terra`, `high` | `codex`, `gpt-5.6-terra`, `xhigh` |
| `high` | `codex`, `gpt-5.6-sol`, `xhigh` | `codex`, `gpt-5.6-sol`, `max` |
| `very-high` | `codex`, `gpt-6-astra`, `xhigh` | `codex`, `gpt-6-astra`, `max` |

Each built-in route also uses these values:

```toml
program = "codex"
extra_args = []
auto_approve = false
approval_policy = "never"
sandbox = "workspace-write"
```

Other optional fields are absent. A global route or repository route can replace the harness and all related fields.

The model choice follows the current OpenAI model roles. Luna serves cost-sensitive work. Terra balances cost and capability.

Sol serves complex professional work. Astra serves the hardest work.

Sources:

- https://developers.openai.com/api/docs/models/gpt-5.6-luna
- https://developers.openai.com/api/docs/models/gpt-5.6-terra
- https://developers.openai.com/api/docs/models/gpt-5.6-sol
- https://developers.openai.com/api/docs/models/gpt-6-astra

## 6. Configuration Model

The code shall add a closed `ComplexityLevel` type. Its order is `low < medium < high < very-high`.

The code shall add a closed `TagRouteStage` type. It contains `implement` and `review`.

A route key contains one stage and one level. `Config` shall hold one effective `RoleSettings` value for every route key.

The parser shall create the eight built-in routes first. It shall apply optional global overrides second.

The parser shall apply optional repository overrides last. The existing `RoleOverride` merge and validation rules shall apply.

The configuration uses these tables:

```toml
[tag_routes.implement.low]
harness = "codex"
model = "gpt-5.6-luna"
effort = "high"

[tag_routes.review.very-high]
harness = "codex"
model = "gpt-6-astra"
effort = "max"

[repo.gh-klon.tag_routes.implement.high]
harness = "opencode"
model = "zai-coding-plan/glm-5.3"
effort = "max"
auto_approve = true

[repo.gh-klon.tag_routes.review.high]
model = "gpt-6-astra"
effort = "max"
```

All route tables are optional. An absent global table keeps the built-in route.

An absent repository table keeps the effective global route. A repository table can contain one field or many fields.

A table that changes `harness` must also contain `model`. This change shall reset fields from the old harness.

`SettingsEdit` shall gain global route and repository route variants. The existing text editor shall preserve unrelated comments and table order.

The config schema version stays at `1`. The new tables are optional, so old configuration files remain valid.

## 7. Tag Selection

The selection code shall use one pure function. The function receives a route stage and an ordered tag list.

The function shall recognize exact, case-sensitive tag names. Implementation recognizes only these tags:

- `complexity:low`
- `complexity:medium`
- `complexity:high`
- `complexity:very-high`

Review recognizes only these tags:

- `review-complexity:low`
- `review-complexity:medium`
- `review-complexity:high`
- `review-complexity:very-high`

The function shall ignore unknown tags. It shall select `medium` when it finds no valid tag.

The function shall select the highest level when it finds multiple valid tags. This rule gives one safe result without tag order.

An implementation task reads tags from its issue snapshot.

A review task reads tags from its pull request and all linked issues. The task uses the highest valid level from the combined set.

A review task can start before the daemon can resolve a linked issue. The task then uses the pull request tags or `medium`.

The selector shall return evidence with the level and matched tag names. It shall also name direct or linked tag sources.

## 8. Task Binding and Data Flow

The daemon shall resolve the tag route inside `bind_task_role`. The route resolves immediately before the first process starts.

The daemon shall use the current snapshot and link table. A queued task has no stored route before this point.

The daemon shall store the complete effective settings and route evidence in `state.json`. A new optional field shall keep old state files valid.

The stored evidence contains these values:

- The route stage.
- The selected complexity level.
- The matched tags.
- The direct or linked source items.
- The effective source scope.

The effective source scope is `built-in`, `global`, or `repository`. Field source data remains available for the Settings panel.

A Settings save replaces the active configuration after validation. A queued task then uses the new route at its first dispatch.

A running task keeps its stored route. A retry, resume, parked session, and restored task also keep that route.

A terminal task loses its route through the existing cleanup path. A new task for a later head commit resolves a new route.

The daemon shall write one short route line to the task log before runner output. The line names the level, tags, harness, model, and effort.

## 9. Settings Panel

The existing Settings view shall add route rows below the role rows. The row order stays stable.

The order is:

1. Existing execution roles.
2. Implementation routes from `low` through `very-high`.
3. Review routes from `low` through `very-high`.

The `j` and `k` keys move through all rows. The `h` and `l` keys continue to change the scope.

The route form reuses the existing role field editor. It shows harness, program, model, effort, arguments, and all harness fields.

The global scope shows built-in values and global changes. The repository scope shows effective values and local changes.

Each field shows its source. The source is built-in, global, or the selected repository.

The `s` key saves the current route. The daemon validates, writes, activates, and returns the existing result types.

The `d` key removes a route override. In global scope, this action restores the built-in route.

In repository scope, this action restores the effective global route. The action does not change other route rows.

The panel shall show the tag name above the field list. It shall show a short preview with the effective harness, model, and effort.

The narrow layout shall use one vertical row list and one field form. It shall not require a wide matrix.

The active task detail shall show the stored route level. It shall also show the stored harness, model, and effort.

## 10. Validation and Failures

The parser shall reject unknown keys, stages, and levels. It shall reject empty models and invalid harness fields.

The parser shall require all eight effective routes after it applies defaults. This invariant makes route lookup complete.

The system shall not change to another route after a model failure. A dispatch failure uses the existing retry and stuck flow.

The task log shall make the failed route visible. The Settings panel shall keep the invalid draft and show the validation error.

The Doctor command shall check each unique harness program from roles and routes. It shall not start a paid model call.

The Doctor report cannot prove model account access. A model access error can occur only at dispatch.

A stale Settings save shall fail through the existing file revision check. An invalid save shall not change the file or live configuration.

Route changes do not alter repository topology. They shall not require a daemon restart.

## 11. Code Areas

The implementation will change these main areas:

- `src/config.rs` adds route types, defaults, parsing, resolution, edits, and validation.
- `src/daemon.rs` selects and stores one route at first dispatch.
- `src/state.rs` stores optional route evidence.
- `src/sock.rs` sends route settings and task route evidence.
- `src/tui/settings.rs` shows and edits route rows.
- `src/tui/mod.rs` routes the existing Settings actions and results as needed.
- `src/doctor.rs` checks route harness programs.
- `docs/v0.5/factory.example.toml` documents route overrides.
- `docs/v0.6/MIGRATION.md` explains the optional tables and live behavior.
- Tests cover the new configuration, daemon, state, socket, runner, and TUI behavior.

The implementation shall keep unrelated source changes intact.

## 12. Test Strategy

Configuration tests shall cover all built-in routes. They shall cover global and repository precedence.

Configuration tests shall cover comment preservation. They shall cover route removal and a harness replacement.

Selector tests shall cover each exact tag. They shall cover missing, unknown, and multiple tags.

Selector tests shall cover direct pull request tags and linked issue tags. They shall prove the highest-level rule.

Daemon tests shall cover the route at first dispatch. They shall verify the exact harness, model, effort, and runner arguments.

Daemon tests shall prove that a queued task uses a new route after a Settings save.

Daemon tests shall prove that running, retried, resumed, parked, and restored tasks keep their stored route.

State tests shall load an old file without route evidence. They shall save and reload a new route binding.

Socket tests shall cover the new Settings data. They shall verify protocol compatibility rules.

Settings tests shall cover global and repository rows. They shall cover source markers, save, reload, removal, and invalid drafts.

Draw tests shall cover the narrow terminal layout. They shall show the route tag and effective route.

Doctor tests shall check unique route programs. Installer tests shall check the example configuration.

The final implementation shall pass `./check.sh` from the repository root.

## 13. Acceptance Criteria

The change is complete when all these statements are true:

- A `complexity:low` issue starts with the configured low implementation route.
- A `review-complexity:high` linked issue selects the high review route.
- A direct `review-complexity:very-high` tag raises the review route to `very-high`.
- A missing tag selects the medium route.
- A repository override changes only that repository.
- A Settings save changes a queued task without a daemon restart.
- A Settings save does not change a started task.
- The task detail and task log show the selected route evidence.
- An invalid route does not change the file or the active configuration.
- Old configurations and old state files remain valid.
- The full repository quality gate passes.

## 14. Excluded Work

This version does not support custom tag prefixes or custom level names.

This version does not support ordered rules or Boolean tag expressions.

This version does not support a temporary route change for one task.

This version does not copy issue tags to pull requests. It reads both direct and linked tags.

This version does not test paid model access in the Doctor command.

## 15. Open Questions

There are no open product questions. The implementation plan can choose small internal function and file boundaries.
