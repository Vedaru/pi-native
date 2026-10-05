# pi-web integration

This directory holds the code needed to point a fork of
[`agegr/pi-web`](https://github.com/agegr/pi-web) at pi-native unit sessions
instead of pi's embedded TypeScript SDK ([VED-342](https://linear.app/vedaru/issue/VED-342)).

## Why a fork

pi-web embeds `@earendil-works/pi-coding-agent` in-process. `lib/rpc-manager.ts`
constructs the SDK `AgentSession` and calls it directly; the browser talks to
pi-web's own `/api/*` routes. The clean seam is
`AgentSessionWrapper.inner: AgentSessionLike` (`lib/pi-types.ts`). Everything
above it (the UI, SSE plumbing, session viewer, file browser, most routes) can
stay.

## What already works with no fork

Because pi-native now writes pi's session layout
(`sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`, `type: "session"`,
version 3), pi-web's session scanner can already **list, open, and render**
pi-native sessions. The gateway (`pi-native --gateway`) provides the live half:
events over SSE and commands over HTTP.

## Adapter

`lib/pi-native-session.ts` implements the subset of `AgentSessionLike` the chat
surface uses, backed by the gateway:

- `subscribe(listener)` opens `GET /sessions/:id/events?format=pi` and feeds pi's
  canonical events (`agent_start`, `message_update`, `tool_execution_*`,
  `agent_settled`) straight to the listener — the same shapes pi-web's
  `toClientAgentEvent` already consumes.
- `prompt` / `steer` / `followUp` / `abort` / `compact` / `executeBash` /
  `setModel` / `setThinkingLevel` / `setSessionName` map to
  `POST /sessions/:id/commands`.
- `ui_request` events are answered with `POST /sessions/:id/ui_response`.

## Fork steps

1. Fork and check out `agegr/pi-web`.
2. Copy `lib/pi-native-session.ts` into the fork's `lib/`.
3. In `lib/rpc-manager.ts`'s `startRpcSession`, replace the
   `createAgentSessionFromServices(...)` construction with
   `new PiNativeSession({ baseUrl, sessionId })` and return an
   `AgentSessionWrapper` around it. The wrapper's SDK-specific helpers
   (`extensionRunner`, `subagent`, `mcpHost`, `exactSystemPrompt`) do not apply
   to a native session and should be skipped for it.
4. Keep `lib/session-reader.ts` (it already reads native session files), the file
   browser, git, and theme routes.
5. Hide or reimplement the SDK-only panels until the native core supports them:
   sub-agents, MCP, plugins, skills, worktrees, project trust, and the node-pty
   terminal.

## Remaining shims

`AgentSessionLike` also exposes `sessionManager`, `settingsManager`,
`modelRuntime`, `extensionRunner`, `promptTemplates`, and `resourceLoader`.
These are read by panels outside the chat surface. Either back them with small
local readers (session file + pi config) or remove the panels that need them.
