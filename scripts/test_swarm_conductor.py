#!/usr/bin/env python3
"""Tests for the swarm conductor routing and verification gate.

Covers the VED-360/VED-365 routing rules and the VED-369 independent-verification
gate.
Run with `python3 scripts/test_swarm_conductor.py`. Kept dependency-free (stdlib
only) and importable without starting the conductor loop. The `tick()` tests
stub `open_issues`/`dispatch`/`issue_comments` so no gateway or Linear call is
made.
"""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))


def load_conductor():
    spec = importlib.util.spec_from_file_location(
        "swarm_conductor", os.path.join(HERE, "swarm_conductor.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


conductor = load_conductor()


def conductor_shared_orchestrator():
    """A fresh in-memory orchestrator for the ticket/dispatch tests."""
    spec = importlib.util.spec_from_file_location(
        "swarm_orchestrator", os.path.join(HERE, "swarm_orchestrator.py")
    )
    module = importlib.util.module_from_spec(spec)
    import sys

    sys.modules["swarm_orchestrator"] = module
    spec.loader.exec_module(module)
    return module.Orchestrator(module.OrchestratorConfig())


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

    def test_swarm_audit_titles_are_write_work(self):
        # Regression for VED-365: the VED project titles write issues
        # "Swarm audit: <fix list>"; they must reach a write-capable unit.
        for title in (
            "Swarm audit: RPC tool-result shape + stale session name on switch",
            "Swarm audit: plugin randomBytes DoS",
        ):
            role = conductor.role_for(title)
            self.assertEqual(role, "coder", f"{title!r} -> {role!r}")
            self.assertIn(role, conductor.EDITING_ROLES)

    def test_explicitly_read_only_audit_routes_to_reviewer(self):
        for title in (
            "Swarm audit: read-only review of the RPC shape",
            "Swarm audit: report only on the plugin host",
            "Swarm audit findings-only: memory leaks",
        ):
            self.assertEqual(conductor.role_for(title), "reviewer")

    def test_review_titles_route_to_reviewer(self):
        for title in ("Review last commits", "verify the gate"):
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

    def test_read_only_prompt_asks_for_a_machine_readable_verdict(self):
        # VED-365: the reviewer promises a re-route; that promise must be a
        # marker the conductor can parse, not prose it cannot read.
        prompt = conductor.prompt_for("VED-1", "audit it", "reviewer")
        self.assertIn("@conductor route-to: coder", prompt)
        self.assertIsNotNone(conductor.ROUTE_VERDICT_RE.search(prompt))

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


class FreshContextTests(unittest.TestCase):
    def test_fresh_context_is_on_by_default(self):
        self.assertTrue(conductor.FRESH_CONTEXT)

    def test_reset_context_posts_to_the_reset_route(self):
        # Keep the conductor's fresh-context-per-card default honest: the reset
        # request must hit `/sessions/:id/reset` with an empty JSON body.
        seen = {}

        class FakeResponse:
            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        def fake_urlopen(request, timeout=None):
            seen["url"] = request.full_url
            seen["method"] = request.get_method()
            seen["data"] = request.data
            return FakeResponse()

        original = conductor.urllib.request.urlopen
        conductor.urllib.request.urlopen = fake_urlopen
        try:
            conductor.reset_context("abc-0")
        finally:
            conductor.urllib.request.urlopen = original
        self.assertTrue(seen["url"].endswith("/sessions/abc-0/reset"), seen["url"])
        self.assertEqual(seen["method"], "POST")


class VerdictTests(unittest.TestCase):
    def test_route_target_reads_the_last_verdict(self):
        comments = [
            "Reviewer verification (read-only).",
            "Needs a code change.\n@conductor route-to: coder\nSign - [reviewer]",
        ]
        self.assertEqual(conductor.route_target(comments), "coder")

    def test_route_target_ignores_read_only_targets(self):
        self.assertIsNone(conductor.route_target(["@conductor route-to: reviewer"]))
        self.assertIsNone(conductor.route_target(["@conductor route-to: planner"]))

    def test_route_target_none_without_a_verdict(self):
        self.assertIsNone(conductor.route_target(["just findings", ""]))


class TicketTests(unittest.TestCase):
    def setUp(self):
        self.sent = []
        self.comments = []
        patches = [
            mock.patch.object(
                conductor,
                "dispatch",
                lambda session_id, text: self.sent.append((session_id, text)),
            ),
            mock.patch.object(conductor, "issue_comments", lambda identifier: self.comments),
            mock.patch("builtins.print"),
        ]
        for patcher in patches:
            patcher.start()
            self.addCleanup(patcher.stop)

    def _tick(self, orchestrator, now, title="Review the rpc shape"):
        """Drive one orchestrator pass using the conductor's routing."""
        board = {"VED-1": {"state": "Todo", "stateType": "unstarted", "title": title}}
        decisions = orchestrator.tick(
            board,
            now=now,
            is_busy=lambda _sid: False,
            route=conductor.role_for,
            session_for=lambda role: conductor.ROLE_UNITS[role],
            read_only_roles=conductor.READ_ONLY_ROLES,
            reroute=conductor.pending_route,
        )
        for decision in decisions:
            if decision.action == "dispatch":
                conductor.dispatch(
                    decision.session_id,
                    conductor.prompt_for(decision.issue, title, decision.role),
                )
        return decisions

    def test_terminal_statuses_are_recognized(self):
        for status in ("done", "declined", "blocked", "dispatched_readonly"):
            self.assertIn(status, conductor.TERMINAL_STATUSES)

    def test_read_only_dispatch_is_terminal_after_one_attempt(self):
        # A review-routed issue is audited once, then not retried.
        orchestrator = conductor_shared_orchestrator()
        self._tick(orchestrator, 0.0)
        claim = orchestrator.claims["VED-1"]
        self.assertEqual(claim.role, "reviewer")
        self.assertTrue(claim.read_only)
        self.assertTrue(claim.terminal)
        self.sent.clear()
        self._tick(orchestrator, 1.0)
        self.assertEqual(self.sent, [])

    def test_verdict_reroutes_a_read_only_claim_to_a_write_unit(self):
        # VED-365: once the reviewer says "route to coder", the terminal
        # read-only claim is re-dispatched to a write-capable unit.
        orchestrator = conductor_shared_orchestrator()
        self._tick(orchestrator, 0.0)
        self.sent.clear()
        self.comments = ["needs a fix\n@conductor route-to: coder\n- [reviewer]"]
        self._tick(orchestrator, 1.0)
        self.assertEqual(len(self.sent), 1)
        session_id, text = self.sent[0]
        self.assertEqual(session_id, conductor.ROLE_UNITS["coder"])
        self.assertIn("implement the fix", text)
        claim = orchestrator.claims["VED-1"]
        self.assertEqual(claim.role, "coder")
        self.assertEqual(claim.status, "dispatched")
        self.assertTrue(claim.rerouted)

    def test_reroute_happens_only_once(self):
        orchestrator = conductor_shared_orchestrator()
        self._tick(orchestrator, 0.0)
        self.comments = ["@conductor route-to: coder"]
        self._tick(orchestrator, 1.0)
        self.sent.clear()
        self._tick(orchestrator, 2.0)
        self.assertEqual(self.sent, [])



class DagGateTests(unittest.TestCase):
    """VED-378: the conductor skips a card whose DAG deps are not Done."""

    def setUp(self):
        self._old = os.environ.get("SWARM_DAG_PATH")
        self._tmp = tempfile.NamedTemporaryFile(delete=False, suffix=".json")
        self._tmp.close()
        os.unlink(self._tmp.name)  # no cache yet
        os.environ["SWARM_DAG_PATH"] = self._tmp.name

    def tearDown(self):
        if self._old is None:
            os.environ.pop("SWARM_DAG_PATH", None)
        else:
            os.environ["SWARM_DAG_PATH"] = self._old
        if os.path.exists(self._tmp.name):
            os.unlink(self._tmp.name)

    def test_no_cache_means_flat_loop(self):
        # With no DAG cache the gate is a no-op, so the flat conductor is
        # unchanged and cannot be broken by the DAG feature.
        self.assertIsNone(conductor.dag_gate())

    def test_broken_cache_does_not_raise(self):
        with open(self._tmp.name, "w") as handle:
            handle.write("{not json")
        self.assertIsNone(conductor.dag_gate())


if __name__ == "__main__":
    unittest.main()
