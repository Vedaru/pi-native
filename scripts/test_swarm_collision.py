#!/usr/bin/env python3
"""Tests for the swarm collision protocol (VED-375).

Run with `python3 scripts/test_swarm_collision.py`. Stdlib only, no network,
no gateway. Covers the planner's test matrix T1–T16.
"""

from __future__ import annotations

import importlib.util
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_collision():
    spec = importlib.util.spec_from_file_location(
        "swarm_collision", os.path.join(HERE, "swarm_collision.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


collision = load_collision()


def intent(issue, scope, operation="edit", symbols=None, declared_at=0, **extra):
    record = {
        "issue": issue,
        "role": extra.pop("role", "coder"),
        "session_id": extra.pop("session_id", f"sess-{issue}"),
        "operation": operation,
        "scope": list(scope),
        "symbols": list(symbols or []),
        "declared_at": declared_at,
    }
    record.update(extra)
    return record


class NormalizationTests(unittest.TestCase):
    def test_trailing_slash_and_dot_segments_collapse(self):
        # T14: normalised paths.
        self.assertEqual(
            collision.normalize_scope_entry("./crates/pi-rpc/../pi-rpc/src/lib.rs"),
            "crates/pi-rpc/src/lib.rs",
        )
        self.assertEqual(
            collision.normalize_scope_entry("crates/pi-agent/"), "crates/pi-agent"
        )
        self.assertEqual(
            collision.normalize_scope_entry("crates//pi-host//src/lib.rs"),
            "crates/pi-host/src/lib.rs",
        )

    def test_absolute_path_is_made_relative_to_root(self):
        self.assertEqual(
            collision.normalize_scope_entry(
                "/home/u/repo/crates/pi-rpc/src/lib.rs", root="/home/u/repo"
            ),
            "crates/pi-rpc/src/lib.rs",
        )

    def test_normalized_forms_match(self):
        self.assertEqual(
            collision.scope_intersection(
                ["./crates/pi-rpc/../pi-rpc/src/lib.rs"],
                ["crates/pi-rpc/src/lib.rs"],
            ),
            ["crates/pi-rpc/src/lib.rs"],
        )


class DeclareTests(unittest.TestCase):
    def test_intent_persists_across_restart(self):
        # T1: intent persists across restart.
        with tempfile.TemporaryDirectory() as dir_:
            path = os.path.join(dir_, "state.json")
            intents: dict = {}
            collision.declare_intent(intents, intent("VED-1", ["crates/pi-rpc"]))
            with open(path, "w") as handle:
                json.dump({"intents": intents}, handle)

            with open(path) as handle:
                reloaded = json.load(handle)["intents"]
            self.assertIn("VED-1", reloaded)
            self.assertEqual(reloaded["VED-1"]["scope"], ["crates/pi-rpc"])
            self.assertEqual(reloaded["VED-1"]["session_id"], "sess-VED-1")

    def test_idempotent_redeclare_updates_in_place(self):
        # T2: idempotent re-declare.
        intents: dict = {}
        collision.declare_intent(intents, intent("VED-1", ["a"]))
        collision.declare_intent(intents, intent("VED-1", ["a", "b"]))
        self.assertEqual(len(intents), 1)
        self.assertEqual(intents["VED-1"]["scope"], ["a", "b"])

    def test_declare_for_terminal_card_is_rejected(self):
        # AC3.
        with self.assertRaises(ValueError):
            collision.declare_intent(
                {}, intent("VED-1", ["a"]), terminal_issues={"VED-1"}
            )

    def test_declare_requires_an_issue(self):
        with self.assertRaises(ValueError):
            collision.declare_intent({}, intent("", ["a"]))

    def test_record_captures_branch_and_worktree(self):
        # AC4.
        intents: dict = {}
        collision.declare_intent(
            intents,
            intent("VED-1", ["a"], branch="swarm/VED-1", worktree="/ws/VED-1"),
        )
        self.assertEqual(intents["VED-1"]["branch"], "swarm/VED-1")
        self.assertEqual(intents["VED-1"]["worktree"], "/ws/VED-1")

    def test_release_drops_the_intent(self):
        intents: dict = {}
        collision.declare_intent(intents, intent("VED-1", ["a"]))
        self.assertTrue(collision.release_intent(intents, "VED-1"))
        self.assertNotIn("VED-1", intents)
        self.assertFalse(collision.release_intent(intents, "VED-1"))


class DetectionTests(unittest.TestCase):
    def test_exact_file_overlap_blocks_later_edit(self):
        # T3.
        a = intent("VED-A", ["crates/pi-rpc/src/lib.rs"], declared_at=1)
        b = intent("VED-B", ["crates/pi-rpc/src/lib.rs"], declared_at=2)
        finding = collision.detect_overlap(a, b)
        self.assertIsNotNone(finding)
        self.assertTrue(finding["blocking"])
        self.assertEqual(finding["kind"], "file")
        self.assertIn("crates/pi-rpc/src/lib.rs", finding["intersection"])

    def test_directory_prefix_overlap_blocks(self):
        # T4.
        a = intent("VED-A", ["crates/pi-agent"], declared_at=1)
        b = intent("VED-B", ["crates/pi-agent/src/lib.rs"], declared_at=2)
        finding = collision.detect_overlap(a, b)
        self.assertTrue(finding["blocking"])
        self.assertIn("crates/pi-agent/src/lib.rs", finding["intersection"])

    def test_disjoint_scopes_do_not_overlap(self):
        # T5.
        a = intent("VED-A", ["crates/pi-rpc"])
        b = intent("VED-B", ["crates/pi-host"])
        self.assertIsNone(collision.detect_overlap(a, b))

    def test_read_only_op_never_blocks(self):
        # T6.
        a = intent("VED-A", ["crates/pi-rpc/src/lib.rs"], declared_at=1)
        b = intent(
            "VED-B", ["crates/pi-rpc/src/lib.rs"], operation="review", declared_at=2
        )
        finding = collision.detect_overlap(a, b)
        self.assertIsNotNone(finding)
        self.assertFalse(finding["blocking"])
        self.assertEqual(finding["kind"], "informational")

    def test_empty_scope_is_fail_safe(self):
        # T8.
        a = intent("VED-A", [], declared_at=1)
        b = intent("VED-B", ["crates/pi-rpc/src/lib.rs"], declared_at=2)
        finding = collision.detect_overlap(a, b)
        self.assertTrue(finding["blocking"])
        self.assertEqual(finding["kind"], "unknown-scope")

    def test_symbol_overlap_blocks(self):
        # T9.
        a = intent("VED-A", ["crates/pi-rpc"], symbols=["SessionState::switch"])
        b = intent("VED-B", ["crates/pi-host"], symbols=["SessionState::switch"])
        finding = collision.detect_overlap(a, b)
        self.assertTrue(finding["blocking"])
        self.assertEqual(finding["kind"], "symbol")
        self.assertEqual(finding["intersection"], ["SessionState::switch"])

    def test_detection_is_symmetric_and_deterministic(self):
        # T10.
        a = intent("VED-A", ["crates/pi-rpc/src/lib.rs"], declared_at=1)
        b = intent("VED-B", ["crates/pi-rpc/src/lib.rs"], declared_at=2)
        self.assertEqual(collision.detect_overlap(a, b), collision.detect_overlap(b, a))
        self.assertEqual(collision.detect_all([a, b]), collision.detect_all([b, a]))

    def test_same_issue_never_collides_with_itself(self):
        a = intent("VED-A", ["crates/pi-rpc"])
        self.assertIsNone(collision.detect_overlap(a, dict(a)))

    def test_glob_scope_matches(self):
        a = intent("VED-A", ["crates/pi-rpc/src/*.rs"], declared_at=1)
        b = intent("VED-B", ["crates/pi-rpc/src/lib.rs"], declared_at=2)
        finding = collision.detect_overlap(a, b)
        self.assertTrue(finding["blocking"])

    def test_many_non_overlapping_scopes_have_no_false_positives(self):
        # T16.
        intents = [
            intent(f"VED-{i}", [f"crates/crate{i}/src/lib.rs"]) for i in range(50)
        ]
        self.assertEqual(collision.detect_all(intents), [])

    def test_terminal_card_finding_is_ignored_by_resolver(self):
        # T7 (detection side): a terminal issue is filtered in resolve_collisions.
        a = intent("VED-A", ["crates/pi-rpc"])
        b = intent("VED-B", ["crates/pi-rpc"])
        decisions = collision.resolve_collisions([a, b], terminal_issues={"VED-A"})
        self.assertNotIn("VED-A", decisions)
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_ALLOW)


class ResolutionTests(unittest.TestCase):
    def test_later_edit_is_blocked_with_reason_and_blocker(self):
        # AC9.
        a = intent("VED-A", ["crates/pi-rpc/src/lib.rs"], declared_at=1)
        b = intent("VED-B", ["crates/pi-rpc/src/lib.rs"], declared_at=2)
        decisions = collision.resolve_collisions([a, b])
        self.assertEqual(decisions["VED-A"]["decision"], collision.DECISION_ALLOW)
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_BLOCK)
        self.assertEqual(decisions["VED-B"]["blocked_by"], "VED-A")
        self.assertIn("crates/pi-rpc/src/lib.rs", decisions["VED-B"]["reason"])

    def test_tiebreak_by_issue_when_timestamps_equal(self):
        a = intent("VED-A", ["x"], declared_at=5)
        b = intent("VED-B", ["x"], declared_at=5)
        decisions = collision.resolve_collisions([a, b])
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_BLOCK)
        self.assertEqual(decisions["VED-A"]["decision"], collision.DECISION_ALLOW)

    def test_terminal_blocker_releases_the_blocked_card(self):
        # T7/T12: A terminal, B now allowed.
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        decisions = collision.resolve_collisions([a, b], terminal_issues={"VED-A"})
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_ALLOW)

    def test_reconcile_unblocks_after_release(self):
        # T12: release A's intent -> B is allowed by a later resolve pass.
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        blocked = collision.resolve_collisions([a, b])
        self.assertEqual(blocked["VED-B"]["decision"], collision.DECISION_BLOCK)
        unblocked = collision.resolve_collisions([b])
        self.assertEqual(unblocked["VED-B"]["decision"], collision.DECISION_ALLOW)

    def test_read_only_never_produces_a_block(self):
        # T6 (resolution side).
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], operation="test", declared_at=2)
        decisions = collision.resolve_collisions([a, b])
        self.assertEqual(decisions["VED-A"]["decision"], collision.DECISION_ALLOW)
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_ALLOW)

    def test_unknown_scope_warns_and_requires_resolution(self):
        # AC12.
        a = intent("VED-A", [], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        decisions = collision.resolve_collisions([a, b])
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_WARN)
        self.assertIn("explicit resolution", decisions["VED-B"]["reason"])

    def test_force_override_allows_and_records_reason(self):
        # AC13/T13.
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        decisions = collision.resolve_collisions([a, b], force_issues={"VED-B"})
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_ALLOW)
        self.assertTrue(decisions["VED-B"]["forced"])
        self.assertIn("forced past VED-A", decisions["VED-B"]["reason"])

    def test_three_way_overlap_blocks_the_two_later(self):
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        c = intent("VED-C", ["x"], declared_at=3)
        decisions = collision.resolve_collisions([a, b, c])
        self.assertEqual(decisions["VED-A"]["decision"], collision.DECISION_ALLOW)
        self.assertEqual(decisions["VED-B"]["decision"], collision.DECISION_BLOCK)
        self.assertEqual(decisions["VED-C"]["decision"], collision.DECISION_BLOCK)

    def test_resolution_is_deterministic_for_any_input_order(self):
        a = intent("VED-A", ["x"], declared_at=1)
        b = intent("VED-B", ["x"], declared_at=2)
        self.assertEqual(
            collision.resolve_collisions([a, b]),
            collision.resolve_collisions([b, a]),
        )


if __name__ == "__main__":
    unittest.main()
