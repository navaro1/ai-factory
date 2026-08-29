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
- GitHub CLI (`gh`), logged in — the default prompts use it
- git, python3, bash

## Install

```sh
git clone git@github.com:navaro1/ai-factory.git
cd ai-factory
./install.sh
```

The installer copies the layout, prompts, theme, and commands into place:

- `~/.config/zellij/themes/retro-future.kdl`
- `~/.config/zellij/layouts/ai-factory.kdl`
- `~/.config/zellij/prompts/*.md`
- `~/.local/bin/ai-factory`, `clauded`, `opencoded`

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
ai-factory                        # start the full workspace
ai-factory help                   # full guide
ai-factory --skip planner         # factory tab only
ai-factory --skip refiner,reviewer
ai-factory --skip factory         # planner tab only
```

zellij keys: `Ctrl t` tabs, `Ctrl p` panes, `Alt hjkl` move focus,
`Ctrl q` leave the session. The session keeps running in the background.

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

To release a new version:

```sh
git tag v0.x.0
git push --tags
```

## Known limits

- Claude shows a folder-trust dialog on first use in a new repository.
  Accept it, then start the workspace again. The draft then lands.
- If a pane boots slower than the readiness wait, its draft may be missing.
  Start the session again, or type the prompt by hand.
- The draft mechanism depends on zellij actions
  (`write-chars`, `dump-screen` with `--pane-id`).
