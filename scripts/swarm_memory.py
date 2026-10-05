#!/usr/bin/env python3
"""Project-scoped swarm memory: typed, attributable, timestamped entries.

Units lose context between sessions, so decisions, constraints, and lessons get
re-derived — or contradicted — on every run. This module is the shared store
that fixes that (VED-371): one short typed entry per line (JSONL) under the
project root, written by any unit and read back by the conductor and every unit
prompt.

Entry kinds are deliberately few and closed:

- ``decision``   — a choice that was made, with its rationale.
- ``constraint`` — a boundary the work must respect (parity, scope, a gate).
- ``lesson``     — something learned the hard way.
- ``gotcha``     — a sharp edge that will bite again.

Each line is one JSON object::

    {"ts": 1759600000, "kind": "decision", "text": "...", "author": "coder", "issue": "VED-371"}

The store is append-only JSONL so concurrent writers never corrupt it and the
history is auditable (``ts`` + ``author`` + ``issue``). Reads are bounded and
newest-first for prompt injection.

CLI::

    swarm_memory.py record --kind decision --text "..." [--author X] [--issue VED-1]
    swarm_memory.py list [--kind K] [--limit N] [--json]
    swarm_memory.py context [--limit N]      # prompt-ready block
    swarm_memory.py mcp                      # stdio MCP server (tools: memory_record, memory_list)

The path defaults to ``<project>/.pi/swarm-memory.jsonl`` and can be overridden
with ``SWARM_MEMORY_PATH`` (used by the tests and by the conductor).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time

KINDS = ("decision", "constraint", "lesson", "gotcha")
DEFAULT_LIMIT = 20
MAX_TEXT = 2000

# Project root is the parent of this script's directory (scripts/..).
PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_PATH = os.path.join(PROJECT_ROOT, ".pi", "swarm-memory.jsonl")

_SLUG_RE = re.compile(r"\s+")


def memory_path() -> str:
    """Resolve the store path (``SWARM_MEMORY_PATH`` wins for tests/tools)."""
    return os.environ.get("SWARM_MEMORY_PATH") or DEFAULT_PATH


def normalize_kind(kind: str) -> str:
    """Return a canonical kind or raise for one we do not store."""
    value = (kind or "").strip().lower()
    if value not in KINDS:
        raise ValueError(f"unknown kind {kind!r} (expected one of {', '.join(KINDS)})")
    return value


def make_entry(kind: str, text: str, author: str = "unknown", issue: str | None = None,
               now: int | None = None) -> dict:
    """Build a validated entry. ``text`` is required and length-bounded."""
    kind = normalize_kind(kind)
    cleaned = _SLUG_RE.sub(" ", (text or "").strip())
    if not cleaned:
        raise ValueError("text is required")
    if len(cleaned) > MAX_TEXT:
        cleaned = cleaned[:MAX_TEXT]
    entry = {
        "ts": int(time.time() if now is None else now),
        "kind": kind,
        "text": cleaned,
        "author": (author or "unknown").strip() or "unknown",
    }
    if issue:
        entry["issue"] = issue.strip()
    return entry


def record(kind: str, text: str, author: str = "unknown", issue: str | None = None,
           path: str | None = None, now: int | None = None) -> dict:
    """Append one entry to the store and return it."""
    entry = make_entry(kind, text, author=author, issue=issue, now=now)
    target = path or memory_path()
    parent = os.path.dirname(target)
    if parent:
        os.makedirs(parent, exist_ok=True)
    line = json.dumps(entry, ensure_ascii=False, separators=(",", ":")) + "\n"
    # Append with O_APPEND so concurrent unit writers interleave whole lines.
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)
    return entry


def _iter_entries(path: str):
    """Yield parsed entries, skipping blank/corrupt lines (never fail a read)."""
    try:
        handle = open(path, "r", encoding="utf-8")
    except FileNotFoundError:
        return
    with handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(entry, dict) and entry.get("text"):
                yield entry


def list_entries(kind: str | None = None, limit: int = DEFAULT_LIMIT,
                 path: str | None = None) -> list[dict]:
    """Newest-first entries, optionally filtered by kind."""
    wanted = normalize_kind(kind) if kind else None
    entries = [e for e in _iter_entries(path or memory_path())
               if wanted is None or e.get("kind") == wanted]
    entries.sort(key=lambda e: e.get("ts", 0), reverse=True)
    if limit is not None and limit >= 0:
        entries = entries[:limit]
    return entries


def _stamp(ts: int) -> str:
    try:
        return time.strftime("%Y-%m-%d %H:%M", time.localtime(int(ts)))
    except (TypeError, ValueError, OSError):
        return "?"


def render_context(limit: int = DEFAULT_LIMIT, path: str | None = None) -> str:
    """Prompt-ready memory block, or an empty string when nothing is stored."""
    entries = list_entries(limit=limit, path=path)
    if not entries:
        return ""
    lines = [
        "Shared project memory (recorded by earlier units; do not contradict these "
        "without recording a new entry):"
    ]
    for entry in entries:
        issue = f" ({entry['issue']})" if entry.get("issue") else ""
        lines.append(
            f"- [{entry.get('kind')}] {entry.get('text')} "
            f"— {entry.get('author', 'unknown')}{issue}, {_stamp(entry.get('ts', 0))}"
        )
    lines.append(
        "Before finishing, record any new decision, constraint, lesson, or gotcha with "
        "`python3 scripts/swarm_memory.py record --kind <kind> --text \"...\"`."
    )
    return "\n".join(lines)


def context_block(limit: int = DEFAULT_LIMIT, path: str | None = None) -> str:
    """Wrap [`render_context`] in a stable tag for embedding in a prompt."""
    body = render_context(limit=limit, path=path)
    if not body:
        return ""
    return f"<shared_memory>\n{body}\n</shared_memory>"


# --- MCP server (stdio, newline-delimited JSON-RPC 2.0) ---------------------

MCP_PROTOCOL_VERSION = "2024-11-05"

MCP_TOOLS = [
    {
        "name": "memory_record",
        "description": "Record one project memory entry (decision, constraint, lesson, gotcha).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": list(KINDS)},
                "text": {"type": "string", "description": "The entry, one or two sentences."},
                "author": {"type": "string", "description": "Unit or agent recording it."},
                "issue": {"type": "string", "description": "Linear issue id, e.g. VED-371."},
            },
            "required": ["kind", "text"],
        },
    },
    {
        "name": "memory_list",
        "description": "List recent project memory entries, newest first.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": list(KINDS)},
                "limit": {"type": "integer", "minimum": 0, "default": DEFAULT_LIMIT},
            },
        },
    },
]


def mcp_call(name: str, arguments: dict) -> dict:
    """Execute one MCP tool call and return an MCP tool result."""
    arguments = arguments or {}
    if name == "memory_record":
        try:
            entry = record(
                arguments.get("kind", ""),
                arguments.get("text", ""),
                author=arguments.get("author", "unknown"),
                issue=arguments.get("issue"),
            )
        except ValueError as error:
            return {"content": [{"type": "text", "text": str(error)}], "isError": True}
        return {"content": [{"type": "text", "text": json.dumps(entry, ensure_ascii=False)}]}
    if name == "memory_list":
        try:
            limit = int(arguments.get("limit", DEFAULT_LIMIT))
        except (TypeError, ValueError):
            limit = DEFAULT_LIMIT
        try:
            entries = list_entries(kind=arguments.get("kind"), limit=limit)
        except ValueError as error:
            return {"content": [{"type": "text", "text": str(error)}], "isError": True}
        return {"content": [{"type": "text", "text": json.dumps(entries, ensure_ascii=False)}]}
    return {"content": [{"type": "text", "text": f"unknown tool {name!r}"}], "isError": True}


def _mcp_response(request_id, result=None, error=None) -> dict:
    payload = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        payload["error"] = error
    else:
        payload["result"] = result
    return payload


def mcp_handle(message: dict) -> dict | None:
    """Handle one JSON-RPC message; return a response dict or None for notifications."""
    method = message.get("method")
    request_id = message.get("id")
    if request_id is None and method and method.startswith("notifications/"):
        return None
    if method == "initialize":
        return _mcp_response(request_id, {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "swarm-memory", "version": "1"},
        })
    if method == "tools/list":
        return _mcp_response(request_id, {"tools": MCP_TOOLS})
    if method == "tools/call":
        params = message.get("params") or {}
        return _mcp_response(request_id, mcp_call(params.get("name", ""),
                                                  params.get("arguments") or {}))
    if method in ("ping",):
        return _mcp_response(request_id, {})
    if request_id is None:
        return None
    return _mcp_response(request_id, error={"code": -32601, "message": f"method not found: {method}"})


def run_mcp(stdin=None, stdout=None) -> None:
    """Serve MCP over stdio: one JSON-RPC message per line."""
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        response = mcp_handle(message)
        if response is not None:
            stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
            stdout.flush()


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Project-scoped swarm memory (VED-371)")
    sub = parser.add_subparsers(dest="command", required=True)

    record_cmd = sub.add_parser("record", help="append one memory entry")
    record_cmd.add_argument("--kind", required=True, choices=KINDS)
    record_cmd.add_argument("--text", required=True)
    record_cmd.add_argument("--author", default=os.environ.get("SWARM_UNIT", "unknown"))
    record_cmd.add_argument("--issue")
    record_cmd.add_argument("--json", action="store_true")

    list_cmd = sub.add_parser("list", help="list recent entries (newest first)")
    list_cmd.add_argument("--kind", choices=KINDS)
    list_cmd.add_argument("--limit", type=int, default=DEFAULT_LIMIT)
    list_cmd.add_argument("--json", action="store_true")

    context_cmd = sub.add_parser("context", help="print the prompt-ready memory block")
    context_cmd.add_argument("--limit", type=int, default=DEFAULT_LIMIT)

    sub.add_parser("mcp", help="run the stdio MCP server")

    args = parser.parse_args(argv)
    if args.command == "record":
        entry = record(args.kind, args.text, author=args.author, issue=args.issue)
        print(json.dumps(entry, ensure_ascii=False) if args.json else entry["ts"])
    elif args.command == "list":
        entries = list_entries(kind=args.kind, limit=args.limit)
        if args.json:
            print(json.dumps(entries, ensure_ascii=False, indent=2))
        else:
            for entry in entries:
                issue = f" ({entry['issue']})" if entry.get("issue") else ""
                print(f"[{_stamp(entry['ts'])}] {entry['kind']}: {entry['text']} "
                      f"— {entry.get('author', 'unknown')}{issue}")
    elif args.command == "context":
        block = context_block(limit=args.limit)
        if block:
            print(block)
    elif args.command == "mcp":
        run_mcp()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
