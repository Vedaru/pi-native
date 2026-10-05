#!/usr/bin/env python3
"""Tests for the deterministic swarm orchestrator (VED-368).

Run with `python3 scripts/test_swarm_orchestrator.py`. stdlib only and
importable without touching Linear or the gateway.

Covers the acceptance criteria:

* a claim table that survives a restart;
* exponential-backoff retries on transient failure;
* reconcile: an issue moved to Done/Canceled stops its run and releases the
  claim;
* the concurrency cap is configurable.
"""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_orchestrator():
    spec = importlib.util.spec_from_file_location(
        "swarm_orchestrator", os.path.join(HERE, "swarm_orchestrator.py")
    )
    module = importlib.util.module_from_spec(spec)
    # Register before exec so @dataclass can resolve the module (Python 3.14
    # looks the class's module up in sys.modules).
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


orch = load_orchestrator()

ROLES = {
    "coder": "sess-coder",
    "tester": "sess-tester",
    "synthesizer": "sess-synth",
    "reviewer": "sess-reviewer",
    "planner": "sess-planner",
    "researcher": "sess-researcher",
}
READ_ONLY = {"reviewer", "planner", "researcher"}


def board(*entries):
    return {identifier: snapshot for identifier, snapshot in entries}


def active(title="fix it", state="Todo"):
    return {"title": title, "state": state, "stateType": "unstarted"}


def started(title="fix it"):
    return {"title": title, "state": "In Progress", "stateType": "started"}


def done(title="fix it"):
    return {"title": title, "state": "Done", "stateType": "completed"}


def canceled(title="fix it"):
    return {"title": title, "state": "Canceled", "stateType": "canceled"}


def route(title):
    return "coder"


def tick(orchestrator, snapshots, now, busy=None, route_fn=route):
    return orchestrator.tick(
        snapshots,
        now=now,
        is_busy=busy or (lambda _sid: False),
        route=route_fn,
        session_for=lambda role: ROLES[role],
        read_only_roles=READ_ONLY,
    )


class BackoffTests(unittest.TestCase):
    def test_backoff_is_exponential_and_capped(self):
        self.assertEqual(orch.backoff_delay(0), 0.0)
        self.assertEqual(orch.backoff_delay(1, base=10, cap=1000), 10)
        self.assertEqual(orch.backoff_delay(2, base=10, cap=1000), 20)
        self.assertEqual(orch.backoff_delay(3, base=10, cap=1000), 40)
        self.assertEqual(orch.backoff_delay(9, base=10, cap=1000), 1000)

    def test_failure_schedules_a_backoff_retry(self):
        orchestrator = orch.Orchestrator()
        claim = orchestrator.claim_for("VED-1")
        claim.status = orch.STATUS_DISPATCHED
        claim.session_id = "sess-coder"
        orchestrator.running["sess-coder"] = "VED-1"
        decision = orchestrator.record_failure("VED-1", now=100.0, reason="boom")
        self.assertEqual(decision.action, "retry")
        self.assertEqual(claim.attempt, 1)
        self.assertAlmostEqual(claim.next_retry_at, 100.0 + orch.DEFAULT_BACKOFF_BASE_SECONDS)
        self.assertNotIn("sess-coder", orchestrator.running)

    def test_retries_exhaust_and_become_terminal(self):
        config = orch.OrchestratorConfig(max_attempts=2, backoff_base=1, backoff_cap=1)
        orchestrator = orch.Orchestrator(config=config)
        claim = orchestrator.claim_for("VED-1")
        claim.status = orch.STATUS_DISPATCHED
        orchestrator.record_failure("VED-1", now=0.0, reason="a")
        decision = orchestrator.record_failure("VED-1", now=1.0, reason="b")
        self.assertEqual(decision.action, "release")
        self.assertTrue(claim.terminal)
        self.assertFalse(claim.eligible(now=10_000.0, max_attempts=2))

    def test_claim_not_eligible_until_backoff_elapses(self):
        orchestrator = orch.Orchestrator()
        claim = orchestrator.claim_for("VED-1")
        claim.attempt = 1
        claim.next_retry_at = 500.0
        self.assertFalse(claim.eligible(now=499.0, max_attempts=3))
        self.assertTrue(claim.eligible(now=500.0, max_attempts=3))


