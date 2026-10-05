#!/usr/bin/env python3
"""Tests for the conductor task-DAG (VED-378).

Run with `python3 scripts/test_swarm_dag.py`. Stdlib only, no gateway/network:
the graph, validation, readiness, batching and derive-from-board functions are
pure and are exercised directly; `derive_deps_from_board` is fed a synthetic
board shaped like the `linear` CLI output.
"""

from __future__ import annotations

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_dag():
    spec = importlib.util.spec_from_file_location(
        "swarm_dag", os.path.join(HERE, "swarm_dag.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


dag = load_dag()


def node(identifier, *, state="Todo", deps=(), wave=0, pkey=None, role="coder"):
    return {
        "id": identifier,
        "key": identifier,
        "title": identifier,
        "role": role,
        "deps": list(deps),
        "state": state,
        "wave": wave,
        "parallel_key": pkey or role,
    }


def nodes_by_id(*items):
    return {n["id"]: n for n in items}


def goal(nodes, **kw):
    return {"goal": "ship it", "nodes": nodes, **kw}


class ValidationTests(unittest.TestCase):
    def test_valid_goal_normalises(self):
        g = dag.validate_goal(
            goal(
                [
                    {"key": "a", "title": "design X", "blocked_by": []},
                    {"key": "b", "title": "implement X", "blocked_by": ["a"]},
                ],
                defaults={"role": "coder", "max_parallel": 2},
            )
        )
        self.assertEqual([n["key"] for n in g["nodes"]], ["a", "b"])
        self.assertEqual(g["defaults"]["role"], "coder")

    def test_missing_objective_rejected(self):
        with self.assertRaises(dag.DagError):
            dag.validate_goal({"nodes": [{"key": "a", "title": "t"}]})

    def test_empty_nodes_rejected(self):
        with self.assertRaises(dag.DagError):
            dag.validate_goal({"goal": "g", "nodes": []})

    def test_duplicate_key_rejected(self):
        with self.assertRaises(dag.DagError):
            dag.validate_goal(
                goal(
                    [
                        {"key": "a", "title": "one"},
                        {"key": "a", "title": "two"},
                    ]
                )
            )

    def test_unknown_dep_key_rejected(self):
        with self.assertRaises(dag.DagError):
            dag.validate_goal(goal([{"key": "a", "title": "t", "blocked_by": ["missing"]}]))

    def test_missing_title_rejected(self):
        with self.assertRaises(dag.DagError):
            dag.validate_goal(goal([{"key": "a"}]))

    def test_cycle_rejected(self):
        # AC4: a cycle is refused, never dispatched.
        with self.assertRaises(dag.DagError) as caught:
            dag.validate_goal(
                goal(
                    [
                        {"key": "a", "title": "a", "blocked_by": ["b"]},
                        {"key": "b", "title": "b", "blocked_by": ["a"]},
                    ]
                )
            )
        self.assertIn("cycle", str(caught.exception).lower())

    def test_role_defaults_from_title(self):
        # role is recomputed with the conductor's role_for (single source of
        # truth). Only an explicit review/verify title is read-only; a bare
        # "audit …" title is write work (VED-365: the VED project titles its
        # fix issues "Swarm audit: <fix list>").
        g = dag.validate_goal(goal([{"key": "r", "title": "review the gateway"}]))
        self.assertIsNone(g["nodes"][0]["role"])
        role = dag.conductor.role_for(g["nodes"][0]["title"])
        self.assertEqual(role, "reviewer")
        self.assertIn(role, dag.conductor.READ_ONLY_ROLES)

    def test_audit_title_defaults_to_a_writer(self):
        # Regression for VED-365, which the DAG must agree with: "audit the
        # gateway" is implementation work and routes to a write-capable unit.
        role = dag.conductor.role_for("audit the gateway")
        self.assertEqual(role, "coder")
        self.assertIn(role, dag.conductor.EDITING_ROLES)


class WaveTests(unittest.TestCase):
    def test_waves_are_longest_path(self):
        # a -> b -> d, a -> c -> d : wave(d)=2, wave(b)=wave(c)=1.
        waves = dag.compute_waves({"a": [], "b": ["a"], "c": ["a"], "d": ["b", "c"]})
        self.assertEqual(waves, {"a": 0, "b": 1, "c": 1, "d": 2})

    def test_cycle_returns_none(self):
        self.assertIsNone(dag.compute_waves({"a": ["b"], "b": ["a"]}))

    def test_parallel_key_uses_shared_prefix(self):
        # AC2: same crate collides, disjoint crates do not.
        self.assertEqual(
            dag.parallel_key(["crates/pi-rpc/src/lib.rs"], "coder"), "crates/pi-rpc"
        )
        self.assertEqual(
            dag.parallel_key(["crates/pi-host/src/lib.rs"], "coder"), "crates/pi-host"
        )
        self.assertNotEqual(
            dag.parallel_key(["crates/pi-rpc/src/lib.rs"], "coder"),
            dag.parallel_key(["crates/pi-host/src/lib.rs"], "coder"),
        )

    def test_parallel_key_falls_back_to_role(self):
        self.assertEqual(dag.parallel_key([], "reviewer"), "reviewer")


class ReadinessTests(unittest.TestCase):
    def test_node_not_ready_until_deps_done(self):
        # AC1: b is blocked until a is Done.
        nodes = nodes_by_id(node("a"), node("b", deps=["a"]))
        self.assertEqual(dag.ready_ids(nodes), ["a"])
        nodes["a"]["state"] = "Done"
        self.assertEqual(dag.ready_ids(nodes), ["b"])

    def test_chain_never_ready_out_of_order(self):
        nodes = nodes_by_id(node("a"), node("b", deps=["a"]), node("c", deps=["b"]))
        self.assertEqual(dag.ready_ids(nodes), ["a"])
        nodes["a"]["state"] = "Done"
        self.assertEqual(dag.ready_ids(nodes), ["b"])
        nodes["b"]["state"] = "Done"
        self.assertEqual(dag.ready_ids(nodes), ["c"])

    def test_canceled_dep_does_not_unblock(self):
        # A canceled dep is not Done, so its dependent stays blocked (AC5).
        nodes = nodes_by_id(node("a", state="Canceled"), node("b", deps=["a"]))
        self.assertNotIn("b", dag.ready_ids(nodes))

    def test_failed_dependents_transitive(self):
        nodes = nodes_by_id(
            node("a", state="Canceled"),
            node("b", deps=["a"]),
            node("c", deps=["b"]),
            node("independent"),
        )
        blocked = dag.failed_dependents(nodes)
        self.assertEqual(blocked, {"b", "c"})
        self.assertNotIn("independent", blocked)


class BatchTests(unittest.TestCase):
    def test_disjoint_branches_run_together(self):
        # AC2: wave-0 nodes with disjoint keys dispatch in parallel.
        nodes = nodes_by_id(
            node("a", pkey="crates/pi-a", wave=0),
            node("b", pkey="crates/pi-b", wave=0),
            node("c", pkey="crates/pi-c", wave=0),
        )
        batch = dag.select_batch(nodes, {}, max_parallel=2, busy=False)
        self.assertEqual(len(batch), 2)

    def test_colliding_nodes_serialize(self):
        # AC2: two nodes on the same parallel_key never run together.
        nodes = nodes_by_id(
            node("a", pkey="crates/pi-rpc", wave=0),
            node("b", pkey="crates/pi-rpc", wave=0),
        )
        batch = dag.select_batch(nodes, {}, max_parallel=4, busy=False)
        self.assertEqual(len(batch), 1)

    def test_lowest_wave_first(self):
        nodes = nodes_by_id(
            node("later", wave=1, pkey="crates/pi-b"),
            node("first", wave=0, pkey="crates/pi-a"),
        )
        batch = dag.select_batch(nodes, {}, max_parallel=1, busy=False)
        self.assertEqual(batch, ["first"])

    def test_global_busy_serializes(self):
        nodes = nodes_by_id(node("a"), node("b", pkey="other"))
        self.assertEqual(dag.select_batch(nodes, {}, max_parallel=4, busy=True), [])

    def test_running_key_blocks_new_dispatch(self):
        nodes = nodes_by_id(node("a", pkey="crates/pi-rpc"))
        self.assertEqual(
            dag.select_batch(nodes, {"crates/pi-rpc": 1}, max_parallel=4, busy=False), []
        )

    def test_max_parallel_one_is_the_degenerate_sequential_dag(self):
        nodes = nodes_by_id(
            node("a", pkey="crates/pi-a", wave=0),
            node("b", pkey="crates/pi-b", wave=0),
        )
        self.assertEqual(len(dag.select_batch(nodes, {}, max_parallel=1, busy=False)), 1)


class DeriveTests(unittest.TestCase):
    def test_deps_read_from_inverse_blocks_relations(self):
        # Shape produced by `linear issue relation list --json`: the blocked
        # issue sees the blocker in inverseRelations as type "blocks".
        board = [
            {
                "identifier": "VED-2",
                "state": {"name": "Todo", "type": "unstarted"},
                "inverseRelations": {
                    "nodes": [
                        {"type": "blocks", "issue": {"identifier": "VED-1"}},
                    ]
                },
            },
            {
                "identifier": "VED-1",
                "state": {"name": "Done", "type": "completed"},
                "inverseRelations": {"nodes": []},
            },
        ]
        deps = dag.derive_deps_from_board(board)
        self.assertEqual(deps["VED-2"], ["VED-1"])
        self.assertEqual(deps["VED-1"], [])

    def test_rebuild_from_board_reflects_linear_state(self):
        # AC6: the cache rebuilds from the board alone.
        cached = {
            "goal": "g",
            "parent": None,
            "nodes": {
                "design": {"id": "VED-1", "key": "design", "deps": [], "state": "Todo"},
                "impl": {"id": "VED-2", "key": "impl", "deps": ["VED-1"], "state": "Todo"},
            },
            "edges": [["VED-2", "VED-1"]],
        }
        board = [
            {"identifier": "VED-1", "state": {"name": "Done"}, "inverseRelations": {"nodes": []}},
            {
                "identifier": "VED-2",
                "state": {"name": "In Progress"},
                "inverseRelations": {"nodes": [{"type": "blocks", "issue": {"identifier": "VED-1"}}]},
            },
        ]
        rebuilt = dag.rebuild_from_board(cached, board)
        self.assertEqual(rebuilt["nodes"]["design"]["state"], "Done")
        self.assertEqual(rebuilt["nodes"]["impl"]["state"], "In Progress")
        self.assertEqual(rebuilt["nodes"]["impl"]["deps"], ["VED-1"])
        self.assertEqual(rebuilt["nodes"]["impl"]["wave"], 1)
        # A node whose dep is Done becomes ready.
        self.assertIn("impl", dag.ready_ids(rebuilt["nodes"]))

    def test_rebuild_tolerates_unknown_relations(self):
        cached = {"goal": "g", "nodes": {"a": {"id": "VED-1", "key": "a", "deps": []}}}
        board = [{"identifier": "VED-1", "state": {"name": "Todo"}, "inverseRelations": {"nodes": []}}]
        rebuilt = dag.rebuild_from_board(cached, board)
        self.assertEqual(rebuilt["nodes"]["a"]["id"], "VED-1")


if __name__ == "__main__":
    unittest.main(verbosity=2)
