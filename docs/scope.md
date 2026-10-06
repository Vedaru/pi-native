# Scope: the pipelets bare core

pipelets is a **swarm-friendly, low-memory, low-CPU agent bare core**: a partial
rewrite of pi's core plus a plugin wrapper. It is not a full port, and not a port
of the reference Rust project's extended feature set. pi describes itself as "a
minimal, extensible agent harness"; this document is the canonical in/out list
and the guard for new work.

The binary is the **bare core** a single unit needs. It ships one peer-awareness
primitive — the fire-and-forget **shout bus** ([swarm](swarm.md): inlets and
outlets, off unless `PIPELETS_SWARM_DIR` is set). Swarm *orchestration* — a
conductor, role units, a Linear board — lives in a separate repo
(`pipelets-swarm`), driving the core over the single-unit serve. It is never part
of the shipped binary, so an idle unit stays at ~4–5 MB.

The reference port (`Dicklesworthstone/pi_agent_rust`) is used for
**architecture** (QuickJS + Rust-backed Node shims, crate choices), never for
scope.

## In scope

| Surface | Notes |
| --- | --- |
| Tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls` | plus powershell on Windows |
| Modes: print, JSON, RPC (headless; no TUI) | |
| Sessions: JSONL with branching; context building; compaction | `pi-session` |
| Providers and streaming | OpenAI Responses/Completions — `pi-providers`, `pi-net` |
| Extensions, skills, prompt templates, themes, packages | the plugin wrapper (`pi-plugins`) |
| MCP and codemode | as pi ships them |
| Image decode/resize for `read` | `pi-image` |
| Prompt-cache primitives | `pi-cache` |

Core primitives a swarm host drives:

| Surface | Notes |
| --- | --- |
| Unit host: one addressable agent, attach/detach, event fan-out, idle suspend | `pi-host` |
| HTTP + SSE serve (one unit) | `pi-gateway` |

Coordination between units (handoff, ack, ownership) lives in the orchestrator,
not in the unit: pipelets ships no addressed mailbox, no ownership, and no
`/units/*` route (VED-421). The shout bus is awareness only — one shared
bulletin, no addressees.

## Out of scope (reference-port bloat)

beads, LSP, browser, computer, sub-agents, plan mode, memory bank, worktrees,
and any other feature pi's core does not ship. Swarm orchestration (the
conductor, DAGs, dashboards, Linear) is a separate repo, never in the unit.

## Hard constraints

- **Provider-side identity** (VED-315): the process must be indistinguishable
  from pi at the provider API, including prompt-cache behavior. A regression
  blocks merge and release.
- **Plugin compatibility**: existing pi plugins load and run unchanged through
  the wrapper (VED-304). Plugin API compatibility wins over native-core
  convenience when the two conflict.
- **Low memory and low CPU**: an idle unit stays near the Rust floor (~4–5 MB,
  ~0 CPU). New core features must not add unbounded buffers or busy-waits; the
  memory and pressure gates fail CI on a regression.
- **Generalize, do not special-case** (ADR 0001): one mechanism covering many
  cases, mapping tables over branches, provider wire formats as the only
  legitimate format-specific exception.

## Issue map

| Issue | Core surface |
| --- | --- |
| VED-302/316/317 | memory benchmark |
| VED-303/319 | architecture decisions (host; TUI dropped) |
| VED-304 | plugin wrapper (extensions) |
| VED-305 | providers and streaming |
| VED-306 | sessions and context |
| VED-307 | TUI — dropped; a swarm unit is headless |
| VED-308 | images — `read` attaches resized images (agent input) |
| VED-309 | syntax highlighting — dropped (display-only) |
| VED-310 | delta / replicated state — dropped (unused) |
| VED-311 | packaging (single binary, embedded assets) |
| VED-312 | interim lazy imports in pi (not part of the native runtime) |
| VED-313/314/315 | provider parity and the release gate |
| VED-318 | this scope definition |
| VED-337–343 | web/host infra |
| VED-379 | direct unit-to-unit mailbox (removed in VED-421; rig owns coordination) |
| VED-419 | drop the trigger engine; rig owns scheduling |
| VED-420 | single-unit serve; keep pi-web compatible |
| VED-421 | remove the mailbox and `/units` routes (rig owns coordination) |
