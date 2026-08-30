# ai-factory

A zellij workspace that turns a git repository into an AI agent factory.

v4 is event-driven: one per-repository daemon reacts to GitHub changes and
harness events immediately; a slow forced fetch repairs missed state every
ten minutes. Codex and opencode tasks run inside one lazy resident server
per factory. Claude stays supervised. Zellij becomes the control UI.

Run one command inside a repository. v4 gives you a cockpit tab plus one
tab per supervised agent. Automatic agents run with no TUI at all.

## What you get

Tab 1 — `Cockpit`

| Pane | Command |
|---|---|
| Cockpit | `aif tui` (daemon-driven task view) |

One tab per supervised node:

| Node | Command | Model |
|---|---|---|
| Planner | `clauded` (Claude Code) | `claude-fable-5[1m]` |
| Releaser | `clauded` (Claude Code) | `claude-opus-5[1m]` |

The Refiner, Implementer, and Reviewer run as automatic tasks through the
resident `codex app-server` and `opencode serve` processes. They appear as
cockpit cards, not panes.

The session name is `aif-<factory-id>-factory`. Start the command again to
re-attach. The daemon keeps running when the session closes.

## Requirements

- zellij (tested on 0.45.1)
- Claude Code CLI (`claude`), logged in
- opencode CLI (supported `>=1.18.25,<1.20.0`)
- Codex CLI (supported `>=0.150.1,<0.152.0`)
- GitHub CLI (`gh`), logged in
- git, bash; the `aif` binary ships from this repo (cargo builds it)

## Install

```sh
git clone git@github.com:navaro1/ai-factory.git
cd ai-factory
./install.sh
```

The installer copies the layout, prompts, theme, and commands into place
and builds the `aif` binary when cargo is available:

- `~/.config/zellij/themes/retro-future.kdl`
- `~/.config/zellij/layouts/ai-factory.kdl`
- `~/.config/zellij/prompts/*.md`
- `~/.local/bin/aif`, `ai-factory` (shim), `clauded`, `codexd`, `opencoded`

## Configure

**Graph.** `.aif/graph.kdl` declares the workflow:

```kdl
graph version=4 {
    limit 3

    node "refiner" limit=1 {
        agent "codex"
        model "gpt-5.6-sol"
        exec "auto"
        when "issue has label 'to-refine'"
        prompt ".aif/prompts/refiner.md"
    }
}
```

Rules: `limit` caps concurrent reserved work globally; node `limit`
(default 1) caps one node; `retrigger "head-sha"` starts a new Reviewer
task when the PR head changes; `exec "auto"` is rejected for claude.
Edges stay documentation-only. Local timing uses environment variables,
not the graph: `AIF_GITHUB_POLL` (default 60s), `AIF_RECONCILE`
(default 600s), `AIF_SERVER_IDLE` (default 600s).

**Prompts.** Repo-local one-item prompts live in `.aif/prompts/`. The
factory fills `{github_issue_no}` or `{gh_ticket_no}` with one item id.
The daemon creates one isolated worktree per automatic task; prompts must
not create worktrees themselves.

**Trust.** Automatic full-access execution needs one explicit approval:

```sh
aif trust
```

Polling cannot verify who changed a label. Any collaborator who can set a
matching label can trigger automatic work. `AIF_CODEX_YOLO=0` keeps the
restricted codex sandbox; `AIF_ALLOW_UNTESTED_HARNESS=1` bypasses the
version gates.

## Use

```sh
aif start                  # ensure the daemon, then open the lean UI
aif                        # cockpit (tasks, nodes, events)
aif status [--json]        # one-shot daemon state
aif pause [node]           # stop new dispatches
aif resume [node]
aif task submit <id>       # send supervised work into its pane
aif task cancel <id>
aif task retry <id>        # next attempt for a terminal task
aif task resolve <id> failed|succeeded|cancelled
aif task complete <id>     # supervised work finished
aif task fail <id>
aif stop [--force]         # stop the daemon
aif restart
aif events --follow        # journal tail and live follow
aif logs <task>            # bounded task log
aif cleanup                # remove clean worktrees of terminal tasks
aif doctor                 # binaries, protocols, graph, daemon
aif list                   # local factories
```

Cockpit keys: `1-9` select, `Enter` submit, `c` cancel, `r` retry,
`C`/`f` complete/fail, `p`/`P` pause/resume, `q` quit.

Task states: `queued`, `presenting`, `awaiting_user`, `reserved`,
`accepted`, `running`, `cancel_requested`, `uncertain`, `succeeded`,
`failed`, `cancelled`, `superseded`. Reserved, active, cancelling, and
uncertain tasks all consume capacity. Uncertain tasks never retry
automatically; an operator resolves them.

## Migration

```sh
aif graph migrate                      # preview
aif graph migrate --write              # apply with a v3 backup
aif graph migrate --write --auto-workers
```

Migration never changes a supervised node to automatic by itself;
`--auto-workers` performs that explicit step for codex and opencode nodes
and adds head-sha retrigger to draft-review nodes. A live legacy session
blocks the write. Rollback: restore `.aif/graph.kdl.v3.bak` and start v3.

