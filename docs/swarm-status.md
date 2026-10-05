# Swarm status — 2026-10-05

Point-in-time snapshot of the `pi native runtime` project board, written by the
**synthesizer** role unit. Source of truth is Linear project
`4d5e47500fa6` (`pi native runtime: Rust memory-heavy rewrite, drop Node`),
exported via `linear issue mine --team VED --project 4d5e47500fa6 --all-states`.

## Board summary

- **58 issues total**
- **56 Done**
- **1 In Progress**
- **1 Canceled**

The rewrite is feature-complete: the native host, agent loop, tools, RPC/JSON
protocol, gateway, unit host, triggers, provider layer, session store,
packaging, and the memory/CPU gates are all landed. The swarm-audit follow-up
batch (VED-346 — VED-359) has been resolved. The only remaining workstream is
the deferred TUI port.

## Open issues

| Issue | State | Priority | Title |
| --- | --- | --- | --- |
| VED-307 | In Progress | Medium | Port TUI renderer and terminal core to Rust |

### Notes on the open issue

- **VED-307** — port the TUI renderer / terminal core. Sits in milestone
  `M4 - Native host: agent loop, tools, headless protocol`. VED-319 decided
  against reusing pi's original TS TUI; VED-310 (chord delta tracker) is Done.

## Recently closed (this cycle)

The whole audit tail landed: VED-347, VED-348, VED-350, VED-355, VED-356,
VED-358, VED-359. VED-360 (a lifecycle/routing fix for conductor → reviewer
handling of `rpc` issues) is also Done. VED-357 (this status note) is Done and
was committed as `0bd6ad7`.

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

## Shared project memory (VED-371)

Units previously re-derived (or contradicted) decisions, constraints, and
lessons each run. `scripts/swarm_memory.py` is now the shared, project-scoped
store for them: an append-only JSONL file at `.pi/swarm-memory.jsonl` (runtime
data, git-ignored) with short typed entries — `decision`, `constraint`,
`lesson`, `gotcha` — each timestamped and attributed to an author and issue.

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

## Commits landed since the last snapshot

- `e6326f0` — feat(agent): wire the session thinking level to the request and
  make the tool loop unbounded
- `0bd6ad7` — docs(swarm): add the swarm status note (`docs/swarm-status.md`)

Pushed to `origin/master` as `1689197..0bd6ad7`. Gates at that point:
`cargo fmt --all` clean, `cargo clippy --workspace --all-targets -- -D warnings`
clean, `cargo test --workspace` → 228 passed / 0 failed.

## Verification / provenance

- Board exported from Linear: `linear issue mine --team VED --project 4d5e47500fa6 --all-states --limit 100 --json`.
- No source files outside `docs/` were touched by the synthesizer.
