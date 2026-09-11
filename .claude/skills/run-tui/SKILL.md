---
name: run-tui
description: Launch and drive the aif terminal UI for verification.
surface: tui
driver: tmux
tier: terminal
blind: true color output, mouse events
---

## Run

`cargo run -- tui` at the repository root. The command starts the daemon when none runs, then opens the terminal UI. The ready signal is the pipeline view with the tab bar and the footer hints. Stop with `tmux kill-session -t aif-tui`; stop the daemon with `cargo run -- stop`.

## Fast

`cargo test --lib -- tui::crt tui::theory a_governed_poll a_missing_model a_model_in_error a_state_view` in the repository root. It renders the views and the daemon polls through the test backend and finishes in seconds on a warm target directory.

## Auth or seed

None. The TUI reads the factory config at `~/.config/aif/factory.toml` and needs no credentials. A state view with no repositories still renders the tab bar and the empty views.

## Drive

    tmux new-session -d -s aif-tui -c "$PWD"
    tmux send-keys -t aif-tui 'cargo run -- tui' Enter
    tmux send-keys -t aif-tui 6
    tmux capture-pane -t aif-tui -p

Tab `6` opens the Theory view. The strip `GOVERNOR ON · ENTRIES n · AREAS n` or the model read error draws under the `THEORY` title, inside the double-line frame. `j` and `k` move the cursor. `v` asks for a run skill, `t` teaches, `b` bootstraps an empty area, `e` edits the model. `esc` returns to the pipeline view.

## Logs

The TUI prints to the terminal only. The daemon writes its log at `$XDG_STATE_HOME/aif/daemon.log`; a grep for the repository alias finds one poll.

## Gotchas

The TUI needs a raw terminal. Run it inside tmux; a pipe shows nothing. The Theory view draws one block per repository that the daemon state carries, so an idle daemon shows `no repository`.
