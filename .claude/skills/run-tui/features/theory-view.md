---
area: tui
fast: cargo test --lib -- tui::crt tui::theory a_governed_poll a_missing_model a_model_in_error a_state_view
---

# The theory view

Tab `6` of the terminal UI shows what the governor knows about each governed repository: the header strip, the AREAS panel, the HOLDS panel, and the DELTAS panel, drawn in the Amber CRT look inside a double-line frame.

## Sub-features

- The header strip: `GOVERNOR ON · ENTRIES n · AREAS n`, the window gauge, the pause word, or the model read error.
- The AREAS panel: one row per area with its tier mark.
- The HOLDS panel: one row per held item, with the bootstrap key on the hold that names an empty area.
- The DELTAS panel: one row per missed entry, one summary row per open delta, and one closed row per merged delta.
- The bootstrap chat: the session view that replaces the panels while the interview runs.

## How to get to it (user POV)

Start the terminal UI with `aif`, then press `6`. The strip draws under the `THEORY` title. `j` and `k` move the cursor, `esc` returns to the pipeline view.

## Driving it

    tmux new-session -d -s aif-tui -c "$PWD"
    tmux send-keys -t aif-tui 'cargo run -- tui' Enter
    tmux send-keys -t aif-tui 6
    tmux capture-pane -t aif-tui -p

The capture shows the strip and the frame glyphs. The fast command is the proof tier: it renders the view through the ratatui test backend and asserts the strip between the double-line border glyphs, the six CRT palette colors, and the daemon read and cache tests.

## Gotchas

The view draws only a repository that the daemon state carries, so an idle daemon shows `no repository`. With the governor off the strip reads `GOVERNOR OFF` and the panels stay hidden.
