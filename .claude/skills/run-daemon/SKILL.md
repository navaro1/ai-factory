---
name: run-daemon
description: Launch and drive the aif daemon chat wire for verification.
surface: daemon
driver: cargo
tier: terminal
blind: live harness behaviour, network
---

## Run

`cargo run --bin aifd -- run` at the repository root starts the daemon event loop in the foreground. `aif stop` or Ctrl-C stops it. A user install runs the same loop under the systemd unit `aif-daemon`; `aif` or `aif tui` starts the daemon when none runs.

## Fast

`cargo test image` in the repository root. It drives the chat wire from the socket action through the three harness protocols and the queue round trip, all offline against fake children and recorded fixtures, and finishes in seconds on a warm target directory.

## Auth or seed

None for the fast command. A live daemon reads the factory config at `~/.config/aif/factory.toml` and serves its socket at `$XDG_RUNTIME_DIR/aif/daemon.sock`.

## Drive

    cargo test image

The run prints one `ok` line per test and names every test with `image`. For a live socket drive, start the daemon, wait for a task that holds a session, then send one JSON line to the socket:

    printf '%s\n' '{"action":"chat","task":"alias/implement-i42","text":"check this screenshot","images":["/abs/path/shot.png"]}' \
      | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/aif/daemon.sock"

The task log gains one user line whose text names each image file, and the runner protocol carries the images on the next turn.

## Logs

The daemon prints to stderr; under the systemd unit the output lands in the journal, and `journalctl --user -u aif-daemon | grep the alias` finds one poll. Each task log is one JSON stream under the state root, and every accepted chat message appends exactly one user line.

## Gotchas

The daemon queues a chat message for a one-shot harness instead of steering it, and the queue survives a restart through `state.json`. A queued message written by an older daemon loads with no images. The harness children in the tests are fake binaries; no test touches the network or a real harness.
