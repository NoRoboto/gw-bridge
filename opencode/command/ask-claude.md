---
description: Ask Claude/Opus (central brain) through gw-bridge and stream the reply
agent: sdd-apply-gentleman-opus
---

OpenCode is the central interface you drive; Opus is the central brain, reached over `gw-bridge`
— a fast local unix-socket bridge to the OFFICIAL `claude` CLI (stream-json). It shuttles TEXT
only; claude keeps its own Max auth. No credential sharing, no masking.

PRECONDITION: the bridge daemon must be running in another terminal: `gw-bridge serve`.
If `send` reports it can't connect, tell the user to start it, then retry.

TASK:
1. Compose ONE clear, self-contained question for Opus from the user's input below. Include the
   minimal context (paths, the decision, options) — the brain has its own session but can't see
   your screen.
2. Run: `gw-bridge send "<the question>"`. It blocks and streams Opus's answer to stdout until
   the turn completes. Print that answer verbatim to the user.
3. If Opus asks for clarification, you may send one follow-up the same way, then stop. Do not
   loop more than 2–3 times.

TUNING THE BRAIN (optional flags on `send`, both sticky until changed):
- `--effort <low|medium|high|xhigh|max>` — reasoning depth. Default is fine for most asks; use
  `--effort high` or `max` for genuinely hard architecture/tradeoff calls. Higher = slower, deeper.
- `--model <alias>` — e.g. `opus` (default brain) or `sonnet` for cheaper/faster second opinions.
- Changing either transparently re-homes the session (conversation is preserved); don't flip flags
  every turn just to flip them — only when the task's difficulty actually warrants it.
  Example: `gw-bridge send --effort max "<a thorny design question>"`

The question to ask Opus: $ARGUMENTS
