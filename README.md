# AI Factory

AI Factory drives AI coding agents against GitHub issues in several
repositories. A daemon (a background program that runs without a terminal)
does the work. A terminal user interface (UI) shows the work and takes your
decisions.
GitHub is the source of truth. AI Factory keeps no journal and no database.

## The four stages

An issue moves through four fixed stages:

```
refine ──▶ implement ──▶ review ──▶ release
```

| Stage | Default harness and model | Result |
|---|---|---|
| refine | Claude, Opus | You shape the ticket. The issue gets the label `refined`. |
| implement | OpenCode, GLM-5.3-Flash | The agent writes the change and opens a draft pull request. |
| review | OpenCode, GPT-5.6 | The agent reviews the change and marks the pull request ready. |
| release | Claude, Opus | Release trains merge the ready pull requests. |

A release train is one batch of one or more pull requests. The factory merges
the pull requests in order.

Labels drive the flow: `to-refine`, `refined`, `needs-human`, and
`release-stacked`. Add the label `to-refine` to an issue to start the work.

The loop is event-driven. The daemon sleeps until a real deadline and wakes
only for real events. The only periodic clock is the 20-second GitHub poll
of each repository. The poll is conditional: an unchanged repository costs
almost nothing.

Each implementation issue gets one git worktree (a second repository
checkout). The implementation and review agents use this worktree. The
factory creates the worktree. The agents never create it.

## Install

You need:

- Rust and its `cargo` tool. Your command search path (`PATH`) must contain
  `cargo`.
- `git`
- the GitHub command-line interface (`gh`), with an active login
- each harness program that your six roles select

AI Factory supports Claude, OpenCode, and Codex. Claude roles require Claude Code 2.1.223 or later.

Run:

```sh
git clone git@github.com:navaro1/ai-factory.git
cd ai-factory
./install.sh
```

The installer builds the crate and installs `aif` and `aifd` into
`~/.local/bin`. It creates `~/.config/aif` and writes these files when they
do not exist:

| File | Purpose |
|---|---|
| `factory.toml` | Your configuration, copied from the example. |
| `factory.example.toml` | A portable reference copy of the example. |
| `prompts/*.md` | The four stage prompts and the ticket chat prompt. |

The installer never overwrites a file that exists. The installer keeps an
installed prompt file. After an upgrade, copy the new prompt files from
`docs/v0.6/prompts/` by hand. Edit `~/.config/aif/factory.toml` and set the
path of every repository.

## Configure

`factory.toml` uses configuration schema version 1. It requires all six global role tables.

```toml
schema_version = 1

[stage.refine]
harness = "claude"
model = "claude-opus-5[1m]"
limit = 3

[stage.implement]
harness = "opencode"
model = "zai-coding-plan/glm-5.3-flash"
limit = 3

[stage.review]
harness = "opencode"
model = "openai/gpt-5.6-sol"
effort = "xhigh"
extra_args = []
limit = 7

[stage.release]
harness = "claude"
model = "claude-opus-5[1m]"
limit = 1

[ticket.create]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"
permission_mode = "manual"
permission_handler = "inbox"
tools = ["Read", "Glob", "Grep"]
extra_args = []

[repo.borsuk]
path = "/home/you/Workplace/borsuk"
lanes = { implement = 1 }
release = { policy = "manual" }

[repo.borsuk.stage.review]
effort = "max"
```

- `harness` selects Claude, OpenCode, or Codex.
- `program` defaults to the selected harness command. AI Factory executes this string directly.
- `model`, `agent`, `profile`, and `effort` accept nonempty harness values.
- `effort` maps to the native effort or variant option of each harness.
- `extra_args` adds arguments that do not replace managed protocol options.
- `limit` caps concurrent tasks for one global pipeline stage.
- `path` sets the absolute path of one repository checkout.
- `lanes` reserves stage slots for one repository.
- `release.policy` is `manual`, `interval` with `minutes`, or `threshold`
  with `count`.

Claude supports `agent`, permission fields, tool lists, and `strict_mcp`.
OpenCode supports `agent` and `auto_approve`. Codex supports `profile`, `approval_policy`, and `sandbox`.

