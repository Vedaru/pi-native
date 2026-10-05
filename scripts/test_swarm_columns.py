#!/usr/bin/env python3
"""Tests for the read-time Kanban column projection (VED-374).

Run with `python3 scripts/test_swarm_columns.py`. Stdlib only, no gateway or
network: every test injects units, issues, claims, facts and `now` directly into
the pure projection.

Covers the planner's matrix C1–C20.
"""

from __future__ import annotations

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))

NOW = 1_000_000.0
BUSY_WINDOW = 90.0


def load_columns():
    spec = importlib.util.spec_from_file_location(
        "swarm_columns", os.path.join(HERE, "swarm_columns.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


columns_mod = load_columns()


def unit(role="coder", *, session_id=None, last_event="tool_start", age=5.0, running=True):
    return {
        "sessionId": session_id or columns_mod.ROLE_UNITS[role],
        "unit": role,
        "name": role,
        "running": running,
        "lastEvent": last_event,
        "lastEventAt": NOW - age if age is not None else 0,
    }


def issue(identifier, state_name, state_type, title="a card"):
    return {
        "identifier": identifier,
        "title": title,
        "state": {"name": state_name, "type": state_type},
        "assignee": None,
        "updatedAt": None,
        "url": None,
    }


def claim_for(identifier, role="coder"):
    return {identifier: {"attempts": 1, "role": role, "at": NOW - 10}}


def project(issues, units=(), claims=None, facts=None):
    return columns_mod.derive_columns(list(units), list(issues), claims or {}, facts or {}, NOW)


def column_of(columns, identifier):
    for column, cards in columns.items():
        for card in cards:
            if card["identifier"] == identifier:
                return column, card
    return None, None


class ColumnProjectionTests(unittest.TestCase):
    # C1: the same facts, only the Linear state changes, move the column.
    def test_linear_state_move_changes_column(self):
        todo = project([issue("VED-1", "Todo", "unstarted")], [unit()], claim_for("VED-1"))
        started = project(
            [issue("VED-1", "In Progress", "started")], [unit()], claim_for("VED-1")
        )
        self.assertEqual(column_of(todo, "VED-1")[0], "backlog")
        self.assertEqual(column_of(started, "VED-1")[0], "working")

    # C2: the projection is pure — same inputs, identical output, no writes.
    def test_projection_is_pure_and_repeatable(self):
        issues = [issue("VED-1", "In Progress", "started")]
        units = [unit()]
        claims = claim_for("VED-1")
        first = project(issues, units, claims)
        second = project(issues, units, claims)
        self.assertEqual(first, second)
        # The inputs are not mutated by the projection.
        self.assertEqual(issues[0]["state"]["name"], "In Progress")
        self.assertNotIn("column", issues[0])
        self.assertNotIn("column", units[0])

    # C3: busy In Progress card -> Working.
    def test_busy_in_progress_is_working(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")], [unit(age=5)], claim_for("VED-1")
        )
        self.assertEqual(column_of(columns, "VED-1")[0], "working")

    # C4 / C6: stalled unit and blocked fact both -> Needs you.
    def test_stalled_unit_is_needs_you(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")],
            [unit(age=10_000)],
            claim_for("VED-1"),
        )
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "needs_you")
        self.assertEqual(card["reason"], "stalled")

    def test_blocked_fact_is_needs_you(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")],
            [unit()],
            claim_for("VED-1"),
            {"VED-1": {"blocked": True}},
        )
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "needs_you")
        self.assertEqual(card["reason"], "blocked")

    # C5: a claim with no live unit is orphaned.
    def test_orphaned_claim_is_needs_you(self):
        columns = project([issue("VED-1", "In Progress", "started")], [], claim_for("VED-1"))
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "needs_you")
        self.assertEqual(card["reason"], "orphaned")

    # C7: a pending ui_request outranks Working.
    def test_pending_ui_request_is_needs_you(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")],
            [unit()],
            claim_for("VED-1"),
            {"VED-1": {"ui_request": True}},
        )
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "needs_you")
        self.assertEqual(card["reason"], "awaiting-input")

    # C8: state In Review -> In review even with no reviewer activity.
    def test_in_review_state_is_in_review(self):
        columns = project([issue("VED-1", "In Review", "started")], [unit()], claim_for("VED-1"))
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "in_review")
        self.assertEqual(card["reason"], "state")

    # C9 / C14 / C20: a reviewer card is In review, never Working.
    def test_reviewer_card_is_in_review_not_working(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")],
            [unit("reviewer")],
            claim_for("VED-1", "reviewer"),
        )
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "in_review")
        self.assertEqual(card["reason"], "reviewing")
        self.assertNotEqual(column, "working")

    # C10: Done + branch, no PR/CI fact -> Ready to merge with source=branch.
    def test_done_with_branch_is_ready_to_merge(self):
        columns = project([issue("VED-1", "Done", "completed")], [unit()], claim_for("VED-1"))
        column, card = column_of(columns, "VED-1")
        self.assertEqual(column, "ready_to_merge")
        self.assertEqual(card["reason"], "branch")

    # C11: no CI fact must never read as a green CI.
    def test_no_false_ci_green(self):
        columns = project([issue("VED-1", "Done", "completed")], [unit()], claim_for("VED-1"))
        card = column_of(columns, "VED-1")[1]
        self.assertNotIn(card["reason"], ("ci", "green", "success"))
        # With an explicit failing CI, the card is not Ready to merge.
        failing = project(
            [issue("VED-1", "Done", "completed")],
            [unit()],
            claim_for("VED-1"),
            {"VED-1": {"ci": "red"}},
        )
        self.assertEqual(column_of(failing, "VED-1")[0], "needs_you")

    # C12: Needs you wins over Working.
    def test_needs_you_precedes_working(self):
        columns = project(
            [issue("VED-1", "In Progress", "started")],
            [unit(age=5)],  # busy
            claim_for("VED-1"),
            {"VED-1": {"blocked": True}},
        )
        self.assertEqual(column_of(columns, "VED-1")[0], "needs_you")
        self.assertEqual(len(columns["working"]), 0)

    # C13: a card appears in exactly one column.
    def test_card_appears_once(self):
        issues = [
            issue("VED-1", "In Progress", "started"),
            issue("VED-2", "In Review", "started"),
            issue("VED-3", "Done", "completed"),
        ]
        columns = project(issues, [unit()], {"VED-1": {"role": "coder"}})
        self.assertTrue(columns_mod.is_consistent(columns))

    # C14: Canceled cards are excluded entirely.
    def test_canceled_excluded(self):
        columns = project([issue("VED-1", "Canceled", "canceled")], [unit()])
        self.assertIsNone(column_of(columns, "VED-1")[0])

    # C15: Todo with no claim -> Backlog, not Working.
    def test_todo_unclaimed_is_backlog(self):
        columns = project([issue("VED-1", "Todo", "unstarted")])
        self.assertEqual(column_of(columns, "VED-1")[0], "backlog")

    # C16: unknown / missing facts never panic and map to a defined column.
    def test_unknown_facts_are_safe(self):
        columns = project(
            [
                {"identifier": "VED-1", "title": "no state"},  # no state object
                issue("VED-2", "In Progress", "started"),
            ],
            [{"unit": "coder"}],  # unit with no sessionId/lastEvent
            {"VED-2": {"role": "coder"}},  # claim with no matching session
        )
        for column in columns.values():
            self.assertIsInstance(column, list)
        self.assertIsNotNone(column_of(columns, "VED-1")[0])

    # C17: deterministic ordering within a column.
    def test_deterministic_order(self):
        issues = [
            issue("VED-3", "In Progress", "started"),
            issue("VED-1", "In Progress", "started"),
            issue("VED-2", "In Progress", "started"),
        ]
        claims = {i["identifier"]: {"role": "coder"} for i in issues}
        columns = project(issues, [unit()], claims)
        order = [card["identifier"] for card in columns["working"]]
        self.assertEqual(order, ["VED-1", "VED-2", "VED-3"])

    # C19: every placement carries a reason from the shared vocabulary.
    def test_reason_vocabulary(self):
        issues = [
            issue("VED-1", "In Progress", "started"),
            issue("VED-2", "Done", "completed"),
            issue("VED-3", "In Review", "started"),
        ]
        columns = project(issues, [unit()], {"VED-1": {"role": "coder"}})
        for cards in columns.values():
            for card in cards:
                self.assertIn(card["reason"], columns_mod.REASONS)

    # C18: the projection must not require changing the `/swarm` payload.
    def test_column_order_and_contract(self):
        self.assertEqual(
            columns_mod.COLUMN_ORDER,
            ("working", "needs_you", "in_review", "ready_to_merge"),
        )
        columns = project([])
        self.assertEqual(set(columns), set((*columns_mod.COLUMN_ORDER, "backlog")))

    # Edge 10: a future lastEventAt (clock skew) reads as busy, not stalled.
    def test_clock_skew_future_event_is_busy(self):
        skewed = unit(age=-1000)  # lastEventAt in the future
        columns = project(
            [issue("VED-1", "In Progress", "started")], [skewed], claim_for("VED-1")
        )
        self.assertEqual(column_of(columns, "VED-1")[0], "working")

    # Edge 13: empty board -> all columns present and empty.
    def test_empty_board(self):
        columns = project([])
        for column in (*columns_mod.COLUMN_ORDER, "backlog"):
            self.assertEqual(columns[column], [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
