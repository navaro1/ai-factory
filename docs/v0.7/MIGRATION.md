# The v0.7 migration note

One file per breaking change of the theory governor, written by the chunk
that ships the change.

## The required `[theory.audit]` table

The `[theory.audit]` role table is required since C16. The audit role now
carries production dispatches:

- The review of a model-only pull request. The factory opens the model pull
  request of every model edit as a draft, the review runs under
  `theory.audit` with the changed entries in its prompt, and the audit agent
  approves it with `gh pr ready`.
- The drift sweep of `aif doctor --audit` and the cadence fires.
- The grading of every answered card.

A `factory.toml` without the table now fails with:

```
theory.audit is required; see docs/v0.7/MIGRATION.md
```

Add the table to fix it. One harness and one model are enough:

```toml
[theory.audit]
harness = "claude"
model = "claude-opus-5[1m]"
```

`docs/v0.5/factory.example.toml` carries the table, so a fresh install
needs no edit.

## The governor default and the escape

The theory governor is on by default for every repository of `factory.toml`.
One escape exists per repository:

```toml
[repo.borsuk]
path = "/srv/borsuk"
governor = "off"
```

A repository with the governor off keeps its v0.6 behaviour: no gate, no
label, no event, no card, and no sweep of the governor runs for it. The
Settings view shows `WARNING: the theory governor is off` and `aif doctor`
prints one `Warn` line for it.

## The theory labels

The governor owns these GitHub labels. The factory creates each one when it
first needs it, so an upgrade needs no manual step:

| Label | On | Meaning |
|---|---|---|
| `theory-short` | a theory record | the short prediction was accepted |
| `theory-full` | a theory record | the full prediction was accepted |
| `delta-open` | a theory record | the review left an open delta |
| `event-open` | a theory record | one theory event waits for the operator |
| `model-pr` | a pull request | the pull request edits the model |

The `[labels]` table renames none of them; they follow the governor, not
the pipeline.
