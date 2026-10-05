#!/usr/bin/env python3
"""Tests for the unit health watchdog (VED-376).

Run with `python3 scripts/test_swarm_watchdog.py`. Stdlib only, no gateway or
network: every test injects units, cards, claims and `now` into the pure
classifier/planner, or drives `apply_action` against a temp state file.

Covers the planner v2 matrix W1-W24.
"""

from __future__ import annotations

import importlib.util
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
NOW = 1_000_000.0


def load_watchdog():
    spec = importlib.util.spec_from_file_location(
        "swarm_watchdog", os.path.join(HERE, "swarm_watchdog.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wd = load_watchdog()

CONFIG = {"stall_seconds": 600, "max_recoveries": 3, "now": NOW}


def unit(role="coder", *, session_id=None, running=True, suspended=False, dead=False,
         last_event="tool_start", last_event_at=None, busy=False):
    sid = session_id or wd.ROLE_UNITS[role]
    # Default to silent past the stall threshold so stall tests fire; a test
    # that needs a busy unit passes last_event_at=NOW-5 explicitly.
    return {
        "sessionId": sid,
        "unit": role,
        "name": role,
        "running": running and not suspended and not dead,
        "suspended": suspended,
        "dead": dead,
        "lastEvent": last_event,
        "lastEventAt": NOW - 5000 if last_event_at is None else last_event_at,
        "busy": busy,
    }


def card(identifier, state="In Progress", title="a card"):
    return {"identifier": identifier, "title": title, "state": {"name": state, "type": "started"}}


def claim(role="coder", *, at=None, recoveries=0, terminal=False, recovery_at=None):
    # Default to a long-ago dispatch so the derived silence exceeds the stall
    # threshold; tests that exercise a fresh dispatch pass `at=NOW`.
    record = {"attempts": 1, "role": role, "at": NOW - 5000 if at is None else at}
    if recoveries:
        record["recoveries"] = recoveries
    if terminal:
        record["terminal"] = True
    if recovery_at is not None:
        record["recovery_at"] = recovery_at
    return record


class DetectionTests(unittest.TestCase):
    # W1: a silent live unit holding an In Progress card is stalled.
    def test_stalled_detected(self):
        findings = wd.find_findings(
            [unit()], [card("VED-1")], {"VED-1": claim()}, NOW, CONFIG
        )
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["kind"], wd.STALLED)
        self.assertEqual(findings[0]["silent_for"], 5000.0)

    # W2: a busy unit is healthy, no finding.
    def test_healthy_busy_not_flagged(self):
        findings = wd.find_findings(
            [unit(last_event="tool_start", last_event_at=NOW - 5)],
            [card("VED-1")],
            {"VED-1": claim()},
            NOW,
            CONFIG,
        )
        self.assertEqual(findings, [])

    # W3: an idle unit with no open card is never flagged.
    def test_idle_no_card_not_flagged(self):
        findings = wd.find_findings([unit(running=False)], [], {}, NOW, CONFIG)
        self.assertEqual(findings, [])

    # W4: a claim with no session is orphaned.
    def test_orphaned_detected(self):
        findings = wd.find_findings([], [card("VED-1")], {"VED-1": claim()}, NOW, CONFIG)
        self.assertEqual(findings[0]["kind"], wd.ORPHANED)

    # W5: lastEventAt == 0 with a fresh dispatch is unknown, not stalled.
    def test_last_event_zero_is_unknown(self):
        unit_zero = unit(last_event_at=0)
        findings = wd.find_findings(
            [unit_zero], [card("VED-1")], {"VED-1": claim(at=NOW)}, NOW, CONFIG
        )
        # dispatched_at is recent -> silent_for small -> healthy, never epoch-stalled.
        self.assertEqual(findings, [])
        kind, silent = wd.classify(unit_zero, claim(at=0), "In Progress", NOW, CONFIG)
        self.assertEqual(kind, wd.UNKNOWN)
        self.assertIsNone(silent)

    # W5b: a dead worker thread is dead (the running-lies-after-panic case).
    def test_dead_detected(self):
        findings = wd.find_findings(
            [unit(dead=True, running=False)], [card("VED-1")], {"VED-1": claim()}, NOW, CONFIG
        )
        self.assertEqual(findings[0]["kind"], wd.DEAD)

    # W5c: a reaper-suspended unit mid-card is orphaned (recoverable by wake).
    def test_suspended_mid_card_is_orphaned(self):
        findings = wd.find_findings(
            [unit(suspended=True, running=False)],
            [card("VED-1")],
            {"VED-1": claim()},
            NOW,
            CONFIG,
        )
        self.assertEqual(findings[0]["kind"], wd.ORPHANED)

    # W16: clock skew — a future timestamp is healthy.
    def test_clock_skew_future_is_healthy(self):
        findings = wd.find_findings(
            [unit(last_event_at=NOW + 1000)],
            [card("VED-1")],
            {"VED-1": claim()},
            NOW,
            CONFIG,
        )
        self.assertEqual(findings, [])

    # W17: no-claim In Progress card is orphaned.
    def test_no_claim_open_card_is_orphaned(self):
        findings = wd.find_findings([unit()], [card("VED-1")], {}, NOW, CONFIG)
        self.assertEqual(findings[0]["kind"], wd.ORPHANED)

    # W18: a stall threshold of 0 disables detection.
    def test_disabled(self):
        disabled = {"stall_seconds": 0, "max_recoveries": 3, "now": NOW}
        findings = wd.find_findings([unit()], [card("VED-1")], {"VED-1": claim()}, NOW, disabled)
        self.assertEqual(findings, [])

    # In Review is watched for Needs you but the classifier still reports state.
    def test_in_review_is_watched(self):
        findings = wd.find_findings(
            [unit()], [card("VED-1", state="In Review")], {"VED-1": claim()}, NOW, CONFIG
        )
        self.assertEqual(len(findings), 1)

    # W-dedup: two cards claimed by one role is reported, not guessed.
    def test_duplicate_claim_reported(self):
        findings = wd.find_findings(
            [unit()],
            [card("VED-1"), card("VED-2")],
            {"VED-1": claim(), "VED-2": claim()},
            NOW,
            CONFIG,
        )
        self.assertTrue(any(f["kind"] == "duplicate-claim" for f in findings))


class PlannerTests(unittest.TestCase):
    # W6: exponential backoff schedule, capped.
    def test_backoff_schedule(self):
        self.assertEqual(wd.recovery_delay(1), wd.RECOVERY_BASE_SECONDS)
        self.assertEqual(wd.recovery_delay(2), wd.RECOVERY_BASE_SECONDS * 2)
        self.assertEqual(wd.recovery_delay(3), wd.RECOVERY_BASE_SECONDS * 4)
        self.assertEqual(wd.recovery_delay(50), wd.RECOVERY_MAX_SECONDS)

    # W7: budget exhausted -> needs-you, never another retry.
    def test_retry_exhaustion_needs_you(self):
        findings = wd.find_findings(
            [unit()], [card("VED-1")], {"VED-1": claim(recoveries=3)}, NOW, CONFIG
        )
        actions = wd.plan_actions(findings, [unit()], {"VED-1": claim(recoveries=3)}, CONFIG)
        self.assertEqual(actions[0]["action"], "needs-you")
        self.assertEqual(actions[0]["reason"], "budget-exhausted")

    # W7b: a stalled card inside its backoff window is not re-dispatched.
    def test_backoff_window_blocks_retry(self):
        claims = {"VED-1": claim(recoveries=1, recovery_at=NOW - 5)}
        findings = wd.find_findings([unit()], [card("VED-1")], claims, NOW, CONFIG)
        actions = wd.plan_actions(findings, [unit()], claims, CONFIG)
        self.assertEqual(actions, [])

    # W8: an orphaned card is reassigned to an idle editing unit.
    def test_reassign_on_orphan(self):
        # The card is claimed by a reviewer (no live unit -> orphaned); an idle
        # coder can take it.
        units = [unit("coder", session_id="idle-coder", running=True, busy=False)]
        claims = {"VED-1": claim(role="researcher")}
        findings = wd.find_findings(units, [card("VED-1")], claims, NOW, CONFIG)
        self.assertEqual(findings[0]["kind"], wd.ORPHANED)
        actions = wd.plan_actions(findings, units, claims, CONFIG)
        self.assertEqual(actions[0]["action"], "reassign")
        self.assertEqual(actions[0]["target"], "idle-coder")

    # W9: no capable idle unit -> needs-you, no spin.
    def test_no_capable_unit_needs_you(self):
        units = [unit("coder", suspended=True, running=False)]
        findings = wd.find_findings(
            units, [card("VED-1")], {"VED-1": claim()}, NOW, CONFIG
        )
        actions = wd.plan_actions(findings, units, {"VED-1": claim()}, CONFIG)
        self.assertEqual(actions[0]["action"], "needs-you")
        self.assertEqual(actions[0]["reason"], "no-capable-unit")

    # W5: a dead unit is restarted, not reassigned.
    def test_dead_restart_path(self):
        units = [unit(dead=True, running=False)]
        findings = wd.find_findings(
            units, [card("VED-1")], {"VED-1": claim()}, NOW, CONFIG
        )
        actions = wd.plan_actions(findings, units, {"VED-1": claim()}, CONFIG)
        self.assertEqual(actions[0]["action"], "restart")

    # W10: a read-only card is recovered once, then flagged.
    def test_read_only_recovered_once(self):
        units = [unit("reviewer", last_event="tool_start", last_event_at=NOW - 5000)]
        first = wd.find_findings(
            units, [card("VED-1")], {"VED-1": claim(role="reviewer")}, NOW, CONFIG
        )
        first_actions = wd.plan_actions(
            first, units, {"VED-1": claim(role="reviewer")}, CONFIG
        )
        self.assertEqual(first_actions[0]["action"], "retry")

        second = wd.find_findings(
            units,
            [card("VED-1")],
            {"VED-1": claim(role="reviewer", recoveries=1)},
            NOW,
            CONFIG,
        )
        second_actions = wd.plan_actions(
            second, units, {"VED-1": claim(role="reviewer", recoveries=1)}, CONFIG
        )
        self.assertEqual(second_actions[0]["action"], "needs-you")
        self.assertEqual(second_actions[0]["reason"], "read-only-exhausted")

    # W15: multiple stalls are ordered deterministically.
    def test_deterministic_multi_stall_order(self):
        claims = {
            "VED-3": claim(recoveries=0),
            "VED-1": claim(recoveries=0),
            "VED-2": claim(recoveries=1),
        }
        units = [unit()]
        findings = wd.find_findings(
            units, [card("VED-1"), card("VED-2"), card("VED-3")], claims, NOW, CONFIG
        )
        actions = wd.plan_actions(findings, units, claims, CONFIG)
        order = [a["issue"] for a in actions]
        # Lowest attempt first, then issue id: VED-1, VED-3 (attempt 1), then VED-2 (2).
        self.assertEqual(order, ["VED-1", "VED-3", "VED-2"])

    # W23: a terminal claim is never retried again.
    def test_terminal_claim_not_retried(self):
        claims = {"VED-1": claim(terminal=True)}
        findings = wd.find_findings([unit()], [card("VED-1")], claims, NOW, CONFIG)
        actions = wd.plan_actions(findings, [unit()], claims, CONFIG)
        self.assertEqual(actions, [])


class ApplyActionTests(unittest.TestCase):
    def setUp(self):
        self._old_state = wd.STATE_PATH
        self._tmp = tempfile.NamedTemporaryFile(delete=False, suffix=".json")
        self._tmp.close()
        wd.STATE_PATH = self._tmp.name
        # Stub the side effects (network / Linear CLI).
        self.calls = []
        self._old_dispatch = wd.dispatch
        self._old_comment = wd.post_comment
        wd.dispatch = lambda session_id, text: self.calls.append(("dispatch", session_id))
        wd.post_comment = lambda identifier, finding: self.calls.append(("comment", identifier))

    def tearDown(self):
        wd.STATE_PATH = self._old_state
        wd.dispatch = self._old_dispatch
        wd.post_comment = self._old_comment
        os.unlink(self._tmp.name)

    def _state(self):
        with open(self._tmp.name) as handle:
            return json.load(handle)

    # W11/W20/W24: retry transfers the claim (no duplicate) and writes the
    # recovery time before dispatching; the action is recorded.
    def test_retry_records_before_dispatch(self):
        action = {
            "issue": "VED-1",
            "kind": wd.STALLED,
            "action": "retry",
            "role": "coder",
            "unit": "u",
            "silent_for": 700.0,
            "attempt": 1,
            "reason": None,
            "at": NOW,
        }
        wd.apply_action(action, {})
        state = self._state()
        self.assertEqual(state["VED-1"]["recoveries"], 1)
        self.assertEqual(state["VED-1"]["recovery_at"], NOW)
        self.assertEqual(state["VED-1"]["recovery_kind"], wd.STALLED)
        self.assertEqual(state["VED-1"]["watchdog_log"][0]["action"], "retry")
        self.assertIn(("dispatch", wd.ROLE_UNITS["coder"]), self.calls)

    # W13/W9: needs-you posts a signed comment and marks the claim terminal.
    def test_needs_you_comments_and_terminates(self):
        action = {
            "issue": "VED-1",
            "kind": wd.STALLED,
            "action": "needs-you",
            "role": "coder",
            "unit": "u",
            "silent_for": 700.0,
            "attempt": 3,
            "reason": "budget-exhausted",
            "at": NOW,
        }
        wd.apply_action(action, {})
        state = self._state()
        self.assertTrue(state["VED-1"]["terminal"])
        self.assertEqual(state["VED-1"]["status"], "needs-you")
        self.assertIn(("comment", "VED-1"), self.calls)
        self.assertEqual(state["VED-1"]["watchdog_log"][0]["reason"], "budget-exhausted")

    # W12: reconcile releases the claim and does not resurrect the card.
    def test_reconcile_releases_claim(self):
        action = {
            "issue": "VED-1",
            "kind": wd.STALLED,
            "action": "reconcile",
            "role": "coder",
            "unit": "u",
            "silent_for": 700.0,
            "attempt": 1,
            "reason": "card-left-board",
            "at": NOW,
        }
        wd.apply_action(action, {})
        state = self._state()
        self.assertTrue(state["VED-1"]["terminal"])
        self.assertEqual(state["VED-1"]["at"], 0.0)
        self.assertNotIn(("dispatch", wd.ROLE_UNITS["coder"]), self.calls)

    # W22: the watchdog's busy predicate is the conductor's, byte for byte.
    def test_shared_busy_constant(self):
        self.assertEqual(wd.ACTIVE_EVENTS, wd.conductor.ACTIVE_EVENTS)
        self.assertEqual(wd.BUSY_WINDOW_SECONDS, wd.conductor.BUSY_WINDOW_SECONDS)
        sample = {"lastEvent": "tool_start", "lastEventAt": NOW - 5}
        self.assertEqual(wd.is_busy(sample, NOW), wd.conductor.is_busy(sample, NOW))


if __name__ == "__main__":
    unittest.main(verbosity=2)