## v3 compatibility

A graph without `version=4` keeps the whole v3 runtime: five panes, the
tick loop, `aif run --once`, and the ledger. Nothing migrates by itself.

## Memory

| Lever | Effect |
|---|---|
| Resident harness servers | One lazy `codex app-server` and one lazy `opencode serve` per factory; both stop after `AIF_SERVER_IDLE` with zero active tasks. No agent TUIs. |
| Scope isolation | The daemon runs inside the factory slice; `AIF_MEMORY_HIGH` still caps it. |
| `aif top` | Per-process RSS from `/proc` plus scope totals. |

## Development

The Rust console lives in `ui/console`. Run `./check.sh` before pushing;
it runs fmt, clippy, tests, and the tokens drift check. Tests never touch
the network or real harness binaries; protocol behavior uses scripted
transports.

To release:

```sh
git tag v0.x.0
git push --tags
```

## Memory

Five agent TUIs are heavy. The workspace attacks this on four fronts:

| Lever | Effect |
|---|---|
| Codex for the gpt-5.6-sol panes | A painted codex TUI idles at ~200 MiB against opencode's ~850 MiB. `codexd` execs the native binary directly, skipping the node launcher. Effort is pinned with `model_reasoning_effort=max` (`AIF_CODEX_EFFORT`). |
| opencode `--mini` for the rest | Roughly halves an idle opencode pane (~445 MiB). Disable with `AIF_OPENCODE_MINI=0`. |
| Scope isolation + throttle | `aif start` runs the session in `aif-<repo>-factory.scope` with `MemoryHigh` (default 3 GiB, `AIF_MEMORY_HIGH` to change, `0` disables). The factory throttles itself before the user slice reaches systemd-oomd pressure. |
| `aif top` | Per-process RSS from `/proc` plus scope totals, in the style of t3 Code's native resource monitor. |

Measured: a full factory dropped from ~3.2 GiB to ~1.4 GiB.

## Subagents at v3

The loop prompts tell each pane to dispatch subagents. All three
harnesses run subagents **inside the pane process** — a subagent costs
tokens, never a process:

| Harness | Mechanism | Notes |
|---|---|---|
| codex | `multi_agent.spawn` / `wait_agent` / `close_agent` tool set | In-process threads; children can spawn children; `codexd` pins `features.multi_agent=true`. Completed agents count toward concurrency until closed — tell agents to close finished work. |
| opencode | `task` tool → child session (`parentSessionId`) | In-process; one nesting level (subagents cannot call `task`); `background` flag is experimental. |
| claude | Task tool, `where:"in-process"` | In-process; nests to depth 3 (`CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`); subagents inherit the parent model (`CLAUDE_CODE_SUBAGENT_MODEL`). The harness default allows 20 concurrent subagents — `clauded` pins `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS=3` (`AIF_CLAUDE_MAX_SUBAGENTS` to change) so the prompt's "at most 3" is enforced by the engine, not by hope. |

So the v3 fleet scales by tokens, not RAM: ten parallel subagents under
one codex pane still measure ~200 MiB total for the harness.

For v0.4.0, `exec auto` no longer means a process per task. The plan now
follows the t3 Code pattern with resident harness servers: one persistent
`codex app-server` thread host and one `opencode serve` per factory, tasks
dispatched as in-process threads or sessions over RPC, torn down after an
idle timeout. Idle TUIs disappear the same way — the factory keeps one
server per agent kind instead of five panes.

Codex panes do not support the `/loop` line; `codexd` and the scheduler
strip it automatically. The 30-minute clock comes from the scheduler, not
from prompt magic: run `aif graph init` once per repo to write a starter
`.aif/graph.kdl`, then `aif run` dispatches on every tick. `aif start`
prints a reminder when a repo has no graph file.

`codexd` runs `--dangerously-bypass-approvals-and-sandbox` by default —
the same auto-approve stance as `opencode --auto` — because worktrees and
`gh` need writes and network outside the codex sandbox. Set
`AIF_CODEX_YOLO=0` to fall back to `-a never` (auto-approve, sandboxed).

## Known limits

- Claude shows a folder-trust dialog on first use in a new repository.
  Accept it, then start the workspace again. The draft then lands.
- If a pane boots slower than the readiness wait, its draft may be missing.
  Start the session again, or type the prompt by hand.
- The draft mechanism depends on zellij actions
  (`write-chars`, `dump-screen` with `--pane-id`).
- systemd-oomd can kill a whole terminal scope under memory pressure,
  taking every factory started from that terminal with it. `aif start`
  therefore runs the session inside its own user scope,
  `aif-<repo>-factory`. Five agent TUIs are heavy; avoid running several
  factories plus a browser in one user session, or raise the oomd
  thresholds. To shield a factory further:
  `systemctl --user edit --runtime aif-<repo>-factory.scope` and set
  `ManagedOOMPreference=avoid`.
