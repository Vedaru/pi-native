#!/usr/bin/env python3
"""Tests for the swarm merge queue + gate (VED-377).

Run with `python3 scripts/test_swarm_merge_queue.py`. Stdlib only. Covers the
pure rules (risk classification, FIFO/serial order, receipt binding, merge
eligibility, conflict recording, restart idempotency) and the gate receipt.
"""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, f"{name}.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


queue_mod = load("swarm_merge_queue")
gate = load("swarm_gate")


def make_entry(issue="VED-1", head="abc123", risk="low", status="queued", **extra):
    entry = {
        "issue": issue, "branch": f"swarm/{issue}", "head": head,
        "base": "base0", "risk": risk, "status": status, "attempts": 0,
        "receipt": None, "conflict": None, "approved_by": None,
        "merge_sha": None,
    }
    entry.update(extra)
    return entry


class FakeCompleted:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


class RiskTests(unittest.TestCase):
    def test_shared_gate_file_is_high(self):
        for path in ("scripts/swarm_conductor.py", "Cargo.toml", "Cargo.lock",
                     ".github/workflows/ci.yml", "docs/swarm/README.md"):
            self.assertEqual(queue_mod.classify_risk([path]), "high", path)

    def test_cross_cutting_crate_is_high(self):
        self.assertEqual(
            queue_mod.classify_risk(["crates/pi-agent/src/lib.rs"]), "high"
        )

    def test_diffstat_threshold_is_high(self):
        self.assertEqual(
            queue_mod.classify_risk(["crates/pi-tui/src/lib.rs"], files_changed=20), "high"
        )
        self.assertEqual(
            queue_mod.classify_risk(["crates/pi-tui/src/lib.rs"], loc_changed=500), "high"
        )

    def test_review_title_is_high(self):
        self.assertEqual(queue_mod.classify_risk([], title="Swarm audit: rpc shape"), "high")
        self.assertEqual(queue_mod.classify_risk([], title="Review last commits"), "high")

    def test_ordinary_crate_change_is_low(self):
        self.assertEqual(
            queue_mod.classify_risk(
                ["crates/pi-tui/src/lib.rs"], title="render image protocols",
                files_changed=3, loc_changed=60,
            ),
            "low",
        )


class OrderTests(unittest.TestCase):
    def test_fifo_order(self):
        q = {"entries": [], "seq": 0}
        queue_mod.enqueue(q, "VED-1", "h1", "b", "low", now=1)
        queue_mod.enqueue(q, "VED-2", "h2", "b", "low", now=2)
        queue_mod.enqueue(q, "VED-3", "h3", "b", "low", now=3)
        self.assertEqual(queue_mod.next_entry(q)["issue"], "VED-1")

    def test_serial_when_one_is_active(self):
        q = {"entries": [], "seq": 0}
        queue_mod.enqueue(q, "VED-1", "h1", "b", "low")
        queue_mod.enqueue(q, "VED-2", "h2", "b", "low")
        q["entries"][0]["status"] = queue_mod.STATUS_GATING
        self.assertIsNone(queue_mod.next_entry(q))

    def test_next_entry_skips_terminal(self):
        q = {"entries": [make_entry("VED-1", status="merged"), make_entry("VED-2", status="queued")], "seq": 2}
        self.assertEqual(queue_mod.next_entry(q)["issue"], "VED-2")

    def test_restart_idempotency_same_head(self):
        q = {"entries": [], "seq": 0}
        queue_mod.enqueue(q, "VED-1", "h1", "b", "low", now=1)
        queue_mod.enqueue(q, "VED-1", "h1", "b", "low", now=2)
        self.assertEqual(len(q["entries"]), 1)
        self.assertEqual(q["seq"], 1)

    def test_moved_head_replaces_receipt(self):
        q = {"entries": [], "seq": 0}
        entry = queue_mod.enqueue(q, "VED-1", "h1", "b", "low")
        entry["receipt"] = {"gate": "green", "sha": "h1"}
        queue_mod.enqueue(q, "VED-1", "h2", "b", "low")
        updated = queue_mod.find_entry(q, "VED-1")
        self.assertEqual(updated["head"], "h2")
        self.assertIsNone(updated["receipt"])
        self.assertEqual(updated["status"], queue_mod.STATUS_QUEUED)


