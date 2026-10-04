# RPC protocol

A headless unit is driven over JSON lines: one request or event per line. The
host does not assume a terminal, so any client (a terminal client, a web page, a
test harness) can drive it and render **generic UI requests**.

Protocol version: `1` (reported in the `ready` event).

## Requests (client to unit)

```json
{"type": "prompt", "text": "run the tests"}
{"type": "get_state"}
{"type": "ui_response", "id": "ui-1", "value": true}
```

| Type | Fields | Meaning |
| --- | --- | --- |
| `prompt` | `text` | Run a prompt through the agent |
| `get_state` | — | Report current state |
| `ui_response` | `id`, `value` | Answer a `ui_request` |

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

- `serve` in `pi-rpc` reads requests line by line and writes one or more events
  per request, flushing after each.
- The memory benchmark's `pi-native --rpc` idle mode is separate from this
  protocol; serving an agent is wired in the client milestone (VED-325).