A repository role table can override individual fields. A harness change requires a complete role block.
The Settings view marks inherited repository values with `~`.

The example keeps ticket chat read-only through the exact Claude tool list.
You can configure a different tool list to permit write access.
Real Claude questions always reach the inbox in every permission mode.

AI Factory rejects managed, sharing, and combined bypass arguments in `extra_args`.
Use only the typed permission fields for dangerous native modes. The Settings view shows a warning.

A task binds its resolved settings when it starts. Retries, parked sessions, and restarts keep that binding.
Later configuration changes apply only to tasks without a binding.

Version 0.6.0 makes a clean configuration break. See `docs/v0.6/MIGRATION.md` for migration steps.

## Commands

| Command | Effect |
|---|---|
| `aif` or `aif tui` | Starts the daemon when needed, then opens the terminal UI. |
| `aif --paused` or `aif tui --paused` | Starts the daemon with the whole factory paused. |
| `aif stop` | Stops the daemon. |
| `aif doctor` | Reports on the installation and the configuration. |
| `aif doctor --clean` | Removes the worktrees of closed issues and merged pull requests. |

`aif doctor` checks each configured harness program once. It applies the Claude version floor only to Claude roles.

### Stop the daemon and update

`aif stop` sends the stop action to the daemon. The daemon stops every live
agent session, waits for the exits, writes its state, and exits. The wait can
take up to 40 seconds.

The same exit path serves a logout, a reboot, `systemctl --user stop
aif-daemon`, and `Ctrl-C` on a foreground `aifd run`. Each one sends a signal
that the daemon turns into the same stop action.

To update AI Factory, run:

```sh
aif stop
./install.sh
aif
```

The factory resumes: pause marks, attempt counts, queued messages, stuck
rows, and the tasks that ran at the stop come back.

### Start paused

`--paused` starts a new daemon with the factory paused. The daemon polls,
builds state, serves the socket, and reports to the UI. It dispatches no
task until you resume with `P` in the UI. The daemon flag is
`aifd run --paused`; `aif --paused` passes it through when `aif` starts the
daemon.

A new factory pointed at live repositories needs this flag. Without it, the
first poll fires the stage gates at once. Agents then create worktrees,
write code, and open pull requests before you see the UI. The flag cannot
apply when a daemon already runs. `aif` prints a message instead. Pause
that daemon with `P`, or stop it with `aif stop` and start again.
`aif doctor` reports the paused state. The UI shows a bold `paused` badge
in the header and marks every stage row. An exact stage, lane, or task state
marks its scope. A resumed item under a broader pause shows `resumed`.
A queued ticket that a pause blocks shows `paused` instead of `queued`.

## The five views

Keys `1` through `5` switch the views. `!` opens the oldest inbox row.
`?` opens the help overlay. `q` quits the UI.
The status bar of every view shows the open decision count.

### Pipeline, view 1

The pipeline shows Refine, Implement, Review, and Release as four side-by-side lanes.
Each lane groups its tickets by repository.
Each lane header shows its running and queued counts against the limit.
The release lane puts the next, active, or retry batch inside a border.
The waiting pull requests start below the border.
The oldest request is at the top, and the newest request is at the bottom.

| Key | Action |
|---|---|
| `j` / `k` or Up / Down | Move the selection inside a lane. |
| `h` / `l` or Left / Right | Move the selection between lanes. |
| `enter` | Open the session of the selected ticket. Every stage. Every state. |
| `+` / `-` | Change the limit of the selected stage. On a repository row, change its lane reservation. |
| `p` | Pause or resume the exact selected item. `P` pauses or resumes everything. |
| `r` | Refine the selected ticket in the session view. |
| `n` | Create a new ticket for the selected repository. |
| `x` | Abort the selected task, after a confirmation. |
| `R` | Retry a failed task. |
| Space | Add or remove the selected waiting pull request from the next release. |
| `g` | Start the outlined next release after a confirmation. |
| `s` | Cycle the release policy: manual, interval, threshold. |

Lowercase `p` follows the selected row:

- A stage row changes only that stage.
- A repository row changes only that repository lane in the current stage.
- A task row changes only that task.
- A release box or waiting pull request changes its repository release lane.

