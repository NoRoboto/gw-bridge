# gw-bridge — Interface Contract

This document is the authoritative wire/CLI contract for `gw-bridge`. It exists so anyone can
build a client (OpenCode command, editor plugin, script, or another language) against the bridge
without reading the Rust source.

`gw-bridge` is legit by design: it only shuttles **text** and drives the **official `claude` CLI**
in its native stream-json mode. `claude` keeps its own Max/OAuth credentials; the bridge never
sees, stores, masks, or forwards them. No third-party routing of the subscription.

---

## 1. Topology

```
client(s)  ──unix socket (NDJSON)──►  gw-bridge serve  ──stdin/stdout (stream-json)──►  claude CLI
   ▲                                        │                                              (Opus)
   └────────────── broadcast events ────────┘
```

- One `gw-bridge serve` daemon multiplexes the socket across **N persistent `claude` sessions —
  one per project**. Every op carries a `project` key (a canonical directory); the daemon spawns
  (or reuses) that project's session manager on demand.
- Each project's session id is **persisted** (`$XDG_STATE_HOME/gw-bridge/sessions.json`, mapping
  project dir → session id) and **resumed across restarts**: close everything, come back tomorrow,
  and that project's Opus conversation picks up where it left off (`claude --resume`). The id is
  persisted only when a real turn spawns claude — a bare `status` ping never reserves one.
