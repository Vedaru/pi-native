# pi-native swarm — unit guide

You are one **role unit** in a pi-native swarm. This file lives in the repo so
you can read it inside the workspace jail. The human talks to the **conductor**
unit; you receive work from the conductor and coordinate through Linear.

## The board is the shared channel

Team **VED**, project **pi native runtime** (`4d5e47500fa6`). Use the `linear`
CLI (credentials are already configured).

- List work: `linear issue mine --team VED --project 4d5e47500fa6 --all-states --json`
- Read before you work: `linear issue comment list <ISSUE>` — another unit may
  have left notes or partial work.
- Post updates / ask / hand off:
  `linear issue comment add <ISSUE> --body "..."` (prefer `--body-file`).
- Keep the issue state current: `linear issue update <ISSUE> --state "In Progress"`
  / `"Done"`.
- **Sign every comment with your role**, e.g. `- [coder]`.

## Roles and routing

| Unit | Owns | May edit code |
| --- | --- | --- |
| `planner` | backlog, prioritisation | no (analysis only) |
| `researcher` | `pi-providers`, `pi-cache`, `pi-net`, `pi-plugins` | yes |
| `coder` | `pi-gateway`, `pi-host`, `pi-triggers`, `pi-agent`, `pi-session`, `pi-rpc` | yes |
| `reviewer` | review of `pi-rpc` / `pi-agent` | no (analysis only) |
| `tester` | tests and gates | yes (tests only) |
| `synthesizer` | status, docs | docs only |
| `conductor` | planning + dispatch | no |

A **read-only** unit that receives a code card must bounce it back on the issue
with the reason instead of declining silently.

## Working on code

- Code cards run directly in the **shared main tree** — no worktrees. The
  orchestrator dispatches one editor at a time, and commits land on the current
  branch.
- **One editor at a time** — only one unit should run a heavy `cargo` build.
- Gates before Done: `cargo fmt --all`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`.
- **Do not push.** The synthesizer pushes.
- Scratch files: write to `/tmp` (an allowed write root); **never** leave temp
  files in the repo.

## Collision protocol (declare intent before editing)

Parallel runs must not silently edit the same files. Before writing, declare the
files and symbols you expect to touch:

```
python3 scripts/swarm_conductor.py --declare VED-375 --role coder \
    --session <sessionId> --scope crates/pi-rpc/src/lib.rs crates/pi-agent \
    --symbols SessionState::switch
```

The conductor checks the declaration against the other in-flight write intents.
An overlap blocks the **later** declaration, records the collision (and the
resolved paths/symbols) on **both** cards, and re-evaluates on every reconcile
pass: the card dispatches automatically once the blocker reaches Done/Canceled or
releases its intent (`--release VED-375`). Read-only ops (`review`/`test`) never
block. An unknown/empty scope is fail-safe: it warns and requires explicit
resolution instead of passing as disjoint. `--force` overrides a block but the
override is recorded on both cards — it is never silent. The pure rules live in
`scripts/swarm_collision.py` (`scripts/test_swarm_collision.py`).

## Direct unit-to-unit messaging (mailbox + escrow)

Linear comments are the durable work board, but they are high-latency and not
machine-addressed. Units coordinate over an in-memory **mailbox** in
`crates/pi-host`, surfaced by additive gateway routes:

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/units/:id/messages?after=<seq>` | Messages enqueued to a unit, plus `nextSeq` |
| `POST` | `/units/:id/messages` | Send a direct message |
| `POST` | `/units/:id/messages/:msgid/ack` | Acknowledge a message |
| `POST` | `/units/:id/ownership` | Transfer ownership of a unit |

A message is an envelope (`id`, `from`, `to`, `kind`, `issue`, `corrId`, `body`,
`state`, `ownerBefore/After`, `seq`) shared verbatim by the mailbox, the
`swarm_message` session entry, and the Linear escrow comment. `kind` is one of
`request|reply|handoff|ownership|ack`.

- **Send + ack.** `POST /units/:id/messages` returns `202` with the accepted
  envelope; the recipient polls (`after=0`) or consumes the existing SSE stream.
  `POST .../ack` marks the original `acked` and delivers an `ack` envelope to
  the sender, which observes delivery by `corrId` — no reply scraping.
- **FIFO + resume.** Messages carry the host's monotonic `seq`, so
  `after=<seq>` is exact (strictly newer, no gaps or duplicates) and `nextSeq`
  is the cursor to persist across a reconnect.
- **Backpressure.** The mailbox is bounded; a full mailbox returns `429` to the
  **sender** (`HostError::MailboxFull`) — never a silent drop. Acking frees a
  slot. An oversized body is `413`.
- **Ownership.** `transfer_ownership` records `ownerBefore`/`ownerAfter`, sets
  the owner, and mirrors it into the session header `unit`. Only the current
  owner may transfer (`409` otherwise).
- **Non-pollution.** `swarm_message`/`swarm_ownership` entries are durable but
  are excluded from `build_context`, so transport never reaches the model.
- **Escrow.** `handoff`/`ownership`/terminal-`ack` messages are mirrored by
  `scripts/swarm_mail.py` as signed Linear comments (a fenced `swarm-envelope`
  block); a Linear failure is surfaced, never swallowed. See
  `docs/adr/0004-swarm-mailbox.md`.

## Patterns

`python3 scripts/swarm_run.py --pattern concurrent|sequential|moa --task "..."` runs
an orchestration across units (the conductor uses this).
