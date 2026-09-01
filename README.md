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

| Stage | Agent and model | Result |
|---|---|---|
| refine | claude Opus, one interactive chat | You shape the ticket. The issue gets the label `refined`. |
| implement | opencode, model glm-5.3-flash | The agent writes the change and opens a draft pull request. |
| review | opencode, model gpt-5.6-sol | The agent reviews the change and marks the pull request ready. |
| release | claude Opus | Release trains merge the ready pull requests. |

A release train is one batch of one or more pull requests. The factory merges
the pull requests in order.

Labels drive the flow: `to-refine`, `refined`, `needs-human`, and
`release-stacked`. Add the label `to-refine` to an issue to start the work.

The loop is event-driven. The daemon sleeps until a real deadline and wakes
only for real events. The only periodic clock is the 60-second GitHub poll
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
- the `claude` command-line interface for Claude Code, version 2.1.223 or later
- the `opencode` command-line interface, with an active login

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
| `prompts/refine.md` and three more | The default prompt of each stage, for you to edit. |

The installer never overwrites a file that exists. Edit
`~/.config/aif/factory.toml` and set the path of every repository.

## Configure

`factory.toml` sets the agent and the concurrency limit of every stage, and
the repositories:

```toml
[stage.refine]
model = "claude-opus-5[1m]"
runner = "claude"
limit = 3
yolo = true

[stage.implement]
model = "zai-coding-plan/glm-5.3-flash"
runner = "opencode"
limit = 3

[stage.review]
model = "openai/gpt-5.6-sol"
runner = "opencode"
variant = "xhigh"
limit = 7

[stage.release]
model = "claude-opus-5[1m]"
runner = "claude"
limit = 1

[repo.borsuk]
path = "/home/you/Workplace/borsuk"
lanes = { implement = 1 }
release = { policy = "manual" }

[repo.qubitsok]
path = "/home/you/Workplace/qubitsok"
release = { policy = "threshold", count = 3 }
```

- `model` sets the model identifier that the runner receives.
- `runner` selects the `claude` or `opencode` command.
- `variant` sets the optional effort level for `opencode`.
- `limit` caps the concurrent tasks of a stage.
- `yolo` auto-approves the ordinary tool permissions of a stage. The default
  is `true`. A real question always reaches the inbox.
- `path` sets the absolute path of one repository checkout.
- `lanes` reserves stage slots for one repository.
- `release.policy` is `manual`, `interval` with `minutes`, or `threshold`
  with `count`.

`yolo` is not `--dangerously-skip-permissions`. That flag closes the control
channel, and then the agent can not ask you anything. AI Factory never
passes it.

## Commands

| Command | Effect |
|---|---|
| `aif` or `aif tui` | Starts the daemon when needed, then opens the terminal UI. |
| `aif --paused` or `aif tui --paused` | Starts the daemon with the whole factory paused. |
| `aif stop` | Stops the daemon. |
| `aif doctor` | Reports on the installation and the configuration. |
| `aif doctor --clean` | Removes the worktrees of closed issues and merged pull requests. |

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

## The three views

Keys `1`, `2`, and `3` switch the views. `!` jumps to the oldest row of the
decisions inbox. `?` opens the help overlay. `q` quits the UI. The status
bar of every view shows the count of open decisions.

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
- A pending question of the agent appears inline, with its options.

The implement and review agents run one turn per process. A turn that already
runs can not change course. So a message waits for the turn to end, and then
starts the next turn in the same agent session. The agent keeps the context of
its earlier work.

### Decisions inbox, view 3

The inbox is the core of the product. Everything that an agent cannot
decide alone arrives in one place:

- a tool permission, when `yolo` is off,
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

## How state survives

- GitHub carries the flow. The labels and the pull request states are the
  record.
- The worktree of each issue carries the agent session id and the last
  reviewed commit as marker files.
- `state.json` in `~/.local/state/aif` holds only the runtime overrides and
  the last release times.
- After a restart, the first poll rebuilds everything. Work resumes in
  place.

## Known limits

- A label change becomes visible at the next poll, at most 60 seconds later.
- A restart kills the agent processes of the running tasks. The gates
  re-open that work at the next poll.
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

The file `docs/v0.5/SPEC.md` specifies the whole system.

To release a version:

```sh
git tag v0.x.0
git push --tags
```