class ClaimTablePersistenceTests(unittest.TestCase):
    def test_claim_table_survives_a_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "claims.json")
            orchestrator = orch.Orchestrator()
            tick(orchestrator, board(("VED-1", active())), now=100.0)
            claim = orchestrator.claims["VED-1"]
            claim.attempt = 2
            claim.next_retry_at = 999.0
            orchestrator.save(path)

            restarted = orch.Orchestrator.load(path)
            self.assertIn("VED-1", restarted.claims)
            reloaded = restarted.claims["VED-1"]
            self.assertEqual(reloaded.attempt, 2)
            self.assertEqual(reloaded.next_retry_at, 999.0)
            self.assertEqual(reloaded.session_id, "sess-coder")

    def test_restart_does_not_double_dispatch_an_in_flight_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "claims.json")
            orchestrator = orch.Orchestrator()
            tick(orchestrator, board(("VED-1", started())), now=100.0)
            orchestrator.save(path)

            restarted = orch.Orchestrator.load(path)
            # The run is still considered in flight after the restart, so the
            # same issue is not dispatched again.
            self.assertEqual(restarted.running.get("sess-coder"), "VED-1")
            decisions = tick(restarted, board(("VED-1", started())), now=200.0)
            self.assertEqual(
                [d for d in decisions if d.action == "dispatch"], []
            )

    def test_missing_or_corrupt_state_loads_empty(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = orch.Orchestrator.load(os.path.join(directory, "nope.json"))
            self.assertEqual(missing.claims, {})
            corrupt_path = os.path.join(directory, "bad.json")
            with open(corrupt_path, "w") as handle:
                handle.write("{not json")
            corrupt = orch.Orchestrator.load(corrupt_path)
            self.assertEqual(corrupt.claims, {})


class ConcurrencyTests(unittest.TestCase):
    def test_concurrency_cap_is_configurable(self):
        config = orch.OrchestratorConfig(max_concurrent_agents=1)
        orchestrator = orch.Orchestrator(config=config)
        decisions = tick(
            orchestrator,
            board(("VED-1", active()), ("VED-2", active()), ("VED-3", active())),
            now=1.0,
        )
        dispatched = [d for d in decisions if d.action == "dispatch"]
        skipped = [d for d in decisions if d.action == "skip"]
        self.assertEqual(len(dispatched), 1)
        self.assertEqual(len(orchestrator.running), 1)
        # Skipped entries include the concurrency reason (only if they reached
        # the cap check; cards routed to a busy unit are skipped too).
        self.assertTrue(any("cap" in d.reason or "busy" in d.reason for d in skipped))

    def test_default_cap_is_ten(self):
        self.assertEqual(orch.DEFAULT_MAX_CONCURRENT_AGENTS, 10)
        self.assertEqual(orch.Orchestrator().config.max_concurrent_agents, 10)

    def test_cap_allows_up_to_the_limit(self):
        config = orch.OrchestratorConfig(max_concurrent_agents=3)
        orchestrator = orch.Orchestrator(config=config)
        roles = {"coder": "s1", "tester": "s2", "synthesizer": "s3"}

        # Distinct sessions via per-issue routing.
        snapshots = board(
            ("VED-1", active()),
            ("VED-2", active()),
            ("VED-3", active()),
            ("VED-4", active()),
        )
        counters = {"n": 0}

        def round_robin(_title):
            values = list(roles)
            value = values[counters["n"] % len(values)]
            counters["n"] += 1
            return value

        decisions = orchestrator.tick(
            snapshots,
            now=1.0,
            is_busy=lambda _sid: False,
            route=round_robin,
            session_for=lambda role: roles[role],
            read_only_roles=set(),
        )
        dispatched = [d for d in decisions if d.action == "dispatch"]
        self.assertEqual(len(dispatched), 3)
        self.assertEqual(len(orchestrator.running), 3)


class ReconciliationTests(unittest.TestCase):
    def test_issue_moved_to_done_stops_its_run(self):
        config = orch.OrchestratorConfig(max_concurrent_agents=2)
        orchestrator = orch.Orchestrator(config=config)
        tick(orchestrator, board(("VED-1", started())), now=1.0)
        self.assertIn("sess-coder", orchestrator.running)

        # Next tick the card is Done -> release and stop.
        decisions = tick(
            orchestrator,
            board(("VED-1", done())),
            now=200.0,
            busy=lambda _sid: True,
        )
        releases = [d for d in decisions if d.action == "stop"]
        self.assertEqual(len(releases), 1)
        self.assertEqual(releases[0].issue, "VED-1")
        self.assertEqual(releases[0].session_id, "sess-coder")
        self.assertNotIn("sess-coder", orchestrator.running)
        self.assertTrue(orchestrator.claims["VED-1"].terminal)

    def test_canceled_issue_releases_its_claim(self):
        orchestrator = orch.Orchestrator()
        tick(orchestrator, board(("VED-1", started())), now=1.0)
        tick(orchestrator, board(("VED-1", canceled())), now=200.0, busy=lambda _s: True)
        self.assertTrue(orchestrator.claims["VED-1"].terminal)
        self.assertEqual(orchestrator.running, {})

    def test_issue_leaving_the_board_releases_its_claim(self):
        orchestrator = orch.Orchestrator()
        tick(orchestrator, board(("VED-1", started())), now=1.0)
        decisions = tick(orchestrator, board(), now=200.0, busy=lambda _s: True)
        self.assertTrue(any(d.action == "stop" for d in decisions))
        self.assertEqual(orchestrator.running, {})

    def test_startup_cleanup_drops_terminal_cards(self):
        orchestrator = orch.Orchestrator()
        orchestrator.claims["VED-1"] = orch.Claim(issue="VED-1")
        orchestrator.claims["VED-2"] = orch.Claim(issue="VED-2")
        decisions = orchestrator.startup_cleanup(active_issues={"VED-2"})
        self.assertEqual([d.issue for d in decisions], ["VED-1"])
        self.assertNotIn("VED-1", orchestrator.claims)
        self.assertIn("VED-2", orchestrator.claims)

    def test_reopened_card_is_revived(self):
        orchestrator = orch.Orchestrator()
        tick(orchestrator, board(("VED-1", started())), now=1.0)
        tick(orchestrator, board(("VED-1", done())), now=200.0, busy=lambda _s: True)
        self.assertTrue(orchestrator.claims["VED-1"].terminal)
        # A human moves the card back to an active state.
        decisions = tick(
            orchestrator, board(("VED-1", active())), now=400.0, busy=lambda _s: True
        )
        self.assertTrue(any(d.action == "retry" for d in decisions))
        self.assertFalse(orchestrator.claims["VED-1"].terminal)

    def test_exhausted_retry_is_not_revived(self):
        config = orch.OrchestratorConfig(max_attempts=1)
        orchestrator = orch.Orchestrator(config=config)
        tick(orchestrator, board(("VED-1", started())), now=1.0)
        tick(orchestrator, board(("VED-1", started())), now=200.0, busy=lambda _s: False)
        self.assertTrue(orchestrator.claims["VED-1"].terminal)
        decisions = tick(orchestrator, board(("VED-1", active())), now=400.0)
        self.assertFalse(any(d.action == "retry" for d in decisions))

    def test_no_retry_before_the_grace_window(self):
        orchestrator = orch.Orchestrator()
        tick(orchestrator, board(("VED-1", started())), now=100.0)
        # Immediately after dispatch the unit may not have emitted events yet.
        decisions = tick(
            orchestrator,
            board(("VED-1", started())),
            now=100.0 + orch.DISPATCH_GRACE_SECONDS - 1,
            busy=lambda _sid: False,
        )
        self.assertEqual([d for d in decisions if d.action == "retry"], [])
        self.assertIn("sess-coder", orchestrator.running)


class DispatchTests(unittest.TestCase):
    def test_new_active_card_is_dispatched_once(self):
        orchestrator = orch.Orchestrator()
        decisions = tick(orchestrator, board(("VED-1", active())), now=1.0)
        dispatched = [d for d in decisions if d.action == "dispatch"]
        self.assertEqual(len(dispatched), 1)
        self.assertEqual(dispatched[0].session_id, "sess-coder")
        self.assertEqual(orchestrator.claims["VED-1"].attempt, 1)

    def test_read_only_audit_is_dispatched_once(self):
        orchestrator = orch.Orchestrator()

        def review_route(_title):
            return "reviewer"

        decisions = tick(
            orchestrator, board(("VED-1", active("audit rpc"))), now=1.0, route_fn=review_route
        )
        dispatched = [d for d in decisions if d.action == "dispatch"]
        self.assertEqual(len(dispatched), 1)
        claim = orchestrator.claims["VED-1"]
        self.assertTrue(claim.terminal)
        # A read-only run does not hold a concurrency slot.
        self.assertEqual(orchestrator.running, {})
        decisions = tick(orchestrator, board(("VED-1", active("audit rpc"))), now=2.0, route_fn=review_route)
        self.assertEqual([d for d in decisions if d.action == "dispatch"], [])

    def test_one_session_is_not_targeted_twice_in_a_tick(self):
        orchestrator = orch.Orchestrator()
        decisions = tick(
            orchestrator, board(("VED-1", active()), ("VED-2", active())), now=1.0
        )
        dispatched = [d for d in decisions if d.action == "dispatch"]
        self.assertEqual(len(dispatched), 1)
        self.assertTrue(any("busy" in d.reason for d in decisions if d.action == "skip"))

    def test_snapshot_reports_the_claim_table(self):
        orchestrator = orch.Orchestrator()
        tick(orchestrator, board(("VED-1", active())), now=1.0)
        snapshot = orchestrator.snapshot()
        self.assertEqual(snapshot["maxConcurrentAgents"], 10)
        self.assertEqual(snapshot["inFlight"], 1)
        self.assertIn("VED-1", snapshot["claims"])


if __name__ == "__main__":
    unittest.main()
