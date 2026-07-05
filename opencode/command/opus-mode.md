---
description: Set the model/effort gw-bridge uses for Opus this session (no turn sent)
---

Set which Claude **model** and **reasoning effort** gw-bridge drives Opus with for THIS session.

This does NOT change OpenCode's own worker model — it is OpenCode acting as the control panel for
the `claude` CLI behind the bridge. It is **sticky**: the choice persists for the session and the
bridge only re-homes Opus (respawn with `--resume`, history kept) if the value actually changed —
setting the same value again does nothing.

PRECONDITION: the bridge daemon must be running (`gw-bridge serve`). If the command can't connect,
tell the user to start it.

From the arguments below, extract whichever are present:
- `effort` — one of: low, medium, high, xhigh, max
- `model` — a Claude alias (e.g. `opus`, `sonnet`)

Then run exactly this, including ONLY the flags the user actually provided:

```
gw-bridge config [--effort <effort>] [--model <model>]
```

If neither was provided, instead run `gw-bridge status` and report the current model/effort.
Print the command's output verbatim, then stop.

Arguments: $ARGUMENTS