The most specific state wins. The order is factory, stage, repository lane,
and task. A second `p` changes the same state again. Uppercase `P` changes the
whole factory and removes all narrower states. A pause blocks future task
starts. It does not stop an active task.

### Session, view 2

The session view follows the log of one task. You see the agent output and
you converse with the agent. Press `enter` on a ticket in the pipeline to
open its session.

The input bar states what your message does. The daemon decides this per
task, and the bar never promises what the daemon refuses:

| Bar hint | What happens |
|---|---|
| `enter send` | A live claude chat takes the message at once. |
| `resumes the parked chat` | The message restarts a parked claude session. |
| `lands after this turn` | An opencode turn runs. The message becomes the next turn. |
| `starts a follow-up turn` | The task finished. The message continues the same session. |
| a reason, and a dim bar | The task takes no message. The reason says why. |

- Press `ctrl-x` to abort. On a running opencode task that holds a queued
  message, the abort makes that message start at once.
- Press `PageUp` and `PageDown` to scroll. Press `End` to follow the live
  output again.
- The chat bar holds the keyboard while it is focused. Press `esc` or
  `tab` to release it, `h` and `l` to move to the previous or next live
  session, and `i` or `enter` to take the keyboard back.
- A bar that cannot take a message holds no keyboard. With no shown task,
  a closed input, or a released focus, keys `1` through `5` switch the
  views and `?` opens the help.
- A pending question of the agent appears inline, with its options.

The implement and review agents run one turn per process. A turn that already
runs can not change course. So a message waits for the turn to end, and then
starts the next turn in the same agent session. The agent keeps the context of
its earlier work.

### Decisions inbox, view 3

The inbox is the core of the product. Everything that an agent cannot
decide alone arrives in one place:

- a tool permission that requires an operator response,
- a real question from an agent,
- a stuck task, after three failed attempts,
- an issue or pull request with the `needs-human` label,
- a release train that waits for your go.

The oldest decision is at the top. Each item starts with the decision message.
The selected item shows its choices and quick actions.

Press `enter` to open the source context:

- A release decision shows the pull request title and description.
- A `needs-human` decision shows the issue or pull request description.
- A task decision shows recent visible agent context from the exact task log.
- Press `o` in a task detail to open the full session.
- Press `esc` to return to the same feed item.
- Use Left and Right to move through a release batch.
- Use Page Up and Page Down when one feed item is taller than the screen.

Each decision type has one answer path:

| Row type | Keys |
|---|---|
| Permission | `y` allows. `n` denies, with a typed reason. |
| Question | `1`–`9` picks an option. `s` submits. `i` types a free answer. |
| Stuck | `r` retries. `c` cancels. |
| Needs human | `t` writes a comment and clears the label. `c` clears the label. |
| Release gate | `1`–`9` includes one pull request. Space changes all. `g` releases. |

There is no "allow always" key. This is deliberate. The wire protocol can
not carry a saved permission, and a key that promises more than it does
would break trust.

### Tickets, view 4

The Tickets view shows one tab per repository that has open issues.
The tabs follow the repository order of the configuration.
It excludes pull requests. It groups untouched, `to-refine`, and `refined`
issues inside the active tab.

| Context | Key | Action |
|---|---|---|
| List | `h` / `l` or Left / Right | Switch the repository tab. The switch wraps. |
| List | `/` | Search the active tab: number, title, and label text. |
| List | `enter` | Open the selected issue. |
| Issue | `e` | Edit the title and description. |
| Issue | `l` | Open the repository label picker. |
| Issue | `c` | Start or resume the configured ticket chat. |
| Issue | `a` | Apply the latest shown agent proposal. |
| Editor | `ctrl-s` | Save the content edit. |
| Label picker | Space | Apply one label change. |
| Label picker | `n` | Create and attach a repository label. |
| Conflict | `g` | Keep the GitHub version. |
| Conflict | `p` | Reapply the pending version after another fetch. |
| Nested view | `esc` | Return one level. |

The issue focus shows all issue details and the GitHub reference.
Wide terminals put the details and chat beside each other.
Narrow terminals put the chat below the details.

