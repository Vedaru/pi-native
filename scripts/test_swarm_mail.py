#!/usr/bin/env python3
"""Tests for the durable unit-to-unit mail escrow (VED-379).

Covers the escrow decision (which kinds are recorded), the signed comment
shape, and — the point of the card — that a Linear failure is surfaced and does
NOT block the mailbox write.

Run with `python3 scripts/test_swarm_mail.py`. Stdlib only; Linear is injected,
so no CLI or network call is made.
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_mail():
    spec = importlib.util.spec_from_file_location(
        "swarm_mail", os.path.join(HERE, "swarm_mail.py")
    )
    module = importlib.util.module_from_spec(spec)
    # Register before exec so `dataclass` field resolution can find the module
    # (Python 3.14 resolves annotations via `sys.modules`).
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


mail = load_mail()


def envelope(**overrides):
    base = dict(
        id="VED379-3",
        from_unit="coder",
        to="reviewer",
        kind="handoff",
        body="take over the mailbox",
        created_at=1_730_000_000,
        issue="VED-379",
        seq=3,
    )
    base.update(overrides)
    return mail.Envelope(**base)


class EscrowTests(unittest.TestCase):
    def test_handoff_posts_a_signed_comment_with_the_envelope(self):
        # P1: a handoff lands as a signed Linear comment carrying the envelope.
        posted = []

        def fake_post(issue, body):
            posted.append((issue, body))
            return True, "ok"

        result = mail.escrow(envelope(kind="handoff"), "coder", post=fake_post)
        self.assertTrue(result.posted)
        self.assertEqual(result.reason, "")
        self.assertEqual(len(posted), 1)
        issue, body = posted[0]
        self.assertEqual(issue, "VED-379")
        self.assertIn("```swarm-envelope", body)
        self.assertIn("- [coder]", body, "the escrow record is signed")
        # The fenced block round-trips to the same envelope.
        block = body.split("```swarm-envelope", 1)[1].split("```", 1)[0]
        parsed = json.loads(block)
        self.assertEqual(parsed["id"], "VED379-3")
        self.assertEqual(parsed["kind"], "handoff")
        self.assertEqual(parsed["corrId"], None)

    def test_ownership_records_before_and_after(self):
        # AC8's record carries the ownership audit trail.
        seen = {}

        def fake_post(issue, body):
            seen["body"] = body
            return True, "ok"

        owned = envelope(
            kind="ownership", owner_before="coder", owner_after="reviewer"
        )
        result = mail.escrow(owned, "coder", post=fake_post)
        self.assertTrue(result.posted)
        self.assertIn('"ownerBefore": "coder"', seen["body"])
        self.assertIn('"ownerAfter": "reviewer"', seen["body"])

    def test_a_linear_failure_is_surfaced_not_swallowed(self):
        # P2/AC9: escrow reports the failure and does not raise.
        def failing_post(issue, body):
            return False, "rate limited"

        result = mail.escrow(envelope(kind="ownership"), "coder", post=failing_post)
        self.assertFalse(result.posted)
        self.assertEqual(result.reason, "rate limited")
        self.assertEqual(result.envelope_id, "VED379-3")

    def test_a_cli_crash_is_surfaced_too(self):
        def exploding_post(issue, body):
            raise OSError("no linear binary")

        with self.assertRaises(OSError):
            # The injectable poster's own exceptions propagate; the default
            # poster catches OSError and returns (False, reason) instead.
            mail.escrow(envelope(), "coder", post=exploding_post)
        # The default poster turns the same failure into a result.
        original = mail.subprocess.run

        def boom(*_args, **_kwargs):
            raise OSError("no linear binary")

        mail.subprocess.run = boom
        try:
            result = mail.escrow(envelope(kind="handoff"), "coder")
        finally:
            mail.subprocess.run = original
        self.assertFalse(result.posted)
        self.assertIn("unavailable", result.reason)

    def test_plain_requests_are_not_escrowed(self):
        # Only handoff/ownership/ack persist; a request is transport-only.
        result = mail.escrow(envelope(kind="request"), "coder", post=lambda i, b: (True, ""))
        self.assertFalse(result.posted)
        self.assertIn("not escrowed", result.reason)

    def test_force_escrows_any_kind(self):
        result = mail.escrow(
            envelope(kind="request"), "coder", post=lambda i, b: (True, ""), force=True
        )
        self.assertTrue(result.posted)

    def test_an_envelope_without_an_issue_cannot_be_escrowed(self):
        result = mail.escrow(envelope(kind="handoff", issue=None), "coder")
        self.assertFalse(result.posted)
        self.assertIn("no issue", result.reason)

    def test_envelope_json_uses_the_shared_camelcase_shape(self):
        # The escrow block, the mailbox, and the session entry share one shape.
        data = envelope(kind="ownership", owner_after="reviewer").to_json()
        for key in ("id", "from", "to", "kind", "corrId", "createdAt", "ownerAfter", "seq"):
            self.assertIn(key, data)
        self.assertNotIn("from_unit", data)
        # And it round-trips back.
        back = mail.envelope_from_json(data)
        self.assertEqual(back.owner_after, "reviewer")
        self.assertEqual(back.from_unit, "coder")


class CorrelationTests(unittest.TestCase):
    def test_ack_correlates_to_the_original_by_corr_id(self):
        # AC3: the escrowed ack names the request it acknowledges.
        ack = envelope(kind="ack", id="reviewer-ack-3", corr_id="VED379-3", issue="VED-379")
        body = mail.escrow_body(ack, "reviewer")
        block = body.split("```swarm-envelope", 1)[1].split("```", 1)[0]
        parsed = json.loads(block)
        self.assertEqual(parsed["corrId"], "VED379-3")
        self.assertIn("Acknowledgement of VED379-3", body)


if __name__ == "__main__":
    unittest.main()
