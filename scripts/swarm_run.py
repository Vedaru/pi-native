#!/usr/bin/env python3
"""Run an orchestration pattern across the swarm — the conductor's method.

This is the executable form of the topology runner: the conductor (or a human)
calls it instead of filling in a form. It drives the pipelets gateway directly,
so it has no dependency on the web app.

Patterns:
  concurrent  every selected unit gets the task at once
  sequential  units run in order, each fed the previous answer
  moa         units answer in parallel, then one synthesizes (mixture of agents)

Examples:
  scripts/swarm_run.py --list
  scripts/swarm_run.py --pattern concurrent --task "Summarise the repo layout"
  scripts/swarm_run.py --pattern sequential --units researcher,coder --task "Draft then implement X"
  scripts/swarm_run.py --pattern moa --task "Review the design" --aggregator synthesizer
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")

ACTIVE_EVENTS = {
    "agent_start",
    "turn_start",
    "message_start",
    "message_update",
    "assistant_delta",
    "thinking_delta",
    "tool_start",
    "tool_end",
}
BUSY_WINDOW_SECONDS = 120


def http_json(path: str) -> dict:
    with urllib.request.urlopen(f"{GATEWAY}{path}", timeout=10) as response:
        return json.load(response)


def post_command(session_id: str, text: str) -> None:
    body = json.dumps({"type": "prompt", "text": text}).encode()
    request = urllib.request.Request(
        f"{GATEWAY}/sessions/{session_id}/commands",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10):
        pass


def units_by_role() -> dict[str, dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=10) as response:
        payload = json.load(response)
    by_role: dict[str, dict] = {}
    for unit in payload["units"]:
        role = unit.get("unit") or unit.get("name")
        if role:
            by_role[role] = unit
    return by_role


def is_busy(unit: dict, now: float) -> bool:
    if unit.get("lastEvent") not in ACTIVE_EVENTS:
        return False
    return now - float(unit.get("lastEventAt") or 0) < BUSY_WINDOW_SECONDS


def latest_assistant_text(session_path: str) -> str:
    try:
        with open(session_path) as handle:
            lines = handle.readlines()
    except OSError:
        return ""
    text = ""
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        message = entry.get("message")
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        content = message.get("content")
        if not isinstance(content, list):
            continue
        joined = "".join(
            block.get("text", "")
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        )
        if joined.strip():
            text = joined
    return text


def wait_for_turn(role: str, baseline: str, timeout: float) -> tuple[str, bool]:
    """Poll until the unit is idle with a new answer, or the timeout passes."""
    deadline = time.time() + timeout
    saw_busy = False
    grace_start = time.time()
    while time.time() < deadline:
        by_role = units_by_role()
        unit = by_role.get(role)
        if unit is None:
            return "", False
        busy = is_busy(unit, time.time())
        output = latest_assistant_text(unit.get("sessionPath", ""))
        if busy:
            saw_busy = True
        if not busy and output != baseline:
            return output, False
        if not busy and saw_busy:
            return output, False
        if not busy and not saw_busy and time.time() - grace_start > 8:
            return output, False
        time.sleep(2)
    by_role = units_by_role()
    unit = by_role.get(role)
    output = latest_assistant_text(unit.get("sessionPath", "")) if unit else ""
    return output, True


def chain_prompt(task: str, previous: str) -> str:
    if not previous.strip():
        return task
    return f"{task}\n\n---\nOutput from the previous agent in the chain:\n\n{previous}"


def synthesis_prompt(task: str, answers: list[dict]) -> str:
    sections = "\n\n".join(f"## {a['role']}\n\n{a['output'] or '(no output)'}" for a in answers)
    return (
        "Several agents worked on the task below in parallel. Synthesize their answers into one "
        f"clear result, keeping anything only one of them caught.\n\n# Task\n\n{task}\n\n"
        f"# Agent answers\n\n{sections}"
    )


def run_pattern(pattern: str, task: str, roles: list[str], aggregator: str | None, timeout: float) -> dict:
    steps: list[dict] = []
    try:
        if pattern == "sequential":
            previous = ""
            for role in roles:
                baseline = latest_assistant_text(units_by_role()[role]["sessionPath"])
                post_command(units_by_role()[role]["sessionId"], chain_prompt(task, previous))
                output, timed_out = wait_for_turn(role, baseline, timeout)
                previous = output
                steps.append({"role": role, "status": "timeout" if timed_out else "done", "output": output})
        else:
            baselines = {}
            for role in roles:
                unit = units_by_role()[role]
                baselines[role] = latest_assistant_text(unit["sessionPath"])
                post_command(unit["sessionId"], task)
            outputs = []
            for role in roles:
                output, timed_out = wait_for_turn(role, baselines[role], timeout)
                steps.append({"role": role, "status": "timeout" if timed_out else "done", "output": output})
                outputs.append({"role": role, "output": output})
            if pattern == "moa":
                if not aggregator:
                    raise SystemExit("moa requires --aggregator")
                unit = units_by_role()[aggregator]
                baseline = latest_assistant_text(unit["sessionPath"])
                post_command(unit["sessionId"], synthesis_prompt(task, outputs))
                output, timed_out = wait_for_turn(aggregator, baseline, timeout)
                steps.append({"role": aggregator, "status": "timeout" if timed_out else "done", "output": output})
    except KeyError as error:
        raise SystemExit(f"unknown role: {error}") from error
    return {"pattern": pattern, "task": task, "steps": steps}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pattern", choices=["concurrent", "sequential", "moa"])
    parser.add_argument("--task")
    parser.add_argument("--units", help="comma-separated role names (default: every role)")
    parser.add_argument("--aggregator", help="role that synthesizes for --pattern moa")
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--list", action="store_true", help="list unit roles and exit")
    args = parser.parse_args()

    if args.list:
        for role, unit in sorted(units_by_role().items()):
            print(f"{role}\t{unit['sessionId']}\t{'busy' if is_busy(unit, time.time()) else 'idle'}")
        return 0

    if not args.pattern or not args.task:
        parser.error("--pattern and --task are required")

    available = units_by_role()
    roles = [r.strip() for r in args.units.split(",")] if args.units else sorted(available.keys())
    if args.pattern == "moa" and args.aggregator:
        roles = [r for r in roles if r != args.aggregator]

    result = run_pattern(args.pattern, args.task, roles, args.aggregator, args.timeout)

    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"# {result['pattern']}: {result['task']}\n")
        for step in result["steps"]:
            print(f"## {step['role']} [{step['status']}]\n\n{step['output'] or '(no output)'}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