The example uses Claude with only `Read`, `Glob`, and `Grep`.
A different ticket chat role can permit repository changes.
AIF applies a shown proposal only after key `a`.

### Settings, view 5

The Settings view edits all six execution roles. It supports global and repository scopes.

| Key | Action |
|---|---|
| `h` / `l` | Select the global or repository scope. |
| `j` / `k` | Select a role, or a row inside an open list. |
| `Tab` | Select a field. |
| `Enter` | Open the value list of the field, or apply the marked row. |
| `d` | Remove the selected repository override. |
| `s` | Save the draft. |
| `r` | Reload the file. |
| `Esc` | Close a list, cancel an edit, or confirm draft removal. |

`Enter` opens a value list on these fields: `harness`, `program`, `model`,
`effort`, `agent`, `profile`, `permission mode`, `permission handler`,
`approval policy`, and `sandbox`. The list starts on the current value. Type
to filter the rows, `Backspace` shortens the filter, `Enter` applies the
marked row, and `Esc` closes the list without a change.

The candidates join three sources: the fixed values the harness documents,
the values the pushed settings state holds for the same field and harness,
and the discovered OpenCode models. `aif` runs `opencode models` once in the
background at start. The model list of an OpenCode role shows
`discovering models...` until the result arrives, and one dim reason row
when the probe fails. The state values and the custom row stay available.

An optional field starts with a `(none)` row that clears the field. The open
fields (`program`, `model`, `effort`, `agent`, `profile`,
`permission handler`) end with a `custom value...` row that opens the text
box, so no legal value becomes unreachable. `strict MCP` and `auto approve`
stay toggles. The argument and tool lists keep the row editor, and `limit`
keeps the text box.

A harness change sets the program, picks a default model, and clears every
field of the old harness. The model comes from the same global role, else
from the first row of the fixed harness table, else from the first sorted
candidate. `auto approve` turns off under OpenCode. One
notice line under the form names the new harness and every field that the
change reset. The line disappears at a save, a reload, a draft discard, or
the next harness change.

Narrow terminals stack the role list above the form.
The daemon rejects a stale save if the file changed after the draft loaded.
Repository topology changes require a daemon restart.

## How state survives

- GitHub carries the flow. The labels and the pull request states are the
  record.
- The worktree of each issue carries the agent session id and the last
  reviewed commit as marker files.
- `state.json` holds runtime overrides, role bindings, release times, and
  ticket chat metadata.
- `state.json` also holds one `runtime` object. It carries the pause marks,
  the task table with its attempt counts, the queued chat messages, the
  review ticket sets, the release batches, and the stuck rows. The daemon
  writes the object with every drive, so a crash keeps a snapshot that is at
  most one drive old.
- Task logs hold the full transcripts.
- After a restart, the first poll rebuilds everything from GitHub, and the
  runtime object restores the rest. A task that ran at the stop becomes
  queued again and resumes its agent session. Its first prompt carries a
  short notice about the restart.
- The stop sequence stops every live agent session before the daemon exits,
  so no agent process stays behind as an orphan.

## Known limits

- An external GitHub label change becomes visible at the next 20-second poll.
- An interrupted turn restarts from the stage prompt plus the restart
  notice. The agent reads the worktree to find its place.
- A `SIGKILL` or a power loss keeps the snapshot of the last drive, not of
  the last event.
- Anyone who can set the trigger labels on a repository can start work
  there.
- A permission answer is valid for one request only. You answer the same
  tool again next time.
- An implement or review turn that already runs can not change course. A
  message waits for the next turn.
- A release gate row refreshes at the poll after you stack a pull request.

## Development

The crate sits at the repository root. Run `./check.sh` before you push. It
runs the formatter, the linter, and the tests. The tests run offline. They
use fake binaries and recorded fixtures, and never touch the network or a
real agent.

The [v0.5 specification](docs/v0.5/SPEC.md) describes the base system.
The [v0.6 migration guide](docs/v0.6/MIGRATION.md) describes the configuration break.

To release a version:

```sh
git tag v0.x.0
git push --tags
```
