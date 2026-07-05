---
description: Delegate a task down to the cheap OpenCode worker through gw-bridge
---

Claude Code is the brain; the OpenCode worker (GLM/MiniMax…) is the cheap executor, reached over
`gw-bridge` — the same local unix-socket bridge, in reverse. Lane `worker` runs one headless
`opencode run` per ask (session continued across asks). The bridge shuttles TEXT only; each tool
keeps its own auth. No credential sharing, no masking.

PRECONDITION: the bridge daemon must be running in another terminal: `gw-bridge serve`.
If `send` reports it can't connect, tell the user to start it, then retry.

TASK:
1. Compose ONE clear, self-contained task for the worker from the user's input below. Include
   everything it needs (paths, acceptance criteria, constraints) — the worker has its own session
   but cannot see your screen.
2. Run: `gw-bridge send --lane worker "<the task>"`. It blocks and prints the worker's report to
   stdout when the run completes (each run pays opencode's ~10s startup — be patient).
3. Relay the worker's report to the user, then verify anything that matters before relying on it
   — you are the brain; the worker executes.

ROUTING (optional): the worker's model/agent come from the routing config —
`gw-bridge routing --lane worker --model provider/model --agent <name>` (empty = opencode's own
defaults). A `--model` flag on `send` overrides the model stickily; effort does not apply here.

The task to delegate: $ARGUMENTS
