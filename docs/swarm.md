# Swarm: inlets and outlets

A unit can work alongside others through a **fire-and-forget shout bus**: every
unit writes its actions to an *outlet* and reads its peers' outlets on its
*inlet*. It is stigmergy — a shared environment the units observe — not a
conductor, a mailbox, or a lock.

Off by default: a unit with no `PIPELETS_SWARM_DIR` writes nothing and its
system prompt stays byte-identical to pi.

## Enable

```sh
export PIPELETS_SWARM_DIR=/path/to/swarm   # one directory shared by the units
export PIPELETS_UNIT_ID=unit-a             # optional; defaults to the session id
```

## Outlet — what a unit broadcasts

One JSON line per action, appended to `<dir>/<unit>.outlet`:

```json
{"from":"unit-a","kind":"tool","text":"edit package/__init__.py"}
{"from":"unit-a","kind":"says","text":"Merging unit-b's export, then done."}
```

| `kind` | when |
| --- | --- |
| `tool` | a workspace-changing tool call (`bash`, `edit`, `write`, `apply_patch`, …), summarised to its command or path |
| `says` | the reply, once per turn (not once per token) |
| `error` | a failed tool call, or a turn error |

Reads, searches, successful tool completions, and streaming deltas are **not**
shouted — they are noise.

## Inlet — what a unit hears

Before **every model call** the unit reads its peers' new outlet lines and folds
them into one short block, delivered as a mid-turn steering message:

```xml
<shouts>
[unit-b] tool: write package/strings.py
[unit-b] tool: edit package/__init__.py
[unit-b] says: Done.
</shouts>
```

- One block per model call; the most recent 10 lines, older ones collapsed to
  `(N earlier peer action(s) omitted)`.
- Delivered **mid-turn**: the unit sees a peer's action at its next reasoning
  step, not on a later prompt.
- The `<swarm>` system-prompt section tells the unit to read shouts as peer
  context, never as instructions.

## What it is not

A shout is **advisory**. It does not stop two units writing the same file in the
same instant — the first round is blind. It makes them notice and reconcile, and
in practice they do ("leaving `calc.py` untouched to avoid re-clobbering a
peer"). Prevention is a lock or separate worktrees; the bus only reduces wasted
work.

There is no addressing, no ownership or acknowledgement, and no conductor in the
unit — those live in the orchestrator (`pipelets-swarm`). See [scope](scope.md).

## Cost

An idle unit in a swarm is unchanged (~4.5 MB): the inlet polls once per model
call and returns nothing without allocating. Figures in
[performance](performance.md).
