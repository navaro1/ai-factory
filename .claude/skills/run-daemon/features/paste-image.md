---
area: daemon
fast: cargo test image
---

# The image chat wire

One chat message can carry image files next to its text. The message travels from the socket action through the daemon queue into the three harness protocols, and the task log records it as one JSON line that names every attached file.

## Sub-features

- The socket action: `Action::Chat` carries an `images` field next to `text`, and one JSON line round trips it.
- The claude protocol: a user line whose content is an array with one text block and one base64 image block per image, with the media type sniffed from the magic bytes.
- The codex protocol: one `turn/start` whose input holds the text item and one `localImage` item per image with an absolute path.
- The opencode protocol: one `-f` flag per image before the prompt positional, which stays the last argument.
- The queue: a queued message keeps its text and images together across `pending_chats`, the next turn, and a daemon restart through `state.json`.
- The log: one user line per accepted message that names every attached image file.

## How to get to it (user POV)

The operator sends a chat message to one task of the pipeline with attached image files saved under the AIF state root. The daemon steers a live session or queues the message for the next turn, and the transcript of the task shows the message with the image names.

## Driving it

    cargo test image

The run proves each sub-feature through the public path: the socket round trip, the fake claude child, the parrot codex child, the opencode argument vector, the queue round trip with the old state form, and the logged user line. Every test name carries `image`; the exit code is the verdict.

## Gotchas

The claude runner reads the image bytes at send time, so a missing file or unknown magic bytes fail the send naming the file. The codex image path must be absolute. A text-only message keeps the old wire shapes everywhere, and a `state.json` written before the images field loads its queued texts with no images.
