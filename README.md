# ai-factory

A zellij workspace that turns a git repository into an AI agent factory.

Run one command inside a repository. You get two tabs with five agents.
Each factory pane starts with its prompt already typed in. You check the
text, then press Enter to send it.

## What you get

Tab 1 — `Planner - <repo>`

| Pane | Command | Model |
|---|---|---|
| Planner | `clauded` (Claude Code) | `claude-fable-5[1m]` |

Tab 2 — `AI factory - <repo>`

| Pane | Command | Model |
|---|---|---|
| Refiner | `opencoded` (opencode, auto) | `openai/gpt-5.6-sol` |
| Reviewer | `opencoded` (opencode, auto) | `openai/gpt-5.6-sol` |
| Implementer | `opencoded` (opencode, auto) | `zai-coding-plan/glm-5.3-flash` |
| Releaser | `clauded` (Claude Code) | `claude-opus-5[1m]` |

The factory panes run loop prompts. The Refiner refines tickets labelled
`to-refine`. The Reviewer works through draft PRs. The Implementer picks up
tickets labelled `refined`. The Releaser merges and deploys. Edit the prompt
files to change this flow. See [Configure](#configure).

The session name is `<repo>-factory`. Start the command again to re-attach.

## Requirements

- zellij (tested on 0.45.1)
- Claude Code CLI (`claude`), logged in
- opencode CLI (tested on 1.18.25), with your providers configured
- GitHub CLI (`gh`), logged in — the graph engine queries it
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
- `~/.local/bin/aif`, `ai-factory` (shim), `clauded`, `opencoded`

It also sets `theme "retro-future"` in your zellij config. The theme applies
to all your zellij sessions. Remove the line if you do not want that.

## Configure

**Models.** Open `zellij/layouts/ai-factory.kdl`. Each pane has a
`--model` argument. Change it to any model your tools support. Then run
`./install.sh` again.

**Prompts.** Open the files in `zellij/prompts/`:

| File | Pane |
|---|---|
| `refiner.md` | Refiner |
| `reviewer.md` | Reviewer |
| `implementer.md` | Implementer |
| `releaser.md` | Releaser |

The prompt text is plain Markdown. The workspace types it into the pane at
start. The Planner pane has no prompt file — it starts empty.

The default prompts use the `/loop every 30m` command and `gh` queries.
Make sure your agent tool supports them, or adapt the text.

**Colors.** Open `zellij/themes/retro-future.kdl` and edit the palette.

**Reasoning effort (optional).** To run the OpenAI and Z.AI models at
maximum effort, add this to your opencode config
(`~/.config/opencode/opencode.jsonc`):

```jsonc
{
  "provider": {
    "openai": {
      "options": {
        "reasoningEffort": "max"
      }
    },
    "zai-coding-plan": {
      "options": {
        "reasoningEffort": "max"
      }
    }
  }
}
```

**Concurrency.** Every loop prompt caps the work at 3 subagents at the
same time. Edit the `Dispatch at most 3 subagents` line in a prompt file
to change this.

## Use

```sh
aif start                       # start the factory session (ai-factory is a shim)
aif                             # open the cockpit TUI
aif status [--json]            # one-shot pane map
aif start --skip planner       # factory tab only
aif start --skip refiner,reviewer
```

zellij keys: `Ctrl t` tabs, `Ctrl p` panes, `Alt hjkl` move focus,
`Ctrl q` leave the session. The session keeps running in the background.

Cockpit keys: `1-5` select, `Enter` submit a waiting draft, `r` press
enter in the pane, `s` next pane, `l` scrollback, `q` quit.

## Graph mode

A repo can carry `.aif/graph.kdl`. It declares the agent graph: nodes with
agents, models, and `when` conditions over GitHub state; edges for
documentation. See this repo's own `.aif/graph.kdl` as the example.

```sh
aif graph validate             # parse and check the graph
aif graph dot                  # Graphviz export
aif run --once --dry-run       # show what would dispatch
aif run --once                 # dispatch ready tasks into idle panes
aif run                        # loop on the graph tick
aif events                     # the JSONL dispatch log
```

When the graph file exists, the static pane drafts stand down. The
scheduler owns the prompts: it fills the ticket number into
`{github_issue_no}` or `{gh_ticket_no}` and pastes the prompt into an
idle pane. You press Enter in the pane to send it.

Rules: one task per pane, `limit` concurrent dispatches, one dispatch per
ticket per cycle. Dependencies come from "blocked by #N" in the issue
body; a ticket with open blockers never reaches the Implementer.

## How the typed prompts work

Each pane command accepts `--draft-file <path>`. A background job in the
pane waits until the TUI is ready, then asks the zellij server to write the
text into the pane as a bracketed paste. Nothing is sent. You press Enter.

This is why the drafts appear a few seconds after the panes start.

## Versioning and development

The repo uses git tags for versions (`v0.1.0` is the first release). To
work on it, edit the files here, run `./install.sh`, and test inside a
scratch git repository. Send a pull request with a short description of
the change.

**Console (`aif`).** The Rust console and graph engine live in
`ui/console`. The color source of truth is `ui/tokens/tokens.json`; the
zellij theme is generated from it:

```sh
cargo run --manifest-path ui/console/Cargo.toml -- tokens zellij
```

CI does not exist in this repo on purpose. Run `./check.sh` before you
push; it runs fmt, clippy, tests, and the tokens drift check.

To release a new version:

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
