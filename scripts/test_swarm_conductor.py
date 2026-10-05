#!/usr/bin/env python3
"""Tests for the swarm conductor routing and verification gate.

Covers the VED-360 routing rules and the VED-369 independent-verification gate.
Run with `python3 scripts/test_swarm_conductor.py`. Kept dependency-free (stdlib
only) and importable without starting the conductor loop.
"""

from __future__ import annotations

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_conductor():
    spec = importlib.util.spec_from_file_location(
        "swarm_conductor", os.path.join(HERE, "swarm_conductor.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


conductor = load_conductor()


class RoutingTests(unittest.TestCase):
    def test_write_intent_never_routes_to_a_read_only_unit(self):
        # Every write-intent issue must land on a unit that can edit and commit.
        for title in (
            "pi-rpc: compaction_end shape",
            "plugin hostcall errors are swallowed",
            "compaction is never recorded",
            "provider stream errors truncate",
            "gateway never evicts idle units",
            "unit removable: Host::remove",
            "Conductor routes 'rpc' issues to the read-only reviewer unit, causing a re-dispatch loop",
        ):
            role = conductor.role_for(title)
            self.assertIn(
                role,
                conductor.EDITING_ROLES,
                f"{title!r} routed to read-only {role!r}",
            )

    def test_rpc_routes_to_a_write_capable_unit(self):
        # Regression for VED-360: "rpc" used to map to the read-only reviewer.
        role = conductor.role_for("pi-rpc: compaction_end shape")
        self.assertEqual(role, "coder")
        self.assertNotIn(role, conductor.READ_ONLY_ROLES)

    def test_review_titles_route_to_reviewer(self):
        for title in ("Swarm audit: rpc shape", "Review last commits", "verify the gate"):
            self.assertEqual(conductor.role_for(title), "reviewer")

    def test_status_titles_route_to_the_synthesizer(self):
        self.assertEqual(conductor.role_for("Swarm status — today"), "synthesizer")


class PromptTests(unittest.TestCase):
    def test_write_prompt_asks_to_implement_and_commit(self):
        prompt = conductor.prompt_for("VED-1", "fix it", "coder")
        self.assertIn("implement the fix", prompt)
        self.assertIn("Commit only the files", prompt)
        self.assertIn("set it Done", prompt)

    def test_read_only_prompt_forbids_committing(self):
        for role in ("reviewer", "researcher", "planner"):
            prompt = conductor.prompt_for("VED-1", "audit it", role)
            self.assertIn("do NOT edit files, commit", prompt)
            self.assertNotIn("implement the fix", prompt)
            self.assertNotIn("Commit only the files", prompt)

    def test_prompt_signer_matches_the_role(self):
        self.assertIn("[coder]", conductor.prompt_for("VED-1", "x", "coder"))
        self.assertIn("[reviewer]", conductor.prompt_for("VED-1", "x", "reviewer"))

    def test_owner_prompt_reports_for_verification_not_done(self):
        # VED-369: an owner must not set its own card Done.
        prompt = conductor.prompt_for("VED-1", "fix it", "coder")
        self.assertIn("In Review", prompt)
        self.assertIn("do NOT set it Done", prompt)
        self.assertNotIn("set it Done when the gate is green", prompt)

    def test_verifier_prompt_is_independent_and_read_only(self):
        prompt = conductor.prompt_for("VED-1", "fix it", conductor.VERIFIER_ROLE)
        self.assertIn("FRESH session", prompt)
        self.assertIn("REFUTE", prompt)
        self.assertIn("acceptance check", prompt)
        self.assertIn("net test count decreased", prompt)
        self.assertIn("Do NOT edit files, commit", prompt)
        self.assertIn("verifier: pass", prompt)
        self.assertIn("[verifier]", prompt)


class TestCountTests(unittest.TestCase):
    def test_sums_all_test_result_lines(self):
        output = (
            "test result: ok. 10 passed; 0 failed; 0 ignored\n"
            "test result: ok. 5 passed; 0 failed; 1 ignored\n"
        )
        self.assertEqual(conductor.parse_test_count(output), 15)

    def test_returns_none_without_a_summary(self):
        self.assertIsNone(conductor.parse_test_count("running 3 tests\n"))
        self.assertIsNone(conductor.parse_test_count(""))

    def test_net_reduction_is_detected(self):
        self.assertTrue(conductor.tests_decreased(228, 227))
        self.assertFalse(conductor.tests_decreased(227, 228))
        self.assertFalse(conductor.tests_decreased(228, 228))

    def test_unknown_counts_are_not_a_reduction(self):
        self.assertFalse(conductor.tests_decreased(None, 10))
        self.assertFalse(conductor.tests_decreased(10, None))


class VerificationGateTests(unittest.TestCase):
    def test_pass_requires_acceptance_refutation_and_no_reduction(self):
        verdict, reasons = conductor.verification_verdict(True, False, 228, 230)
        self.assertEqual(verdict, conductor.VERIFY_PASS)
        self.assertEqual(reasons, [])

    def test_failed_acceptance_is_rejected(self):
        verdict, reasons = conductor.verification_verdict(False, False, 228, 228)
        self.assertEqual(verdict, conductor.VERIFY_FAIL)
        self.assertTrue(any("acceptance" in reason for reason in reasons))

    def test_refutation_is_rejected(self):
        verdict, reasons = conductor.verification_verdict(True, True, 228, 228)
        self.assertEqual(verdict, conductor.VERIFY_FAIL)
        self.assertTrue(any("refuted" in reason for reason in reasons))

    def test_net_test_reduction_is_always_rejected(self):
        # Even a passing acceptance check and an unrefuted claim cannot smuggle
        # a smaller test suite through the gate.
        verdict, reasons = conductor.verification_verdict(True, False, 228, 227)
        self.assertEqual(verdict, conductor.VERIFY_FAIL)
        self.assertTrue(any("net test reduction" in reason for reason in reasons))

    def test_done_requires_a_stored_verifier_pass(self):
        # An In Review card with no stored pass is never promoted to Done.
        self.assertEqual(
            conductor.gate_decision(conductor.IN_REVIEW, {}), "verify"
        )
        # A stored pass on an In Review card promotes it.
        self.assertEqual(
            conductor.gate_decision(
                conductor.IN_REVIEW, {"verification": conductor.VERIFY_PASS}
            ),
            "done",
        )

    def test_a_failed_verdict_reopens_the_card(self):
        record = {}
        conductor.record_verdict(record, conductor.VERIFY_FAIL, ["refuted"])
        self.assertEqual(record["status"], "reopened")
        self.assertFalse(record["terminal"])
        self.assertEqual(conductor.gate_decision(conductor.IN_REVIEW, record), "reopen")

    def test_reopened_card_can_be_verified_again(self):
        # After a fail and reopen, a new In Review report must reach the
        # verifier again, not loop on the stale fail verdict forever.
        record = {}
        conductor.record_verdict(record, conductor.VERIFY_FAIL, ["refuted"])
        self.assertEqual(conductor.gate_decision(conductor.IN_REVIEW, record), "reopen")
        # The reopen branch re-dispatches the owner and clears the stale verdict.
        record["status"] = "dispatched"
        record.pop("verification", None)
        self.assertEqual(conductor.gate_decision(conductor.IN_REVIEW, record), "verify")

    def test_a_pass_is_terminal(self):
        record = {}
        conductor.record_verdict(record, conductor.VERIFY_PASS, [])
        self.assertTrue(record["terminal"])
        self.assertEqual(record["status"], "done")

    def test_inflight_verification_is_not_dispatched_twice(self):
        record = {"status": conductor.AWAITING_VERIFICATION}
        self.assertFalse(conductor.should_verify(conductor.IN_REVIEW, record))
        self.assertEqual(conductor.gate_decision(conductor.IN_REVIEW, record), "skip")

    def test_a_card_not_in_review_is_not_verified(self):
        self.assertFalse(conductor.should_verify(conductor.IN_PROGRESS, {}))
        self.assertEqual(
            conductor.gate_decision(conductor.IN_PROGRESS, {}), "skip"
        )


class VerifierRoutingTests(unittest.TestCase):
    def test_verifier_is_read_only_and_distinct(self):
        self.assertIn(conductor.VERIFIER_ROLE, conductor.READ_ONLY_ROLES)
        self.assertNotIn(conductor.VERIFIER_ROLE, conductor.EDITING_ROLES)
        # A dedicated session id, so verification never reuses the owner state.
        self.assertIn(conductor.VERIFIER_ROLE, conductor.ROLE_UNITS)

    def test_verifier_prompt_does_not_ask_to_commit(self):
        prompt = conductor.verifier_prompt("VED-1", "audit it")
        self.assertNotIn("implement the fix", prompt)
        self.assertNotIn("Commit only the files", prompt)


class VerifierReportTests(unittest.TestCase):
    def test_parses_a_pass_block(self):
        body = "Re-ran the gate.\n\nverifier: pass\ntests: 228 -> 230\n- [verifier]"
        self.assertEqual(conductor.parse_verifier_report(body), ("pass", 228, 230))

    def test_parses_a_fail_block_with_unknown_counts(self):
        body = "Acceptance failed.\n\nverifier: fail\ntests: ? -> ?\n"
        self.assertEqual(conductor.parse_verifier_report(body), ("fail", None, None))

    def test_ignores_prose_without_a_verdict_block(self):
        self.assertEqual(conductor.parse_verifier_report("looks good to me"), (None, None, None))
        self.assertEqual(conductor.parse_verifier_report(""), (None, None, None))

    def test_verdict_line_must_be_its_own_line(self):
        # A passing mention in prose must not be mistaken for a verdict.
        self.assertEqual(
            conductor.parse_verifier_report("I will not say verifier: pass here"),
            (None, None, None),
        )

    def test_ingest_stores_a_pass(self):
        conductor.issue_comments = lambda identifier: [
            "owner: done-ish",
            "verifier: pass\ntests: 228 -> 228",
        ]
        record = {}
        conductor.ingest_verifier_verdict("VED-1", record)
        self.assertEqual(record["verification"], conductor.VERIFY_PASS)
        self.assertTrue(record["terminal"])

    def test_ingest_overrides_a_pass_with_a_net_reduction(self):
        # The deterministic guard beats the verifier's own claim (VED-369).
        conductor.issue_comments = lambda identifier: ["verifier: pass\ntests: 228 -> 227"]
        record = {}
        conductor.ingest_verifier_verdict("VED-1", record)
        self.assertEqual(record["verification"], conductor.VERIFY_FAIL)
        self.assertTrue(any("net test reduction" in r for r in record["refutation"]))

    def test_ingest_stores_a_fail(self):
        conductor.issue_comments = lambda identifier: ["verifier: fail\ntests: 228 -> 228"]
        record = {}
        conductor.ingest_verifier_verdict("VED-1", record)
        self.assertEqual(record["verification"], conductor.VERIFY_FAIL)
        self.assertEqual(record["status"], "reopened")

    def test_ingest_uses_the_newest_verdict(self):
        conductor.issue_comments = lambda identifier: [
            "verifier: fail\ntests: 228 -> 228",
            "verifier: pass\ntests: 228 -> 229",
        ]
        record = {}
        conductor.ingest_verifier_verdict("VED-1", record)
        self.assertEqual(record["verification"], conductor.VERIFY_PASS)

    def test_ingest_skips_comments_without_a_verdict(self):
        conductor.issue_comments = lambda identifier: ["just a note"]
        record = {}
        conductor.ingest_verifier_verdict("VED-1", record)
        self.assertNotIn("verification", record)


class RetryTests(unittest.TestCase):
    def test_terminal_statuses_are_recognized(self):
        for status in ("done", "declined", "blocked", "dispatched_readonly"):
            self.assertIn(status, conductor.TERMINAL_STATUSES)

    def test_read_only_dispatch_is_terminal_after_one_attempt(self):
        # Simulate the conductor's claim decision for a review-routed issue.
        record = {"attempts": 0}
        role = conductor.role_for("Swarm audit: rpc shape")
        self.assertIn(role, conductor.READ_ONLY_ROLES)
        # The dispatch bookkeeping the conductor applies for read-only roles.
        record["attempts"] += 1
        record["status"] = "dispatched_readonly"
        record["terminal"] = True
        self.assertTrue(record["terminal"])
        self.assertIn(record["status"], conductor.TERMINAL_STATUSES)


if __name__ == "__main__":
    unittest.main()
