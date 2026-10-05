# pipelets swarm status — 2026-10-05

Point-in-time snapshot of the **pipelets** (formerly `pi native runtime`) project
board. Source of truth is Linear project `4d5e47500fa6`, exported via
`linear issue mine --team VED --project 4d5e47500fa6 --all-states --json`.

## Board summary

- **89 issues total**
- **88 Done**
- **1 Canceled**
- **0 open**

The core is feature-complete: the native host, agent loop, tools, RPC/JSON
protocol, gateway, unit host, triggers, provider layer, session store, mailbox,
packaging, and the memory/CPU gates are all landed. Worktrees were dropped.

## What this snapshot covers

| Area | State |
| --- | --- |
| Bare core (loop, tools, session, providers, plugins) | Done |
| Unit host + HTTP/SSE gateway | Done |
| Trigger engine (schedules, budgets, receipts) | Done |
| Direct unit-to-unit mailbox (VED-379) | Done |
| Worktrees | Removed (`2cdeae7`) |
| Gateway host-lock fix (VED-389) | Done (`f2f8fbd`) |
| Rename `pi-native` → `pipelets` | Done |

## Gates at this snapshot

- `cargo fmt --all -- --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` → **352 passed / 0 failed**
- Release binary **8.6 MB**; idle RSS **4.4 MB** (`--rpc`) / **5.0 MB**
  (`--gateway`); idle CPU **~0.001 s / 3 s**

## The seven role units

Coordination is Linear-only; there is no shared chat. Scopes are disjoint.

| Unit | Owns | May edit | Read-only |
| --- | --- | --- | --- |
| **planner** | backlog, prioritisation | — | yes |
| **researcher** | `crates/pi-providers`, `crates/pi-cache`, `crates/pi-net` | — | yes (findings only) |
| **coder** | `crates/pi-gateway`, `crates/pi-host`, `crates/pi-triggers` | those crates | no |
| **reviewer** | `crates/pi-rpc`, `crates/pi-agent` | — | yes (review only) |
| **tester** | tests and gates | `crates/*/tests/*`, `scripts/*.py` | no |
| **synthesizer** | status, docs | `docs/*.md` only | no |
| **conductor** | planning + dispatch | — | no |

## Shared project memory (VED-371)

Units previously re-derived (or contradicted) decisions, constraints, and
lessons each run. `scripts/swarm_memory.py` is the shared, project-scoped store
for them: an append-only JSONL file at `.pi/swarm-memory.jsonl` (runtime data,
git-ignored) with short typed entries — `decision`, `constraint`, `lesson`,
`gotcha` — each timestamped and attributed to an author and issue.

- Record from any unit:
  `python3 scripts/swarm_memory.py record --kind decision --text "…" --author coder --issue VED-371`.
- Read it back: `python3 scripts/swarm_memory.py list` (newest first) or
  `context` (the prompt-ready `<shared_memory>` block).
- The conductor injects the memory block into every assignment and audit prompt,
  so a decision recorded by one unit is visible to the next unit and to the
  conductor's prompts. Injection is best-effort: a memory error never blocks a
  dispatch.
- Over MCP: `.pi/mcp.json` registers `swarm-memory`, exposing `memory_record`
  and `memory_list` to any harness. Run the stdio server directly with
  `python3 scripts/swarm_memory.py mcp`.
- Tests: `python3 scripts/test_swarm_memory.py`.

## Verification / provenance

- Board exported from Linear: `linear issue mine --team VED --project 4d5e47500fa6 --all-states --json`.
- No source files outside `docs/` were touched by the synthesizer.
