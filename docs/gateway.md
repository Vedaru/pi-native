# Unit serve

`pipelets --serve --addr <host:port>` serves **one** unit over HTTP + SSE so a web
UI or remote client can attach to it. Rig owns the fleet and launches one
`pipelets` process per seat (VED-417), so each process hosts exactly one session:
a later `POST /sessions` replaces the open unit rather than growing a fleet. The
legacy `--gateway`/`--gateway-addr` spelling is an alias for the same single-unit
serve.

```bash
pipelets --serve --session /path/to/session.jsonl --cwd /path/to/project \
  --addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …
```

Sessions are addressed by id, not by a file descriptor; any number of clients can
attach, and an idle unit releases its in-memory agent until the next command.

## Routes

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/sessions` | List the unit's session id (exactly one) |
| `GET` | `/swarm` | Status snapshot of the unit |
| `POST` | `/sessions` | Open the session (`{"sessionPath"?: …, "cwd"?: …}`); replaces the current unit |
| `GET` | `/sessions/:id` | Resolved state |
| `DELETE` | `/sessions/:id` | Remove the unit |
| `GET` | `/sessions/:id/commands` | Extension slash commands |
| `POST` | `/sessions/:id/title` | Generate a session title from the transcript |
| `GET` | `/sessions/:id/events` | SSE stream (replay + live) |
| `POST` | `/sessions/:id/commands` | Send a command (`prompt`, `steer`, `abort`, …) |
| `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |

Coordination (handoff, ack, ownership) lives in rig, not in pipelets: there is
no mailbox and no `/units/*` route. Peer *awareness* is a file-based shout bus
([swarm](swarm.md)), not an HTTP surface. See
[the web integration](../integrations/pi-web/README.md) for where a fleet points
pi-web (rig).

By default the SSE stream carries the native event envelope; add `?format=pi`
to receive pi's canonical events (`agent_start`, `message_update`,
`tool_execution_*`, `agent_settled`) through `pi_rpc::PiEventAdapter`.

Units run tools without asking, matching pi. Binding a non-loopback address
without `--gateway-token` is refused at startup; see
[the web integration](../integrations/pi-web/README.md) for the threat model.

## pi-web compatibility seam

pi-web embeds pi's SDK in-process and talks to a session over `/sessions`,
`/sessions/:id/events` (SSE), `/sessions/:id/commands`, and `ui_response`. Those
routes stay in place for this process's single unit, so **pi-web runs against a
single `pipelets` unit unchanged** (point `PIPELETS_GATEWAY` at the one unit's
`--addr`).

A **fleet**, though, is addressed at **rig**, not at a pipelets process: rig owns
the multi-unit HTTP/SSE surface and proxies each seat to its own single-unit
serve. Point pi-web at rig's surface (one base URL for the fleet) once it is
available; per-seat addressing is by seat, not by a pipelets `--addr`. Until
then, a single unit is served directly and every route pi-web uses keeps
working. See [the pi-web integration](../integrations/pi-web/README.md) for the
fork details.

The fleet surface (`GET /swarm`, coordination handoff/ack/ownership) is owned by
**rig**, not by a pipelets process. A single unit exposes only the `/sessions/*`
routes pi-web uses (VED-421).

The unit carries **no clock and no run ledger**: it has no trigger engine and no
`--triggers` mode. Scheduling lives in rig, which drives a scheduled prompt over
the serve like any other client.
