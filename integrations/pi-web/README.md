# pi-web integration

How Pi Web drives pipelets unit agents instead of pi's embedded TypeScript SDK
([VED-342](https://linear.app/vedaru/issue/VED-342)).

The fork lives at `../pi-web` on the `pipelets` branch. Its `PIPELETS.md`
documents the details; this file records the approach.

## Run it

```bash
# 1. In the project the agent should work in, serve one unit:
pipelets --serve --session /path/to/session.jsonl --cwd "$PWD" \
  --addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …

# 2. In the pi-web fork:
PIPELETS_GATEWAY=http://127.0.0.1:30142 npm run dev
```

`--gateway --gateway-addr 127.0.0.1:30142` is the legacy spelling of the same
single-unit serve and keeps working.

When `PIPELETS_GATEWAY` is unset the fork behaves exactly as upstream.

## The route-compatibility seam (VED-420)

Rig owns the fleet and launches one `pipelets` process per seat (VED-417), so a
`pipelets` serve hosts **exactly one unit**. pi-web consumes a session over
`/sessions`, `/sessions/:id/events` (SSE), `/sessions/:id/commands`, and
`ui_response`; those routes stay in place for the unit, so **pi-web runs against
a single pipelets unit unchanged**.

Where a fleet points pi-web:

- **One unit** — point `PIPELETS_GATEWAY` directly at that unit's `--addr`.
- **A fleet** — point pi-web at **rig**, not at any pipelets process. rig owns
the multi-unit HTTP/SSE surface and proxies each seat to its own single-unit
serve; a seat is addressed by name, not by a pipelets `--addr`. Per-seat SSE
replay ids and the session-cwd behaviour are preserved across that proxy.

Until the rig surface exists, a single unit is served directly and every route
pi-web uses keeps working. Coordination (handoff, ack, ownership) is **rig's**,
not a pipelets route: there is no mailbox and no `/units/*` route in the unit
(VED-421).

## Authentication (threat model)

The gateway fronts a fully-capable agent: any peer that can reach the port can
run `bash`/`edit`/`write` with the gateway process's privileges, read full
transcripts, and approve `ui_request` prompts. Treat the port as remote code
execution and never expose it untrusted.

- **Loopback only by default.** `--addr` defaults to `127.0.0.1:30142`
  (`--gateway-addr` is the legacy spelling).
  Binding a non-loopback address without a token is refused at startup
  (fail closed).
- **Token auth.** Set `--gateway-token <token>` or
  `PIPELETS_GATEWAY_TOKEN=<token>`. Every route (including `GET /sessions`,
  the SSE stream, and the POST control plane) then requires
  `Authorization: Bearer <token>` (or `X-Pi-Token: <token>`) and answers `401`
  otherwise. The token is compared in constant time and never logged.
- **Host allowlist.** Requests whose `Host` header names anything other than a
  loopback name or the bound address are rejected with `403`, blunting
  DNS-rebinding attacks.

When using a token, export it to both processes:

```bash
# 1. the one unit
PIPELETS_GATEWAY_TOKEN=$(openssl rand -hex 24) \
  pipelets --serve --session /path/to/session.jsonl --cwd "$PWD" \
  --addr 0.0.0.0:30142 \
  --provider openai-completions --base-url … --model … --api-key …

# 2. pi-web fork (same token)
PIPELETS_GATEWAY=http://127.0.0.1:30142 \
PIPELETS_GATEWAY_TOKEN=<same token> \
  npm run dev
```

pi-web must attach the token to every request; without it the serve returns
`401` on the session list, streams, and commands.

## Approach

pi-web embeds pi's SDK in-process, so the integration is deliberately
**route-level and additive**, not a rewrite of `rpc-manager.ts`:

- `lib/pipelets.ts` (in the fork) implements the small session surface the
  agent routes and the SSE stream need: `isAlive`, `send`, `onEvent`,
  `isStreaming`, `streamingMessage`, `ready`, plus a registry.
- `app/api/agent/[id]/route.ts`, `app/api/agent/[id]/events/route.ts`, and
  `app/api/agent/new/route.ts` branch to the native registry when
  `nativeEnabled()`.
- The gateway's `?format=pi` stream already emits pi's canonical events, so the
  browser's existing projection (`lib/agent-event-wire.ts`) is unchanged.
- The embedded SDK remains the default; no SDK-only panel is disturbed.

## Session browsing already works

pipelets writes pi's session layout
(`sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`, `type: "session"`,
version 3), so `lib/session-reader.ts` lists, opens, and renders native sessions
without any fork change.

## Known limitations

- One working directory per serve (the unit's agent is built with a single
  cwd); run one serve per project, as rig does per seat.
- `set_tools` is fixed at unit creation (`recreated: false`).
- SDK-only panels (sub-agents, MCP, skills, plugins, project trust,
  exact system prompt, node-pty terminal) are not backed on the native path.
- `navigate_tree` maps to the gateway `fork` command.
- `bash` returns immediately; its output arrives on the event stream.
