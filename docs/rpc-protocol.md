# RPC protocol

A headless unit is driven over JSON lines: one request or event per line. The
host does not assume a terminal, so any client (a terminal client, a web page, a
test harness) can drive it and render **generic UI requests**.

Protocol version: `1` (reported in the `ready` event).

## Requests (client to unit)

```json
{"type": "prompt", "text": "run the tests"}
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
| `switch_session` | `sessionPath` | Load a session file and continue (resume) |
| `set_session_name` | `name` | Name the current session |
| `ui_response` | `id`, `value` | Answer a `ui_request` |

The rest of pi's RPC command surface (steering, abort, model/thinking control,
compaction, fork/clone, `export_html`, `bash`) is not implemented yet.

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
| `assistant_text` | `text` | A chunk of model text |
| `tool_start` | `name`, `input` | A tool call began |
| `tool_end` | `name`, `is_error`, `content` | A tool call finished |
| `done` | `stop_reason` | The turn finished |
| `state` | `messages` | Reply to `get_state` |
| `ui_request` | `id`, `kind`, `prompt`, `options` | A generic UI request |
| `error` | `message` | A protocol or turn error |

## Generic UI

`ui_request` carries a `kind` (`confirm`/`select`/`input`/`notify`), a prompt, and
up to a few options. The client renders it however it likes and replies with
`ui_response { id, value }`. This is how a headless unit asks a human something
without a terminal UI: approvals, choices, and free text all go through the same
message.

## Notes

- `serve` / `serve_with` in `pi-rpc` read requests line by line and **stream**
  events as the turn produces them (no whole-turn buffering); `serve_with` runs
  a hook after each prompt (e.g. session persistence).
- `serve_unit` backs approvals with the protocol: an approval-required tool
  emits `ui_request { kind: "confirm" }` and blocks the turn until the matching
  `ui_response` arrives. `--serve` uses it unless `--yolo`.
- `get_state` returns the unit's resolved context (`system` + `transcript` in
  pi's message shape), so a UI service can render it without owning the session.
- The memory benchmark's `pi-native --rpc` idle mode is separate from this
  protocol.
- `--session <path>` seeds the transcript and persists each turn back to the
  file (append; rewrite after compaction).
