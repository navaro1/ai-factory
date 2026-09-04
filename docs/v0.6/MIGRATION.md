# Migrate to AI Factory 0.6

Version 0.6 replaces the old runner settings. It does not support compatibility aliases.

## Required structure

Add `schema_version = 1` at the top of `factory.toml`.

Add all six global role tables:

- `[stage.refine]`
- `[stage.implement]`
- `[stage.review]`
- `[stage.release]`
- `[ticket.create]`
- `[ticket.chat]`

Each table requires `harness` and `model`. The harness value is `claude`, `opencode`, or `codex`.

The `program` field is optional. Its default value is the selected harness command.

AI Factory starts `program` as one executable string. It does not parse a shell command.

Use [the installer example](../v0.5/factory.example.toml) as a complete configuration.

## Removed settings

Replace each removed setting before you start the daemon.

| Removed setting | Replacement |
|---|---|
| `runner` | Set `harness`. Set `program` only for a custom executable. |
| `variant` | Set `effort`. |
| `yolo` | Use the permission field for the selected harness. |
| `[ticket_chat]` | Use `[ticket.chat]`. Also add `[ticket.create]`. |

The parser reports direct migration errors for these names. It does not accept the old names.

OpenCode uses `auto_approve`. Codex uses `approval_policy` and `sandbox`.

Claude uses `permission_mode`, `permission_handler`, tool lists, and `strict_mcp`.

AI Factory always keeps real Claude questions in the inbox.

## Repository overrides

A repository table can override one role field:

```toml
[repo.borsuk.stage.review]
effort = "max"
```

The global role supplies every field that the repository table omits.

A harness change starts a new role contract. Set its `harness` and `model` fields.

The selected harness supplies the default `program`. Old harness fields do not pass to the new harness.

Keep pipeline capacity in the global stage `limit`. Keep repository capacity in `repo.<alias>.lanes`.

## Ticket chat access

The installer example keeps ticket chat read-only with this exact Claude tool list:

```toml
[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"
permission_mode = "manual"
permission_handler = "inbox"
tools = ["Read", "Glob", "Grep"]
extra_args = []
```

You can change the tool list to permit write access. Add only the Claude tools that the role needs.

The ticket prompt no longer sets read-only access. The role settings control access.

## Prompt updates

The refine prompt now writes an implementation plan with chunks, ownership,
dependencies, validation, and parallel waves.

The implement prompt uses those waves. It can start up to three independent
subagents with separate file ownership.

The installer writes the version 0.6 prompts only when a prompt file is absent.
An existing prompt file still overrides the built-in prompt.

The installer now writes `prompts/ticket.md` too. The ticket-creation prompt
takes a file override for the first time. Version 0.5 read no file for it.

Compare each existing prompt with `docs/v0.6/prompts/`. Replace the existing
copy when you want the new execution contract.

The Settings view edits each prompt in place. Select a role, press `Tab` to
the `prompt` field, and press `Enter`. `ctrl-s` saves the file. `d` on the
row restores the built-in prompt. A saved prompt applies to the next task of
the role.

## Extra arguments

Each `extra_args` list passes separate arguments to the harness program.

Do not add protocol, model, directory, session, permission, or output flags.

AI Factory rejects those managed flags. It also rejects OpenCode sharing arguments and combined bypass arguments.

Use typed permission fields for dangerous native modes. The Settings view shows a clear warning.

## Task settings and reloads

A new task binds its resolved role settings when it starts.

A retry, a parked session, or a daemon restart keeps that binding.

A configuration save affects only tasks that have no binding.

The Settings view uses key `5`. It can save role changes and reload the file.

A stale save does not change the file. Repository topology changes require a daemon restart.

## Doctor checks

Run `aif doctor` after the migration.

The doctor checks each configured harness executable once. It checks the Claude version floor only for Claude roles.
