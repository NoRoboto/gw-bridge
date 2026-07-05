# gw-bridge

Cheap models code. Claude thinks. **gw-bridge** is the wire between them.

It's a small unix-socket broker that lets any agent (OpenCode, Claude Code, Cursor, a script)
consult a real Claude brain — on your **Max subscription**, through the **official `claude` CLI** —
without leaving its own session. Think "copy-paste between two terminals, but realtime and streamed".

> **How it stays compliant:** gw-bridge only moves text. It drives the official `claude` CLI, which
> keeps its own auth. No credential sharing, no token masking, nothing routed through a third party.

```
 worker (OpenCode)  ──ask──▶  gw-bridge  ──stream-json──▶  claude -p (official, Max auth)
                              (unix socket) ◀── streamed reply ──
```

## Why

Running Opus-class models for *everything* burns quota on work a cheap model does fine.
The cockpit pattern: a cheap worker (GLM, MiniMax…) reads and codes, and escalates only the
moments that deserve an expensive brain — architecture calls, tradeoffs, verification gates.

| Task | Runs on | How |
|------|---------|-----|
| Coding / implementation | cheap worker (native) | just do it — no bridge |
| Thinking / architecture / tradeoffs | Fable 5 (high) | `gw-bridge send --lane brain "…"` |
| Verification / "what breaks this" | Sonnet (high) | `gw-bridge send --lane verify "…"` |

Lanes are independent claude sessions per project, so a `brain` turn and a `verify` turn run
**concurrently** without colliding. Sessions persist and resume — come back tomorrow and each
project's brain still remembers the conversation.

## Quick start

```bash
cargo build --release && install -m755 target/release/gw-bridge ~/.local/bin/

gw-bridge serve                      # terminal A — the daemon
gw-bridge send "does this API break the slice-1 contract?"   # terminal B — ask and stream
gw-bridge init                       # install the escalation rule so workers know when to ask
```

Other verbs you'll want: `status`, `tap` (live event view), `interrupt`, `doctor` (re-apply the
rule if a sync clobbered it), `statusline` (one-liner for Claude Code's statusLine).

## Routing is config, not code

The lane→model mapping is layered — built-in default, then `~/.config/gw-bridge/routing.json`,
then `<project>/.gw-bridge/routing.json`:

```bash
gw-bridge routing --wizard                            # first-run questionnaire
gw-bridge routing --lane brain --model fable --effort high
```

Same idea for the escalation prompt the workers read: it's a markdown template
(`gw-bridge prompts --init` to customize), rendered from the routing config so the rule always
matches what the daemon actually runs. New models drop in as just another `--model` on a lane.

## Use it from anywhere (MCP)

```bash
claude mcp add gw-bridge -- gw-bridge mcp
```

Exposes `ask_opus(question, lane?, model?, effort?)` — streams via progress notifications,
forwards cancellation as interrupt — and `bridge_status(lane?)`. Any MCP client works.
For OpenCode there are also plain command files in `opencode/command/` (`/ask-claude`,
`/opus-mode`, `/gw-bridge-init`).

Full socket contract (NDJSON ops and events) lives in **`INTERFACE.md`**.

## Tuning

Env knobs on `serve`, in seconds (`0` disables): `GW_TURN_TIMEOUT` (300), `GW_HEARTBEAT` (15),
`GW_IDLE_TIMEOUT` (600 — kills the idle claude child; respawns+resumes on next ask).

## Roadmap

Working today: multi-project daemon, persistent+resumable sessions, concurrent lanes, per-turn
model/effort, MCP server, layered routing + prompt templates, statusline.

Next up:

- [ ] Clean mid-turn interrupt (today it kills + resumes the turn)
- [ ] Reverse lane as a proper OpenCode plugin
- [ ] One-command install via cargo-dist (installers, Homebrew, npm shim)
- [ ] **Phase 2:** Obsidian integration
