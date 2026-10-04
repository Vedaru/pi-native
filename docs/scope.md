# Scope: pi's minimal core

This project is a **partial rewrite of pi's core plus a plugin wrapper** — not a
full port, and not a port of the reference Rust project's extended feature set.
pi describes itself as "a minimal, extensible agent harness"; this document is
the canonical in/out list and the guard for new work.

The reference port (`Dicklesworthstone/pi_agent_rust`) is used for
**architecture** (QuickJS + Rust-backed Node shims, crate choices), never for
scope.

## In scope

| Surface | Notes |
| --- | --- |
| Tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls` | plus powershell on Windows |
| Modes: interactive TUI, print, JSON, RPC | |
| Sessions: JSONL with branching; context building; compaction | `pi-session` |
| Providers and streaming | OpenAI Responses/Completions — `pi-providers`, `pi-net` |
| Extensions, skills, prompt templates, themes, packages | the plugin wrapper (`pi-plugins`) |
| MCP and codemode | as pi ships them |
| Image resize/convert | `pi-image` |
| Syntax highlighting | `pi-highlight` |
| JSON revision diffing | `pi-delta` |
| Prompt-cache primitives | `pi-cache` |

## Out of scope (reference-port bloat)

swarm, beads, LSP, browser, computer, web UI, sub-agents, plan mode, memory
bank, worktrees, and any other feature pi's core does not ship.

## Hard constraints

- **Provider-side identity** (VED-315): the process must be indistinguishable
  from pi at the provider API, including prompt-cache behavior. A regression
  blocks merge and release.
- **Plugin compatibility**: existing pi plugins load and run unchanged through
  the wrapper (VED-304). Plugin API compatibility wins over native-core
  convenience when the two conflict.
- **Generalize, do not special-case** (ADR 0001): one mechanism covering many
  cases, mapping tables over branches, provider wire formats as the only
  legitimate format-specific exception.

## Issue map

| Issue | Core surface |
| --- | --- |
| VED-302/316/317 | memory benchmark |
| VED-303/319 | architecture decisions (host, TUI) |
| VED-304 | plugin wrapper (extensions) |
| VED-305 | providers and streaming |
| VED-306 | sessions and context |
| VED-307 | TUI (native renderer + JS component bridge) |
| VED-308 | images |
| VED-309 | syntax highlighting |
| VED-310 | delta / replicated state |
| VED-311 | packaging (single binary, embedded assets) |
| VED-312 | interim lazy imports in pi (not part of the native runtime) |
| VED-313/314/315 | provider parity and the release gate |
| VED-318 | this scope definition |
