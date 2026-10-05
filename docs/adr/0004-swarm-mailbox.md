# ADR 0004: Durable unit-to-unit messaging (mailbox + Linear escrow)

- Status: Proposed
- Date: 2026-10-05
- Issues: VED-379 (this)
- Deciders: pipelets maintainers

## Context

Coordination between swarm units rides on Linear comments today. The conductor
addresses a unit by session id and `POST /sessions/:id/commands` with a prompt;
delivery is a fire-and-forget HTTP 202 with **no acknowledgement and no message
identity**. Replies are scraped by polling `/swarm` and reading the recipient's
last assistant text block, with correlation implicit in file order. There is no
recorded ownership, no per-recipient FIFO, and no sender-visible backpressure.

`crates/pi-host` already provides the substrate: a per-unit command channel, a
bounded replay ring, live subscriber fan-out, and a monotonic event `seq` used
by SSE `id:`/`Last-Event-ID` resume. `crates/pi-session` persists sessions as
append-only JSONL, and ADR 0001/0003 make the session file the source of truth
with **one writer per session**.

## Decision

Add a durable, acknowledged **direct-message mailbox** to `pi-host`, surfaced by
additive gateway routes, and mirror the durable-record kinds (`handoff`,
`ownership`, terminal `ack`) to Linear as a signed **escrow** comment.

### Envelope

One envelope shape is shared by the mailbox, the session entry, and the Linear
escrow block, so a message is the same object at every hop:

```jsonc
{ "id": "coder-3", "from": "coder", "to": "<recipient session id>",
  "kind": "request|reply|handoff|ownership|ack", "issue": "VED-379",
  "corrId": "coder-1", "body": "…", "createdAt": 1730000000,
  "state": "sent|acked|failed", "ackedAt": null,
  "ownerBefore": null, "ownerAfter": null, "seq": 3 }
```

- **Ordering.** The recipient's mailbox is stamped with the unit's existing
  monotonic `Shared.seq`, so `after=<seq>` is an exact resume cursor and a live
  SSE subscriber and a polling client observe the same order.
- **Ack.** `ack_message` marks the original `acked` (with `ackedAt`), frees a
  mailbox slot, and delivers an `ack` envelope (`corrId` = the original id) to
  the sender's mailbox. The sender observes delivery rather than scraping.
- **Backpressure.** The mailbox is bounded at `MAILBOX_LIMIT`; over the cap
  `send_message` returns `HostError::MailboxFull`, which the gateway surfaces as
  `429`. This is deliberately the opposite of the SSE subscriber drop-on-lag: a
  message is never silently discarded.
- **Ownership.** `transfer_ownership` records `ownerBefore`/`ownerAfter` on an
  `ownership` envelope, sets the unit's in-memory owner, and mirrors the new
  owner into the session header `unit` field. Only the current owner may
  transfer; a non-owner attempt is `HostError::NotOwner` (`409`). The owner unit
  is the **single writer** of its own header; the mailbox is not a second
  journal writer.

### Non-pollution

Message and ownership records are persisted as `swarm_message` /
`swarm_ownership` session entries for durability, but `build_context` maps only
known entry kinds (`message`, `compaction`, `branch_summary`,
`custom_message`), so transport never enters the model transcript. This is
pinned by a `pi-session` test.

### Linear escrow

The mailbox is in-memory, so a handoff or ownership transfer must survive a
process restart. Any `handoff`/`ownership` message and any terminal `ack` is
posted as a signed Linear comment (`scripts/swarm_mail.py`) carrying the
envelope as a fenced JSON block. A Linear failure is **surfaced, never
swallowed**, and never blocks the mailbox write.

### Routes (additive)

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/units/:id/messages?after=<seq>` | Messages enqueued to a unit, plus `nextSeq` |
| `POST` | `/units/:id/messages` | Send a direct message (`202`; `429` full; `413` oversize; `404` unknown) |
| `POST` | `/units/:id/messages/:msgid/ack` | Acknowledge a message |
| `POST` | `/units/:id/ownership` | Transfer ownership (`200`; `409` non-owner; `404` unknown target) |

Existing `/sessions/*`, `/swarm`, and RPC commands are unchanged; protocol
version stays `1`.

## Consequences

- Units can message, poll, and acknowledge each other with explicit correlation
  and a persistable cursor, replacing implicit reply scraping.
- Ownership is a recorded, single-writer event mirrored into the durable header.
- The interactive transport (host mailbox) and the durable record (Linear
  escrow) are separate, so a wiped `/tmp` or a restarted process does not lose
  a handoff.
- Follow-up (not this ADR): the conductor and `swarm_run` switch their dispatch
  and reply paths to `send_message`/`ack`, and the RPC command surface gains
  matching variants if a unit must drive the mailbox over `POST /commands`.
