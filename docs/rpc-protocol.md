# RPC protocol

A pipelets unit is driven over JSON lines: one request or event per line. The
host does not assume a terminal, so any client (a terminal client, a web page, a
test harness) can drive it and render **generic UI requests**.

Protocol version: `1` (reported in the `ready` event).

## Requests (client to unit)

```json
{"type": "prompt", "text": "run the tests"}
{"type": "reset"}
{"type": "get_messages"}
{"type": "get_tree"}
{"type": "switch_session", "sessionPath": "/path/to/session.jsonl"}
{"type": "ui_response", "id": "ui-1", "value": true}
```

Commands reply with pi's envelope: `{"type":"response","id":?,"command":?,
"success":?,"data":{...}}`.

| Type | Fields | Meaning |
| --- | --- | --- |
| `prompt` | `text` (alias `message`) | Run a prompt through the agent |
| `get_state` | — | System prompt + transcript + message count |
| `get_messages` | — | The resolved transcript (pi message shape) |
| `get_entries` | `since`? | Session entries, optionally after `since` |
| `get_tree` | — | Session as `{tree, leafId}` nodes |
| `get_last_assistant_text` | — | Most recent assistant text |
| `get_session_stats` | — | Session counts and file/name |
| `new_session` | `parentSession`? | Start an empty session (new file) |
| `reset` | `text`?, `rerun`? (default `true`) | Clear the context in place and re-run `text` (else the first user message) |
| `switch_session` | `sessionPath` | Load a session file and continue (resume) |
| `set_session_name` | `name` | Name the current session |
| `steer` / `follow_up` | `message` | Queue a message for the next prompt (steering before the model call, follow-up after the turn) |
| `abort`, `clear_queue`, `abort_retry`, `abort_bash` | — | `abort`/`abort_retry`/`abort_bash` are no-ops for a synchronous unit; `clear_queue` drains both queues |
| `set_model`, `cycle_model`, `get_available_models` | `provider`, `modelId` | Record the active model (the provider is fixed at startup) |
| `set_thinking_level`, `cycle_thinking_level`, `get_available_thinking_levels` | `level` | Thinking level `off`…`max` |
| `set_steering_mode`, `set_follow_up_mode` | `mode` | `all` or `one-at-a-time`; any other value returns `success:false` |
| `compact` | `customInstructions`? | Force compaction now (ignores `auto_compaction`) |
| `set_auto_compaction` | `enabled` | Enable/disable automatic threshold compaction for this session |
| `set_auto_retry` | `enabled` | Enable/disable the transport retry loop (429/5xx) for this session |
| `bash` | `command`, `excludeFromContext`? | Run a shell command out of band |
| `export_html` | `outputPath`? | Write the session to an HTML file |
| `fork`, `clone`, `get_fork_messages` | `entryId` | Branch/duplicate the session |
| `get_commands` | — | Slash commands (none built in; extensions are a later slice) |
| `ui_response` | `id`, `value` | Answer a `ui_request` |

This covers pi's 33 RPC commands, plus `reset` for fresh-context execution.
`set_model` records the model but does not swap the live provider, and
`abort`/`abort_retry`/`abort_bash` are no-ops for a synchronous unit;
`get_commands` returns built-ins only. The remaining control flags are
**honoured**, not merely recorded: `set_auto_compaction` gates the agent's
threshold compaction, `set_auto_retry` gates the transport retry loop, and
`set_steering_mode`/`set_follow_up_mode` control whether a prompt delivers all
queued messages or one at a time (`steer` before the model call, `follow_up`
after the turn). An unsupported mode returns `success:false` with an `error`.

Peer shouts are **not** an RPC command: the swarm inlet delivers them as user
messages before each model call. See [swarm](swarm.md).

`reset` clears the transcript **in place**: the session file keeps its id and
header (so the unit stays addressable), only its message entries are dropped.
With `rerun: true` (the default) the unit then re-runs the task — the `text`
field when given, otherwise the first user message in the cleared transcript —
streaming the turn's events after a `reset` response. `rerun: false` clears and
stops, which is what the swarm conductor sends before each card so a unit never
carries one growing transcript across cards (VED-373).

## Events (unit to client)

```json
{"type": "ready", "version": 1}
{"type": "assistant_text", "text": "..."}
{"type": "tool_start", "name": "bash", "input": {"command": "ls"}}
{"type": "tool_end", "name": "bash", "is_error": false, "content": "..."}
{"type": "done", "stop_reason": "end_turn"}
{"type": "state", "messages": 4}
{"type": "ui_request", "id": "ui-1", "kind": "confirm", "prompt": "Run rm -rf?", "options": []}
{"type": "error", "message": "..."}
```

| Type | Fields | Meaning |
| --- | --- | --- |
| `ready` | `version` | Sent once at startup |
| `agent_start` / `agent_settled` | — | A run started / will not continue automatically |
| `turn_start` / `turn_end` | — | One model call and its tool calls |
| `assistant_delta` / `thinking_delta` | `text` | Incremental provider output while streaming |
| `assistant_text` | `text` | The authoritative text for the turn |
| `tool_start` | `tool_call_id`, `name`, `input` | A tool call began |
| `tool_end` | `tool_call_id`, `name`, `is_error`, `content` | A tool call finished |
| `done` | `stop_reason` | The agent loop finished |
| `state` | `messages` | Reply to `get_state` |
| `response` | `command: "reset"`, `cleared`, `reran`, `text` | Reply to `reset` (a `done`/turn events follow when `reran`) |
| `ui_request` | `id`, `kind`, `prompt`, `options` | A generic UI request |
| `error` | `message` | A protocol or turn error |

## pi event compatibility

pi and pi-web consume pi's canonical event stream
(`@earendil-works/pi-coding-agent/docs/json.md`): `agent_start/end/settled`,
`turn_start/end`, `message_start/update/end` with `assistantMessageEvent`
deltas, and `tool_execution_start/update/end` with tool-call ids. The native
stream above is the transport; `pi_rpc::PiEventAdapter` (and `translate_all`)
folds it into those exact shapes, including synthesized `message_start` /
`message_update` / `message_end` and `toolCallId`. The tool-call ids are carried
natively (`tool_call_id`) so the adapter does not have to invent them.

The provider streams incrementally: `assistant_delta` and `thinking_delta`
arrive as they are produced (from `pi_net::stream_sse_with`), and the adapter
emits `text_delta` / `thinking_delta` for each. `assistant_text` is the
authoritative final value for the turn and does not duplicate the deltas.

## Generic UI

`ui_request` carries a `kind` (`confirm`/`select`/`input`/`notify`), a prompt, and
up to a few options. The client renders it however it likes and replies with
`ui_response { id, value }`. Extensions can use it to ask a human something
without a terminal UI (approvals, choices, and free text go through the same
message); tools themselves do not prompt — they run as in pi.

## Notes

- `serve` / `serve_with` in `pi-rpc` read requests line by line and **stream**
  events as the turn produces them (no whole-turn buffering); `serve_with` runs
  a hook after each prompt (e.g. session persistence).
- `serve_session` serves a unit with a session file and runs tools without
  approval, matching pi.
- `get_state` returns the unit's resolved context (`system` + `transcript` in
  pi's message shape), so a UI service can render it without owning the session.
- The memory benchmark's `pipelets --rpc` idle mode is separate from this
  protocol.
- `--session <path>` seeds the transcript and persists each turn back to the
  file (append; rewrite after compaction).
