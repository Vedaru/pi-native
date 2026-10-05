#!/usr/bin/env python3
"""Swarm conductor: keep the role units working through the open Linear board.

A pi-native unit is request-driven: it runs one agent loop for a prompt and then
idles. Nothing schedules the *next* prompt, which is why the swarm "stops".
This conductor is that scheduler. It reads the open issues in the VED
"pi native runtime" project, maps each to a role unit, and dispatches one at a
time — only when no unit is busy, so `cargo test` and `git commit` never race.

It is intentionally deterministic (no LLM in the loop): mapping, claims, and
retries live here; the units do the actual work and report back through Linear.

Routing only sends write-intent work to units that are allowed to edit code
(per ONBOARDING scope). Read-only units (`reviewer`, `planner`) receive an
audit/verify prompt that must not commit, and a read-only dispatch is terminal
after one attempt so it cannot be retried forever (VED-360).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
import urllib.request

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
PROJECT = "4d5e47500fa6"
TEAM = "VED"
STATE_PATH = "/tmp/swarm-conductor-state.json"
POLL_SECONDS = 20
MAX_ATTEMPTS = 3
OPEN_STATES = {"Todo", "In Progress", "In Review"}

# Stable role-unit session ids (the trailing segment of the session file name).
ROLE_UNITS = {
    "planner": "18db8638ff4db8cc-0",
    "researcher": "18db863901307301-1",
    "coder": "18db863901959b2b-2",
    "reviewer": "18db863901dd6353-3",
    "tester": "18db86390222a993-4",
    "synthesizer": "18db8639026e00b8-5",
}

# Which roles may edit and commit. Every other role is read-only (ONBOARDING
# scope) and must only ever be sent an audit/verify prompt.
#
# ONBOARDING: coder (gateway/host/triggers), tester (tests and gates),
# synthesizer (docs) may edit; planner and researcher are findings-only and
# reviewer is review-only.
EDITING_ROLES = {"coder", "tester", "synthesizer"}
READ_ONLY_ROLES = set(ROLE_UNITS) - EDITING_ROLES

# A claim in one of these statuses is terminal: never re-dispatch it.
TERMINAL_STATUSES = {"done", "declined", "blocked", "dispatching", "dispatched_readonly"}

# A unit is busy while it recently emitted an active event. `state`/`response`
# are poll replies, not work, so they never mark a unit busy.
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
BUSY_WINDOW_SECONDS = 90
DISPATCH_COOLDOWN_SECONDS = 60


# Titles that ask for review work route to a read-only unit. Matched only as
# a leading verb ("audit …", "review …", "verify …") so a fix that merely
# mentions the reviewer or a "read-only" path is still routed as write work.
REVIEW_TITLE_RE = re.compile(
    r"^\s*(swarm\s+)?(audit|review|verify)\b", re.IGNORECASE
)


def role_for(title: str) -> str:
    """Route an issue to a role by keyword. The units share one workspace, so
    this only decides who owns the fix, not which files are safe.

    Write-intent issues must resolve to an [`EDITING_ROLES`] unit: a read-only
    role cannot implement or commit, so routing one here would loop forever
    (VED-360). Only titles that explicitly ask for a review/audit go to a
    read-only role.
    """
    t = title.lower()
    if REVIEW_TITLE_RE.search(title):
        return "reviewer"
    if any(word in t for word in ("plan", "priorit", "backlog", "status", "docs")):
        return "planner" if "plan" in t or "priorit" in t else "synthesizer"
    # Everything below is write work. `researcher` is findings-only, so provider/
    # cache/net fixes go to the coder unit, which can edit and commit.
    return "coder"


def open_issues() -> list[tuple[str, str]]:
    result = subprocess.run(
        ["linear", "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    issues = []
    for node in nodes:
        state = (node.get("state") or {}).get("name")
        title = node.get("title") or ""
        if state not in OPEN_STATES:
            continue
        if "swarm status" in title.lower():
            continue  # the synthesizer's running status tracker
        issues.append((node["identifier"], title))
    return issues


def swarm() -> dict[str, dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=5) as response:
        payload = json.load(response)
    return {unit["sessionId"]: unit for unit in payload["units"]}


def is_busy(unit: dict, now: float) -> bool:
    if unit.get("lastEvent") not in ACTIVE_EVENTS:
        return False
    return now - float(unit.get("lastEventAt") or 0) < BUSY_WINDOW_SECONDS


def dispatch(session_id: str, text: str) -> None:
    body = json.dumps({"type": "prompt", "text": text}).encode()
    request = urllib.request.Request(
        f"{GATEWAY}/sessions/{session_id}/commands",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10):
        pass


def load_state() -> dict:
    try:
        with open(STATE_PATH) as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save_state(state: dict) -> None:
    with open(STATE_PATH, "w") as handle:
        json.dump(state, handle, indent=2)


def assignment_prompt(identifier: str, title: str, role: str) -> str:
    """Prompt for a write-capable unit: implement, gate, commit, report."""
    return (
        f"Conductor assignment: work Linear issue {identifier}.\n"
        f"Title: {title}\n"
        f"Read it with `linear issue view {identifier}`. Set it In Progress, implement the fix, "
        f"then run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, "
        f"and `cargo test --workspace`. Commit only the files you changed with a message that "
        f"references {identifier}; do NOT push. Comment the result on {identifier} and set it Done "
        f"when the gate is green. Sign — [{role}]."
    )


def review_prompt(identifier: str, title: str, role: str) -> str:
    """Prompt for a read-only unit: audit, report, and stop.

    Never asks for a commit. The unit records its verdict on the issue; if the
    issue needs code changes it says so and the conductor routes it to a
    write-capable unit instead of retrying this read-only one.
    """
    return (
        f"Conductor assignment: audit Linear issue {identifier}.\n"
        f"Title: {title}\n"
        f"Read it with `linear issue view {identifier}`. You are the read-only {role} unit: "
        f"do NOT edit files, commit, or change the issue state. Verify the claim, inspect the "
        f"relevant code, and comment your findings on {identifier} (exact files and commands). "
        f"If a code change is required, say explicitly that it is out of your scope so the "
        f"conductor can route it to a write-capable unit. Sign — [{role}]."
    )


def prompt_for(identifier: str, title: str, role: str) -> str:
    if role in READ_ONLY_ROLES:
        return review_prompt(identifier, title, role)
    return assignment_prompt(identifier, title, role)


def main() -> None:
    state = load_state()
    last_dispatch = 0.0
    print("conductor started", flush=True)
    while True:
        try:
            now = time.time()
            units = swarm()
            busy = any(is_busy(unit, now) for unit in units.values())
            if not busy and now - last_dispatch >= DISPATCH_COOLDOWN_SECONDS:
                for identifier, title in open_issues():
                    record = state.setdefault(identifier, {"attempts": 0})
                    # A terminal verdict (done/declined/blocked, or an already-
                    # dispatched read-only audit) is never retried.
                    if record.get("terminal"):
                        continue
                    if record.get("status") in TERMINAL_STATUSES:
                        continue
                    if record["attempts"] >= MAX_ATTEMPTS:
                        continue
                    role = role_for(title)
                    session_id = ROLE_UNITS[role]
                    dispatch(session_id, prompt_for(identifier, title, role))
                    last_dispatch = now
                    record["attempts"] += 1
                    record["role"] = role
                    record["title"] = title
                    record["at"] = now
                    # A read-only unit is asked to audit, not to fix. Mark the
                    # claim terminal so it is dispatched exactly once instead of
                    # being retried until MAX_ATTEMPTS (VED-360).
                    if role in READ_ONLY_ROLES:
                        record["status"] = "dispatched_readonly"
                        record["terminal"] = True
                    else:
                        record["status"] = "dispatched"
                    save_state(state)
                    print(f"dispatched {identifier} -> {role} (attempt {record['attempts']})", flush=True)
                    break  # serial: one issue in flight at a time
        except Exception as error:  # keep the conductor alive across transient errors
            print(f"conductor error: {error}", flush=True)
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    main()
