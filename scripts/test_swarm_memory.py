#!/usr/bin/env python3
"""Tests for the project-scoped swarm memory (VED-371).

Stdlib-only and importable without a running conductor. The acceptance criterion
is exercised directly: a decision recorded by one unit is visible to another
unit and to the conductor, and entries are attributable and timestamped.

Run with `python3 scripts/test_swarm_memory.py`.
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


memory = _load("swarm_memory", "swarm_memory.py")
conductor = _load("swarm_conductor", "swarm_conductor.py")


class MemoryStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, ".pi", "swarm-memory.jsonl")
        os.environ["SWARM_MEMORY_PATH"] = self.path

    def tearDown(self):
        os.environ.pop("SWARM_MEMORY_PATH", None)
        self.tmp.cleanup()

    def test_recorded_decision_is_visible_to_another_reader(self):
        # Acceptance: a decision recorded by one unit is visible to another.
        memory.record("decision", "Use JSONL, not SQLite.", author="coder", issue="VED-371",
                      now=1000)
        entries = memory.list_entries(path=self.path)
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0]["text"], "Use JSONL, not SQLite.")
        self.assertEqual(entries[0]["kind"], "decision")

    def test_entries_are_attributable_and_timestamped(self):
        memory.record("lesson", "Conductor must not block on memory.", author="reviewer",
                      issue="VED-371", now=1234)
        entry = memory.list_entries(path=self.path)[0]
        self.assertEqual(entry["author"], "reviewer")
        self.assertEqual(entry["ts"], 1234)
        self.assertEqual(entry["issue"], "VED-371")

    def test_all_typed_kinds_round_trip(self):
        for kind in memory.KINDS:
            memory.record(kind, f"a {kind} entry", author="tester", now=1)
        self.assertEqual(len(memory.list_entries(path=self.path)), len(memory.KINDS))
        for kind in memory.KINDS:
            self.assertTrue(memory.list_entries(kind=kind, path=self.path))

    def test_list_is_newest_first_and_limited(self):
        for i in range(5):
            memory.record("lesson", f"entry {i}", author="coder", now=100 + i)
        entries = memory.list_entries(limit=2, path=self.path)
        self.assertEqual([e["text"] for e in entries], ["entry 4", "entry 3"])

    def test_kind_filter_and_unknown_kind(self):
        memory.record("decision", "d", author="coder", now=1)
        memory.record("gotcha", "g", author="coder", now=2)
        gotchas = memory.list_entries(kind="gotcha", path=self.path)
        self.assertEqual([e["kind"] for e in gotchas], ["gotcha"])
        with self.assertRaises(ValueError):
            memory.record("rumour", "x", author="coder")
        with self.assertRaises(ValueError):
            memory.record("decision", "   ", author="coder")

    def test_corrupt_lines_are_skipped_not_fatal(self):
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        with open(self.path, "w", encoding="utf-8") as handle:
            handle.write("not json\n")
            handle.write(json.dumps(memory.make_entry("decision", "good", author="coder", now=1)) + "\n")
            handle.write("\n")
        self.assertEqual([e["text"] for e in memory.list_entries(path=self.path)], ["good"])

    def test_missing_store_reads_empty(self):
        self.assertEqual(memory.list_entries(path=self.path), [])
        self.assertEqual(memory.context_block(path=self.path), "")

    def test_context_block_includes_provenance(self):
        memory.record("constraint", "Provider parity is byte-exact.", author="coder",
                      issue="VED-315", now=42)
        block = memory.context_block(path=self.path)
        self.assertIn("<shared_memory>", block)
        self.assertIn("Provider parity is byte-exact.", block)
        self.assertIn("coder", block)
        self.assertIn("VED-315", block)


class McpTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "swarm-memory.jsonl")
        os.environ["SWARM_MEMORY_PATH"] = self.path

    def tearDown(self):
        os.environ.pop("SWARM_MEMORY_PATH", None)
        self.tmp.cleanup()

    def test_initialize_and_tools_list(self):
        init = memory.mcp_handle({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        self.assertEqual(init["result"]["serverInfo"]["name"], "swarm-memory")
        tools = memory.mcp_handle({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        names = {tool["name"] for tool in tools["result"]["tools"]}
        self.assertEqual(names, {"memory_record", "memory_list"})

    def test_record_then_list_over_mcp(self):
        # Acceptance over MCP: write as one client, read as another.
        written = memory.mcp_handle({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "memory_record",
                       "arguments": {"kind": "decision", "text": "Ship it",
                                     "author": "coder", "issue": "VED-371"}},
        })
        self.assertFalse(written.get("isError"))
        entry = json.loads(written["result"]["content"][0]["text"])
        self.assertEqual(entry["author"], "coder")

        read = memory.mcp_handle({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "memory_list", "arguments": {"limit": 5}},
        })
        entries = json.loads(read["result"]["content"][0]["text"])
        self.assertEqual([e["text"] for e in entries], ["Ship it"])

    def test_bad_kind_is_an_mcp_error_result(self):
        result = memory.mcp_call("memory_record", {"kind": "rumour", "text": "x"})
        self.assertTrue(result["isError"])

    def test_unknown_method_and_notification(self):
        unknown = memory.mcp_handle({"jsonrpc": "2.0", "id": 9, "method": "nope"})
        self.assertEqual(unknown["error"]["code"], -32601)
        self.assertIsNone(memory.mcp_handle({
            "jsonrpc": "2.0", "method": "notifications/initialized"}))

    def test_stdio_server_round_trip(self):
        stdin = io.StringIO(
            json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "memory_record",
                                   "arguments": {"kind": "lesson", "text": "stdio works",
                                                 "author": "tester"}}}) + "\n"
        )
        stdout = io.StringIO()
        memory.run_mcp(stdin=stdin, stdout=stdout)
        response = json.loads(stdout.getvalue().strip())
        self.assertEqual(response["id"], 1)


class ConductorInjectionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "swarm-memory.jsonl")
        os.environ["SWARM_MEMORY_PATH"] = self.path

    def tearDown(self):
        os.environ.pop("SWARM_MEMORY_PATH", None)
        self.tmp.cleanup()

    def test_memory_is_injected_into_write_prompts(self):
        # Acceptance: the conductor sees a decision recorded by a unit.
        memory.record("decision", "Conductor injects shared memory.", author="coder",
                      issue="VED-371", now=7)
        prompt = conductor.assignment_prompt("VED-2", "fix it", "coder")
        self.assertIn("<shared_memory>", prompt)
        self.assertIn("Conductor injects shared memory.", prompt)
        self.assertIn("[coder]", prompt)

    def test_memory_is_injected_into_read_only_prompts(self):
        memory.record("constraint", "Reviewer stays read-only.", author="synthesizer", now=8)
        prompt = conductor.review_prompt("VED-3", "audit it", "reviewer")
        self.assertIn("<shared_memory>", prompt)
        self.assertIn("Reviewer stays read-only.", prompt)
        self.assertNotIn("Commit only the files", prompt)

    def test_prompts_stay_clean_without_memory(self):
        for prompt in (conductor.assignment_prompt("VED-4", "x", "coder"),
                       conductor.review_prompt("VED-4", "x", "reviewer")):
            self.assertNotIn("<shared_memory>", prompt)


if __name__ == "__main__":
    unittest.main()
