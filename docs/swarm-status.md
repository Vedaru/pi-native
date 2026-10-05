# Swarm status — 2026-10-05

Point-in-time snapshot of the `pi native runtime` project board, written by the
**synthesizer** role unit. Source of truth is Linear project
`4d5e47500fa6` (`pi native runtime: Rust memory-heavy rewrite, drop Node`),
exported via `linear issue mine --team VED --project 4d5e47500fa6 --all-states`.

## Board summary

- **54 issues total**
- **48 Done**
- **3 In Progress**
- **1 Todo**
- **1 Backlog**
- **1 Canceled**

The rewrite is essentially feature-complete: the native host, agent loop, tools,
RPC/JSON protocol, gateway, unit host, triggers, provider layer, session store,
packaging, and the memory/CPU gates are all landed. What remains is a small
tail of **swarm-audit follow-ups** (found while reviewing the landed crates)
plus one deferred port item.

## Open issues

| Issue | State | Priority | Title |
| --- | --- | --- | --- |
| VED-348 | In Progress | High | Swarm audit: RPC tool-result shape + stale session name on switch |
| VED-350 | In Progress | High | Swarm audit: plugin randomBytes DoS, swallowed fs errors, path-denial signaling |
| VED-356 | In Progress | No priority | Make a unit removable: `Host::remove` + `DELETE /sessions/:id` |
| VED-347 | Todo | High | Swarm audit: session compaction is never recorded, persist can diverge |
| VED-307 | Backlog | Medium | Port TUI renderer and terminal core to Rust |

### Notes on open issues

- **VED-348** — RPC role fixed the two high-severity items
  (`tool_execution_end` content blocks; stale `name` cleared on `switch`).
  Remaining: `start_new` ignores `parentSession`, and `entries()` re-reads and
  re-serializes the whole session file on every call.
- **VED-350** — plugin runtime: randomBytes DoS, swallowed fs errors, and
  path-denial signaling.
- **VED-356** — follow-up to VED-355 (idle eviction); units are suspended but
  never removed, so the host map grows with session count. Adds
  `Host::remove` + `DELETE /sessions/:id`, with tests.
- **VED-347** — session compaction is never recorded; persisted state can
  diverge from in-memory state.
- **VED-307** — deferred: port the TUI renderer / terminal core. Sits in
  Backlog (VED-319 decided against reusing pi's TS TUI; VED-310 chord tracker
  is already Done).

## The six role units

Coordination is Linear-only; there is no shared chat. Scopes are disjoint.

| Unit | Owns | May edit | Read-only |
| --- | --- | --- | --- |
| **planner** | backlog, prioritisation | — | yes |
| **researcher** | `crates/pi-providers`, `crates/pi-cache`, `crates/pi-net` | — | yes (findings only) |
| **coder** | `crates/pi-gateway`, `crates/pi-host`, `crates/pi-triggers` | those crates | no |
| **reviewer** | `crates/pi-rpc`, `crates/pi-agent` | — | yes (review only) |
| **tester** | tests and gates | `crates/*/tests/*`, `scripts/*.py` | no |
| **synthesizer** | status, docs | `docs/*.md` only | no |

## Recent audit trail

The swarm-audit batch (VED-346 — VED-355) produced ten issues across the
landed crates; nine are Done and only VED-347 and VED-348 remain open. VED-355
(gateway idle eviction) immediately spawned VED-356 (full unit removal), which
is the current in-flight coder workstream.

## Verification / provenance

- Board exported from Linear: `linear issue mine --team VED --project 4d5e47500fa6 --all-states --limit 100 --json`.
- No source files outside `docs/` were touched. Changes are left in the working
  tree; nothing is committed.
