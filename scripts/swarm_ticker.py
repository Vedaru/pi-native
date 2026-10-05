#!/usr/bin/env python3
"""Capacity tick for the swarm conductor.

The conductor is a normal pi-native unit, so it only acts when prompted. This
is **not** a timer: it ticks only while the swarm has spare capacity and work
to fill it — at least one role unit is idle *and* the Todo / In Progress
columns still hold cards. That is exactly when the conductor should hand the
next card to a free unit, and it stops the moment either side is satisfied.

Talking to the conductor in the web UI is unaffected: a tick waits behind
whatever the human sent and is withheld while every unit is busy.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
import urllib.request

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
CONDUCTOR_UNIT = os.environ.get("SWARM_CONDUCTOR_UNIT", "conductor")
PROJECT = "4d5e47500fa6"
TEAM = "VED"
POLL_SECONDS = 15
COOLDOWN_SECONDS = 60

# The units the conductor assigns work to. The conductor itself is never counted
# as capacity, or the tick would fire whenever it is idle.
ROLE_UNITS = {"planner", "researcher", "coder", "reviewer", "tester", "synthesizer"}

# Columns that hold dispatchable work.
PENDING_STATES = {"Todo", "In Progress"}

# A unit is busy only while it recently emitted an active event; `state` poll
# replies never count.
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


def units() -> list[dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=5) as response:
        return json.load(response)["units"]


def conductor() -> dict | None:
    for unit in units():
        if unit.get("unit") == CONDUCTOR_UNIT:
            return unit
    return None


def is_busy(unit: dict, now: float) -> bool:
    if unit.get("lastEvent") not in ACTIVE_EVENTS:
        return False
    return now - float(unit.get("lastEventAt") or 0) < BUSY_WINDOW_SECONDS


def idle_role_count(now: float) -> int:
    return sum(1 for unit in units() if unit.get("unit") in ROLE_UNITS and not is_busy(unit, now))


def pending_count() -> int:
    result = subprocess.run(
        ["linear", "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    count = 0
    for node in nodes:
        if "swarm status" in (node.get("title") or "").lower():
            continue
        if (node.get("state") or {}).get("name") in PENDING_STATES:
            count += 1
    return count


def tick(session_id: str) -> None:
    body = json.dumps({"type": "prompt", "text": "tick"}).encode()
    request = urllib.request.Request(
        f"{GATEWAY}/sessions/{session_id}/commands",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10):
        pass


def main() -> None:
    last_tick = 0.0
    print("ticker started (ticks while a role unit is idle and Todo/In Progress has cards)", flush=True)
    while True:
        try:
            now = time.time()
            unit = conductor()
            if unit is None:
                print("conductor unit not found", flush=True)
            else:
                idle = idle_role_count(now)
                pending = pending_count()
                if idle >= 1 and pending >= 1 and now - last_tick >= COOLDOWN_SECONDS:
                    tick(unit["sessionId"])
                    last_tick = now
                    print(f"tick -> {unit['sessionId']} ({idle} idle role unit(s), {pending} pending card(s))", flush=True)
        except Exception as error:
            print(f"ticker error: {error}", flush=True)
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    main()