- The claude child runs **in the project directory** (so it loads that project's `CLAUDE.md`).
- A client connection binds to the first project it names: its writer streams **that project's**
  events; the stream is per-project, not a global firehose.
- A project's session is spawned lazily on its first `ask` and kept warm across turns.

## 2. Socket location

Resolved in this order:

1. `$XDG_RUNTIME_DIR/gw-bridge.sock`
2. `/tmp/gw-bridge-$USER.sock` (fallback when `XDG_RUNTIME_DIR` is unset)

The socket is a SOCK_STREAM unix domain socket. The wire format is **NDJSON**: one JSON object
per line, `\n`-terminated, in both directions.

## 3. Client → bridge operations

Send one JSON object per line. **Every op may carry `"project": "<canonical dir>"` and
`"lane": "<name>"`** to select the session. `project` defaults to `""` (or the client's current dir);
`lane` defaults to `"main"`. Each distinct `(project, lane)` is an **independent, concurrent** claude
session — e.g. lane `brain` (Opus) and lane `verify` (Sonnet) run at the same time on one project
without colliding. Reference clients take `--project` / `--lane`. Unknown ops are ignored.

Lane `worker` runs in the **reverse direction**: it is backed by headless `opencode run` (one
one-shot child per ask — spawned, streamed, reaped) instead of a persistent `claude` session, so a
Claude brain can delegate work down to the cheap OpenCode worker. Its model/agent come from the
routing config's `worker` entry (`{"model": "provider/model", "agent": "name"}`; an empty string
means opencode's own default); `effort` is accepted and ignored. The opencode session id is
captured from a run's events, persisted, and continued across asks via `--session`.

| op          | fields                                              | effect |
|-------------|-----------------------------------------------------|--------|
| `ask`       | `text` (string, required); `model`, `effort` (opt.) | Send one user turn to Opus. `model`/`effort` are **sticky** — they persist until changed, and a *different* value transparently re-homes the session via `claude --resume` (history preserved). |
| `config`    | `model`, `effort` (either/both)                     | Set the session-wide model/effort default **without** sending a turn. Takes effect on the next ask; no respawn if unchanged. Broadcasts a `config` event. |
| `interrupt` | —                                                   | Cancel the in-flight turn (kills + respawns the session). |
| `status`    | —                                                   | Ask the bridge to broadcast a `status` event (see §4). |
| `subscribe` | —                                                   | No-op; every connection already receives all events. |

`effort` levels: `low`, `medium`, `high`, `xhigh`, `max`. `model`: a `claude` alias (`opus`,
`sonnet`, …) or full model id.

```json
{"op":"ask","text":"Should the cache key include the tenant id?","effort":"high"}
{"op":"interrupt"}
{"op":"status"}
```

## 4. Bridge → client events

Every line the bridge emits is one JSON object with an `ev` field.

| ev             | fields                                                              | meaning |
|----------------|--------------------------------------------------------------------|---------|
| `init`         | `session_id`, `model`                                               | A claude session (re)started; reports its id + resolved model. |
| `delta`        | `text`                                                              | Streaming partial text of the assistant's reply. Concatenate in order. |
| `final`        | `text`                                                              | The assistant message, whole. Emitted even if deltas weren't. |
| `result`       | `text`, `is_error` (bool)                                           | The turn is complete. `text` is the final result string. |
| `heartbeat`    | `elapsed_ms`                                                        | Liveness ping while a turn is in flight (cadence: `GW_HEARTBEAT`). |
| `model_switch` | `model`, `effort`                                                   | A sticky model/effort change triggered a transparent respawn. |
| `config`       | `model`, `effort`                                                   | The session default was set via a `config` op (echoed to all clients). |
| `timeout`      | `after_ms`                                                          | The turn exceeded `GW_TURN_TIMEOUT` and was cancelled. |
| `idle_reclaim` | —                                                                  | The idle session was reclaimed after `GW_IDLE_TIMEOUT`. |
| `error`        | `text`                                                              | A non-fatal error (e.g. claude failed to spawn). Bridge stays up. |
| `session_ended`| —                                                                  | The claude session ended (crash/eof) **without** completing the turn. Not emitted on a deliberate model/effort respawn. |
| `status`       | `alive`, `in_flight`, `session_id`, `model`, `effort`, `idle_ms`   | Health snapshot, broadcast in response to `{"op":"status"}`. |

### Consuming a single turn (the `send` algorithm)

1. Connect, write `{"op":"ask","text":…}`.
2. Read lines. Print `delta.text` as it arrives (or buffer `final.text` if no deltas came).
3. Stop on `result` (success), or on `timeout` / `session_ended` / `error` (failure — surface it).
4. Ignore `heartbeat` / `init` / `idle_reclaim` / `model_switch` for a plain ask.

## 5. CLI surface

| command                                   | role |
|-------------------------------------------|------|
| `gw-bridge serve`                         | Run the daemon (socket + claude session manager). |
| `gw-bridge send [--model M] [--effort E] <text…>` | One turn; streams the reply to stdout. The reference client. |
| `gw-bridge config [--model M] [--effort E]` | Set the session-wide Opus model/effort default (no turn). For OpenCode's `/opus-mode`. |
| `gw-bridge interrupt`                     | Cancel the in-flight turn. |
| `gw-bridge status`                        | Print the health snapshot as JSON (`{"alive":false}` if the daemon is down). |
| `gw-bridge statusline`                    | One-line human indicator for Claude Code's `statusLine`. Never errors. |
| `gw-bridge tap`                           | Subscribe and print every raw event (NDJSON) — debug / live view. |
| `gw-bridge init [--scope global\|project] [--dir D] [--check]` | Install the Opus-escalation rule. `--check` exits 0 if configured, 2 if not. |
| `gw-bridge doctor`                        | Re-apply the escalation rule (e.g. after a global file gets clobbered). |
| `gw-bridge routing [--json] [--wizard] [--lane brain\|verify\|worker --model M --effort E --agent A] [--scope project\|global] [--dir D]` | Show or change the per-lane model routing. Layered config: built-in default < `~/.config/gw-bridge/routing.json` < `<project>/.gw-bridge/routing.json`. Changes re-render the escalation block. `--agent` (and no `--effort`) applies to the `worker` lane only. |

## 6. Environment knobs

Read by `serve`. Durations are in **seconds**; `0` disables that feature.

| var               | default | effect |
|-------------------|---------|--------|
| `GW_TURN_TIMEOUT` | `300`   | Kill a turn that runs longer than this. |
| `GW_HEARTBEAT`    | `15`    | Emit a `heartbeat` event every N seconds while in flight. |
| `GW_IDLE_TIMEOUT` | `600`   | Reclaim (kill) an idle session after N seconds; it respawns on the next ask. |
| `GW_MODEL`        | unset   | Default model for the session until an `ask` overrides it. |
| `GW_EFFORT`       | unset   | Default effort for the session until an `ask` overrides it. |

## 7. Guarantees & non-guarantees

- **Synchronous ask = ack.** `send` blocks until `result`; success/failure is immediate. There is
  no separate acknowledgement message — the streamed reply *is* the ack, `heartbeat` covers
  liveness, and `timeout` covers hangs.
- **Broadcast, not addressed.** All clients see all events. If you run two `ask`s concurrently from
  two clients, their deltas interleave on the bus. Serialize asks if you need clean separation
  (the `send` client issues one turn and exits).
- **Single brain.** There is one claude session per daemon. Model/effort are session-wide (sticky),
  not per-message — switching them respawns with `--resume`.
- **No credentials cross the wire.** Only text and the small control/event JSON above.
