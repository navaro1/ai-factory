# AI Factory

AI Factory drives AI coding agents against GitHub issues in several
repositories. A daemon (a background program that runs without a terminal)
does the work. A terminal UI shows the work and takes your decisions.
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
| review | opencode, model gpt-5.6-sol | The agent fixes findings and marks the pull request ready. |
| release | claude Opus | Release trains merge the ready pull requests. |

A release train is one batch of several pull requests, merged in order.

Labels drive the flow: `to-refine`, `refined`, `needs-human`, and
`release-stacked`. Add the label `to-refine` to an issue to start the work.

The loop is event-driven. The daemon sleeps until a real deadline and wakes
only for real events. The only periodic clock is the 60-second GitHub poll
of each repository. The poll is conditional: an unchanged repository costs
almost nothing.

Each issue gets one git worktree (a second checkout of the repository).
The factory creates the worktrees. The agents never create them.

## Install

You need:

- Rust, with `cargo` on your `PATH`
- `git`
- the GitHub CLI (`gh`), logged in
- the Claude Code CLI (`claude`), version 2.1.223 or later, logged in
- the opencode CLI (`opencode`), logged in

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
| `factory.example.toml` | A fresh reference copy of the example. |
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

- `limit` caps the concurrent tasks of a stage.
- `yolo` auto-approves the ordinary tool permissions of a stage. The default
  is `true`. A real question always reaches the inbox.
- `lanes` reserves stage slots for one repository.
- `release.policy` is `manual`, `interval` with `minutes`, or `threshold`
  with `count`.

`yolo` is not `--dangerously-skip-permissions`. That flag closes the control
channel, and then the agent can not ask you anything. AI Factory never
passes it.

## Start and stop

| Command | Effect |
|---|---|
| `aif` | Starts the daemon when none runs, then opens the terminal UI. |
| `aif stop` | Stops the daemon. |
| `aif doctor` | Reports on the installation and the configuration. |
| `aif doctor --clean` | Removes the worktrees of closed issues and merged pull requests. |
| `aifd run --config <path>` | Runs the daemon in a terminal, for debugging. |

The daemon start uses `systemd-run --user` with the unit `aif-daemon` when
systemd is present. Without systemd, `aif` spawns the daemon detached.

## The three views

Keys `1`, `2`, and `3` switch the views. `!` jumps to the oldest row of the
decisions inbox. `?` opens the help overlay. `q` quits the UI. The status
bar of every view shows the count of open decisions.

### Pipeline, view 1

The pipeline groups the tickets by stage, and inside a stage by repository.
Each stage header shows its running and queued counts against the limit.
The release group shows the queue, the stacked set, the policy, and the
countdown to the next fire.

| Key | Action |
|---|---|
| `j` / `k` | Move the selection. |
| `+` / `-` | Change the limit of the selected stage. On a repository row, change its lane reservation. |
| `p` | Pause or resume the selected stage or repository. `P` pauses or resumes everything. |
| `r` | Refine the selected ticket in the session view. |
| `n` | Create a new ticket for the selected repository. |
| `x` | Abort the selected task, after a confirmation. |
| `R` | Retry a failed task. |
| space | Stack the head of the release queue. |
| `g` | Fire the release train, after a confirmation that lists the pull requests. |
| `s` | Cycle the release policy: manual, interval, threshold. |

### Session, view 2

The session view follows the log of one task. You see the agent output and
you steer it.

- Type a message and press Enter. The UI sends it to the agent.
- Press `ctrl-x` to abort the task.
- Press `PageUp` and `PageDown` to scroll. Press `End` to follow the live
  output again.
- A pending question of the agent appears inline, with its options.

### Decisions inbox, view 3

The inbox is the core of the product. Everything that an agent cannot
decide alone arrives in one place:

- a tool permission, when `yolo` is off,
- a real question from an agent,
- a stuck task, after three failed attempts,
- an issue or pull request with the `needs-human` label,
- a release train that waits for your go.

Each row type has one answer path:

| Row type | Keys |
|---|---|
| Permission | `y` allow. `n` deny, with a typed reason. Enter opens the session. |
| Question | `1`–`9` pick an option. `i` types a free answer. Enter submits. |
| Stuck | `r` retry. `c` cancel. Enter opens the session. |
| Needs human | `t` write a comment and clear the label. `c` clears the label. |
| Release gate | `1`–`9` mark one pull request. Space marks all. `g` releases. |

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