class ReceiptBindingTests(unittest.TestCase):
    def test_green_receipt_on_head_allows_merge_for_low_risk(self):
        entry = make_entry(status="gated", receipt={"gate": "green", "sha": "abc123"})
        self.assertTrue(queue_mod.can_merge(entry))

    def test_receipt_for_a_different_head_is_rejected(self):
        entry = make_entry(head="abc123", status="gated",
                           receipt={"gate": "green", "sha": "other"})
        self.assertFalse(queue_mod.can_merge(entry))

    def test_red_receipt_is_rejected(self):
        entry = make_entry(status="gated", receipt={"gate": "red", "sha": "abc123"})
        self.assertFalse(queue_mod.can_merge(entry))

    def test_high_risk_without_approval_cannot_merge(self):
        entry = make_entry(risk="high", status="review_required",
                           receipt={"gate": "green", "sha": "abc123"})
        self.assertFalse(queue_mod.can_merge(entry))

    def test_high_risk_with_approval_can_merge(self):
        entry = make_entry(risk="high", status="gated", approved_by="reviewer",
                           receipt={"gate": "green", "sha": "abc123"})
        self.assertTrue(queue_mod.can_merge(entry))

    def test_bind_green_advances_and_red_fails(self):
        entry = make_entry(status="gating")
        queue_mod.bind_receipt(entry, {"gate": "green", "sha": "abc123"})
        self.assertEqual(entry["status"], queue_mod.STATUS_GATED)
        red = make_entry(status="gating")
        queue_mod.bind_receipt(red, {"gate": "red", "sha": "abc123"})
        self.assertEqual(red["status"], queue_mod.STATUS_FAILED)

    def test_bind_green_high_risk_requires_review(self):
        entry = make_entry(risk="high", status="gating")
        queue_mod.bind_receipt(entry, {"gate": "green", "sha": "abc123"})
        self.assertEqual(entry["status"], queue_mod.STATUS_REVIEW_REQUIRED)

    def test_approve_moves_review_required_to_gated(self):
        q = {"entries": [make_entry(risk="high", status="review_required",
                                    receipt={"gate": "green", "sha": "abc123"})], "seq": 1}
        queue_mod.approve_entry(q, "VED-1", "reviewer", now=5)
        self.assertEqual(q["entries"][0]["status"], queue_mod.STATUS_GATED)
        self.assertEqual(q["entries"][0]["approved_by"], "reviewer")


class ConflictTests(unittest.TestCase):
    def test_resolve_records_and_requeues(self):
        q = {"entries": [make_entry(status="conflict", conflict=["a.rs", "b.rs"])], "seq": 1}
        entry = queue_mod.resolve_entry(q, "VED-1", "took ours on a.rs", now=9)
        self.assertEqual(entry["status"], queue_mod.STATUS_QUEUED)
        self.assertEqual(entry["resolution"], "took ours on a.rs")
        self.assertEqual(entry["conflict"], ["a.rs", "b.rs"])


class GateTests(unittest.TestCase):
    def test_parse_test_counts_sums_lines(self):
        output = (
            "test result: ok. 12 passed; 0 failed; 0 ignored\n"
            "test result: ok. 5 passed; 0 failed; 0 ignored\n"
        )
        self.assertEqual(gate.parse_test_counts(output), (17, 0))

    def test_parse_test_counts_reports_failures(self):
        output = "test result: FAILED. 3 passed; 2 failed; 0 ignored\n"
        self.assertEqual(gate.parse_test_counts(output), (3, 2))

    def test_green_receipt_binds_to_head(self):
        calls = []

        def executor(command, cwd=None, capture_output=True, text=True, **kwargs):
            calls.append(command)
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return FakeCompleted(0, "deadbeef\n")
            if command[:2] == ["cargo", "fmt"]:
                return FakeCompleted(0)
            if command[0] == "cargo" and command[1] == "clippy":
                return FakeCompleted(0)
            if command[:2] == ["cargo", "test"]:
                return FakeCompleted(0, "test result: ok. 8 passed; 0 failed; 0 ignored\n")
            return FakeCompleted(0)

        receipt = gate.run_gate("/tmp/ws", executor=executor, now=1.0)
        self.assertEqual(receipt["gate"], "green")
        self.assertEqual(receipt["sha"], "deadbeef")
        self.assertEqual(receipt["test"], {"passed": 8, "failed": 0})

    def test_red_receipt_when_tests_fail(self):
        def executor(command, cwd=None, capture_output=True, text=True, **kwargs):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return FakeCompleted(0, "deadbeef\n")
            if command[0] == "cargo" and command[1] == "test":
                return FakeCompleted(1, "test result: FAILED. 1 passed; 1 failed; 0 ignored\n")
            return FakeCompleted(0)

        receipt = gate.run_gate("/tmp/ws", executor=executor)
        self.assertEqual(receipt["gate"], "red")
        self.assertEqual(receipt["test"], {"passed": 1, "failed": 1})


