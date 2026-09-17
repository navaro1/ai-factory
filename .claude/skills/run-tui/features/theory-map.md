---
area: tui
fast: cargo test --lib -- tui::theory::map the_map_draws a_map_wider j_walks_the_map h_and_l_cycle
---

# The theory map

The MAP panel of the Theory view, at the bottom of the body. It draws one area of the marked repository at a time: the boundary as the outer double-line frame titled with the area id in uppercase, the states as boxes on rows by transition depth, the transitions as `[FROM]──title──▶[TO]` arrows with the title truncated to 8 cells, and the failures that cross the boundary as `⚠ <id> CROSSES <boundary>` lines under the frame.

## Sub-features

- The area cycle: `h` and `l` move the shown area of the marked repository, clamped, no wrap, and the frame title follows.
- The entry cursor: `j` and `k` walk the map entries as stops between the repository header and the area rows, and the bottom strip shows `<ENTRY-ID>: <statement>` while the mark sits on an entry.
- The truncation: a line wider than the pane and content taller than the pane truncate with `…`, without a panic.

## How to get to it (user POV)

Start the terminal UI with `aif`, press `6`, and move to a governed repository. The map draws under the panels.

## Driving it

    tmux new-session -d -s aif-tui -c "$PWD"
    tmux send-keys -t aif-tui 'cargo run -- tui' Enter
    tmux send-keys -t aif-tui 6
    tmux capture-pane -t aif-tui -p

The fast command is the proof tier: it renders the map through the ratatui test backend and asserts the arrow line in the frame, the crossing failure under the frame, the ellipsis at 40 columns, the `j` walk with the statement strip, and the `h`/`l` cycle.

## Gotchas

The pane draws only when the marked repository is governed and carries at least one area. An area whose model carries no entries still draws its empty frame.
