# ADR 0003: Web UI, unit host, and triggers

- Status: Proposed
- Date: 2026-10-05
- Issues: VED-337 (this), VED-338, VED-339, VED-340, VED-341, VED-342, VED-343
- Deciders: pi-native maintainers

## Context

We want `pi-web` (https://github.com/agegr/pi-web) to be the UI end for
pi-native unit agents, and we want units to run long sessions and run on
triggers.

Findings from the investigation:

- `pi-web` is a Next.js app that embeds pi's TypeScript SDK **in-process**
  (`lib/rpc-manager.ts`, ~2500 lines). It is not a thin wire client. The browser
  talks to ~30 `/api/*` route families and 115 React components.
- The clean seam is `AgentSessionWrapper.inner: AgentSessionLike`
  (`lib/pi-types.ts`).
- pi itself ships a canonical RPC event vocabulary
  (`@earendil-works/pi-coding-agent/docs/json.md`): `agent_start/end/settled`,
  `turn_start/end`, `message_start/update/end` + `assistantMessageEvent`,
  `tool_execution_start/update/end`, `queue_update`, `compaction_*`, `retry_*`,
  `extension_ui_request/response`. pi-web consumes exactly these.
- pi-native's RPC matches pi's **commands** (VED-335) but emits a coarser,
  non-standard **event** set (`assistant_text`, `tool_start`, `tool_end`,
  `done`, `usage`, `ui_request`).
- Session files were incompatible: `SessionJournal` wrote
  `{"type":"header","version":1}`. pi and pi-web require
  `{"type":"session","version":3}` and the layout
  `<agent>/sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`. Fixed by VED-338.

## Decision

1. **Fork pi-web** and keep its frontend, SSE plumbing, session viewer, file
   browser, and most API routes. Replace only the embedded agent runtime with a
   pi-native client adapter.
2. **Align pi-native's RPC event stream to pi's `json.md`** rather than build a
   lossy translator. This also makes pi-native consumable by pi's own
   `RpcClient`.
3. Introduce a **unit host** (supervisor) that addresses units by session id,
   not file descriptor. It owns attach/detach, event fan-out + replay, idle
   suspend/wake, and crash recovery. The session JSONL remains the source of
   truth with one writer per session.
4. Expose units over an **HTTP + SSE gateway** with pi-web-compatible semantics
   (snapshot + live + `Last-Event-ID` replay, backpressure that drops
   rebuildable deltas).
5. Add a **trigger engine**: triggers produce episodes (find-or-create session,
   enqueue prompt), each with a capability/budget envelope and durable run
   records.

## Milestones

| Milestone | Issue | Depends on |
| --- | --- | --- |
| Session dir + pi/pi-web interop | VED-338 | — |
| RPC event parity | VED-339 | — |
| Unit host/supervisor | VED-340 | VED-338, VED-339 |
| HTTP + SSE gateway | VED-341 | VED-340 |
| pi-web fork adapter | VED-342 | VED-339 / VED-341 |
| Trigger engine | VED-343 | VED-340 |

## Consequences

- Milestone 1 (browse) is usable as soon as session-dir management lands.
- Milestone 2 (event parity) is the load-bearing change for a live UI; it
  requires exposing provider stream deltas through the agent loop.
- SDK-only panels (subagents, MCP, skills, plugins, project trust,
  exact system prompt, node-pty terminal) must be hidden or reimplemented.
- Unattended triggers must run under the capability/approval policy added in the
  sandbox fixes, with per-run budgets.

## Alternatives considered

- **Thin Rust UI instead of reusing pi-web.** Clean but rebuilds 115 components
  and the entire session/file/config surface.
- **A lossy adapter over the current native events.** Fast to write, but drops
  tool-call ids and streaming deltas, so the UI quality regresses.
- **A Node sidecar speaking pi's SDK.** Reintroduces the Node runtime the
  project exists to remove.