class PersistenceTests(unittest.TestCase):
    def test_queue_survives_a_restart(self):
        with tempfile.TemporaryDirectory() as dir_:
            path = os.path.join(dir_, "q.json")
            old = os.environ.get("SWARM_MERGE_QUEUE_PATH")
            os.environ["SWARM_MERGE_QUEUE_PATH"] = path
            try:
                q = queue_mod.load_queue()
                queue_mod.enqueue(q, "VED-1", "h1", "b", "low", now=1)
                queue_mod.save_queue(q)

                reloaded = queue_mod.load_queue()
                self.assertIn("VED-1", [e["issue"] for e in reloaded["entries"]])
                self.assertEqual(reloaded["seq"], 1)
            finally:
                if old is None:
                    os.environ.pop("SWARM_MERGE_QUEUE_PATH", None)
                else:
                    os.environ["SWARM_MERGE_QUEUE_PATH"] = old


class ProcessEntryTests(unittest.TestCase):
    def test_rebase_conflict_is_surfaced_and_recorded(self):
        import unittest.mock as mock

        entry = make_entry(status="queued")
        with mock.patch.object(queue_mod, "rebase_onto_master", return_value=(False, ["x.rs"])), \
             mock.patch.object(queue_mod, "comment_result"):
            queue_mod.process_entry(entry, executor=lambda *a, **k: FakeCompleted(0))
        self.assertEqual(entry["status"], queue_mod.STATUS_CONFLICT)
        self.assertEqual(entry["conflict"], ["x.rs"])

    def test_gate_step_binds_receipt_and_sets_head(self):
        import unittest.mock as mock

        entry = make_entry(status="gating")
        receipt = {"gate": "green", "sha": "newhead", "test": {"passed": 1, "failed": 0}}
        with mock.patch.object(queue_mod, "run_gate_in", return_value=receipt), \
             mock.patch.object(queue_mod, "comment_result"):
            queue_mod.process_entry(entry, executor=lambda *a, **k: FakeCompleted(0))
        self.assertEqual(entry["status"], queue_mod.STATUS_GATED)
        self.assertEqual(entry["head"], "newhead")

    def test_merge_flag_prevents_heavy_build_when_busy(self):
        import unittest.mock as mock

        entry = make_entry(status="queued")
        with mock.patch.object(queue_mod, "rebase_onto_master") as rebase:
            queue_mod.process_entry(entry, is_busy_fn=lambda: True)
        rebase.assert_not_called()
        self.assertEqual(entry["status"], queue_mod.STATUS_QUEUED)

    def test_merge_tears_down_and_records_sha(self):
        import unittest.mock as mock

        entry = make_entry(status="gated", receipt={"gate": "green", "sha": "abc123"})
        with mock.patch.object(queue_mod, "merge_to_master", return_value=(True, "mergesha")), \
             mock.patch.object(queue_mod, "teardown_worktree") as teardown, \
             mock.patch.object(queue_mod, "comment_result"):
            queue_mod.process_entry(entry, executor=lambda *a, **k: FakeCompleted(0))
        self.assertEqual(entry["status"], queue_mod.STATUS_MERGED)
        self.assertEqual(entry["merge_sha"], "mergesha")
        teardown.assert_called_once()


if __name__ == "__main__":
    unittest.main()
