#!/usr/bin/env python3
"""Durable unit-to-unit mail escrow (VED-379, design C).

The interactive transport is the pi-host mailbox (`crates/pi-host`): a bounded,
sequenced, acknowledged per-unit queue surfaced by the gateway at
`/units/:id/messages`. This module is the *escrow* layer under it. The mailbox
is in-memory, so a handoff or ownership transfer that only exists in the mailbox
would vanish with the process. Any `handoff`/`ownership` message and any
terminal `ack` is therefore mirrored here as a signed Linear comment carrying the
envelope as a fenced JSON block.

Design rules (from the researcher/planner write-ups on VED-379):

* Linear is the durable, ordered, human-visible, already-acked audit layer.
* A Linear failure is **surfaced, never swallowed**, and must **not block** the
  mailbox write: `escrow()` returns a result and the caller records it.
* Envelopes are identical in the mailbox, the session entry, and the Linear
  block, so a message is the same object at every hop.
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import asdict, dataclass
from typing import Any, Callable

# Kinds that must be escrowed: a transfer of work or ownership outlives the
# process, so it needs the durable signed record.
ESCROW_KINDS = {"handoff", "ownership", "ack"}

ENVELOPE_FENCE = "swarm-envelope"


@dataclass
class Envelope:
    """One direct message, matching `pi_host::Envelope`.

    Field names are camelCase to match the Rust `serde(rename_all =
    "camelCase")` shape so the JSON round-trips between the two.
    """

    id: str
    from_unit: str
    to: str
    kind: str
    body: str
    created_at: int
    state: str = "sent"
    issue: str | None = None
    corr_id: str | None = None
    acked_at: int | None = None
    owner_before: str | None = None
    owner_after: str | None = None
    seq: int = 0

    def to_json(self) -> dict[str, Any]:
        """The camelCase wire shape shared with the host and the escrow block."""
        return {
            "id": self.id,
            "from": self.from_unit,
            "to": self.to,
            "kind": self.kind,
            "issue": self.issue,
            "corrId": self.corr_id,
            "body": self.body,
            "createdAt": self.created_at,
            "state": self.state,
            "ackedAt": self.acked_at,
            "ownerBefore": self.owner_before,
            "ownerAfter": self.owner_after,
            "seq": self.seq,
        }


@dataclass
class EscrowResult:
    """Outcome of one escrow attempt; a failure is reported, not raised."""

    posted: bool
    reason: str = ""
    envelope_id: str = ""
    comment: str = ""


def escrow_body(envelope: Envelope, role: str) -> str:
    """The signed Linear comment body for an envelope.

    The envelope is a fenced JSON block so a human reads the transfer and a
    machine can re-parse it, plus the role signature the swarm uses.
    """
    payload = json.dumps(envelope.to_json(), indent=2, sort_keys=True)
    header = {
        "handoff": f"Handoff of {envelope.issue or envelope.to}",
        "ownership": f"Ownership transfer of {envelope.issue or envelope.to}",
        "ack": f"Acknowledgement of {envelope.corr_id or envelope.id}",
    }.get(envelope.kind, f"Message {envelope.id}")
    return (
        f"**{header}**\n\n"
        f"```{ENVELOPE_FENCE}\n{payload}\n```\n\n"
        f"- [{role}]"
    )


def escrow(
    envelope: Envelope,
    role: str,
    *,
    post: Callable[[str, str], tuple[bool, str]] | None = None,
    force: bool = False,
) -> EscrowResult:
    """Mirror `envelope` to the durable Linear record.

    Only [`ESCROW_KINDS`] are escrowed unless `force` is set. `post` is an
    injectable `(issue, body) -> (ok, detail)` so tests never touch Linear; the
    default shells out to `linear issue comment add`.

    Returns an [`EscrowResult`]; a Linear failure sets `posted=False` and a
    reason. It never raises and never blocks the caller's mailbox write.
    """
    if envelope.kind not in ESCROW_KINDS and not force:
        return EscrowResult(
            posted=False,
            reason=f"kind {envelope.kind!r} is not escrowed",
            envelope_id=envelope.id,
        )
    issue = envelope.issue
    if not issue:
        return EscrowResult(
            posted=False,
            reason="envelope has no issue to escrow on",
            envelope_id=envelope.id,
        )
    body = escrow_body(envelope, role)
    poster = post or _linear_comment
    ok, detail = poster(issue, body)
    return EscrowResult(
        posted=bool(ok),
        reason="" if ok else detail,
        envelope_id=envelope.id,
        comment=body if ok else "",
    )


def _linear_comment(issue: str, body: str) -> tuple[bool, str]:
    """Post a comment via the Linear CLI. Never raises; returns (ok, detail)."""
    try:
        result = subprocess.run(
            ["linear", "issue", "comment", "add", issue, "--body", body],
            capture_output=True,
            text=True,
            env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
        )
    except OSError as error:
        return False, f"linear CLI unavailable: {error}"
    if result.returncode != 0:
        return False, result.stderr.strip() or "linear comment failed"
    return True, result.stdout.strip()


def main(argv: list[str] | None = None) -> int:
    """CLI: `swarm_mail.py escrow <json-envelope> --role <role> [--force]`.

    Reads the envelope as JSON so the conductor can pipe a mailbox record
    straight in. Exits non-zero when the escrow fails, so a caller that needs
    the durable record can react (the mailbox write has already happened).
    """
    import argparse

    parser = argparse.ArgumentParser(description="swarm mail escrow (VED-379)")
    sub = parser.add_subparsers(dest="command", required=True)
    escrow_cmd = sub.add_parser("escrow", help="post an envelope to Linear")
    escrow_cmd.add_argument("envelope", help="envelope JSON (or '-')")
    escrow_cmd.add_argument("--role", default="coder")
    escrow_cmd.add_argument("--force", action="store_true")
    args = parser.parse_args(argv)

    raw = args.envelope
    if raw == "-":
        import sys

        raw = sys.stdin.read()
    data = json.loads(raw)
    envelope = envelope_from_json(data)
    result = escrow(envelope, args.role, force=args.force)
    print(json.dumps(asdict(result)))
    return 0 if result.posted else 1


def envelope_from_json(data: dict[str, Any]) -> Envelope:
    """Build an [`Envelope`] from the camelCase wire shape."""
    return Envelope(
        id=data.get("id", ""),
        from_unit=data.get("from", data.get("from_unit", "")),
        to=data.get("to", ""),
        kind=data.get("kind", "request"),
        body=data.get("body", ""),
        created_at=int(data.get("createdAt", data.get("created_at", 0))),
        state=data.get("state", "sent"),
        issue=data.get("issue"),
        corr_id=data.get("corrId", data.get("corr_id")),
        acked_at=data.get("ackedAt", data.get("acked_at")),
        owner_before=data.get("ownerBefore", data.get("owner_before")),
        owner_after=data.get("ownerAfter", data.get("owner_after")),
        seq=int(data.get("seq", 0)),
    )


if __name__ == "__main__":
    raise SystemExit(main())
