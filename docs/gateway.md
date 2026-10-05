# Unit gateway

`--gateway` serves the unit host over HTTP + SSE so a web UI or remote client can
attach to long-lived agents. Sessions are addressed by id, not by a file
descriptor; any number of clients can attach, and an idle unit releases its
in-memory agent until the next command.

```bash
pipelets --gateway --gateway-addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …
```

## Routes

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/sessions` | List session ids |
| `GET` | `/swarm` | Status snapshot of every unit (multi-agent dashboard) |
| `POST` | `/sessions` | Open/create a session (`{"sessionPath"?: …, "cwd"?: …}`) |
| `GET` | `/sessions/:id` | Resolved state |
| `DELETE` | `/sessions/:id` | Remove a unit |
| `GET` | `/sessions/:id/commands` | Extension slash commands |
| `POST` | `/sessions/:id/title` | Generate a session title from the transcript |
| `GET` | `/sessions/:id/events` | SSE stream (replay + live) |
| `POST` | `/sessions/:id/commands` | Send a command (`prompt`, `steer`, `abort`, …) |
| `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |
| `GET` | `/units/:id/messages` | Poll a unit's mailbox |
| `POST` | `/units/:id/messages` | Send a unit-to-unit message |

By default the SSE stream carries the native event envelope; add `?format=pi`
to receive pi's canonical events (`agent_start`, `message_update`,
`tool_execution_*`, `agent_settled`) through `pi_rpc::PiEventAdapter`.

Units run tools without asking, matching pi. Binding a non-loopback address
without `--gateway-token` is refused at startup; see
[the web integration](../integrations/pi-web/README.md) for the threat model.

The unit carries **no clock and no run ledger**: it has no trigger engine and no
`--triggers` mode. Scheduling lives in rig, which drives a scheduled prompt over
the gateway like any other client.
