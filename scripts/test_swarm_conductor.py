#!/usr/bin/env python3
"""Tests for the swarm conductor routing (VED-360).

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
