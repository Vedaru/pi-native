#!/usr/bin/env python3
"""Unit health watchdog: detect stalls and recover them (VED-376).

A stalled unit — one that holds an `In Progress` card but has emitted no active
event for a while — is invisible today: the conductor only dispatches when *no*
unit is busy, so a wedged unit silently stops the swarm. This watchdog pairs unit
liveness with the Linear state of the card each unit holds, and acts.

The classifier and the recovery planner are **pure functions** of
`(units, cards, claims, now, config)`. Nothing inside them reads the clock or the
network, so they are unit-testable without a gateway; the driver (`run_pass`)
is the only part that talks to the gateway/Linear/state.

Recovery policy, in order (see the module constants):

- a live-but-silent unit's card is **retried** with exponential backoff up to
  `MAX_RECOVERIES`;
- a card whose unit is **orphaned** (claim, no live session) or **dead** (worker
  thread crashed) is **reassigned** to an idle unit that may edit;
- when the budget is exhausted, or no capable unit is free, the card is
  **flagged under Needs you** (a signed Linear comment) and never retried again.

Read-only cards (`reviewer`/`planner`) are recovered exactly once and then
flagged, preserving the VED-360 one-shot semantics.

The liveness predicate is imported from `swarm_conductor` so the watchdog, the
conductor and the UI cannot disagree about who is busy.
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def load_conductor():
    """Import the conductor for its liveness constants and claim helpers."""
    spec = importlib.util.spec_from_file_location(
        "swarm_conductor", os.path.join(HERE, "swarm_conductor.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


conductor = load_conductor()

GATEWAY = conductor.GATEWAY
PROJECT = conductor.PROJECT
TEAM = conductor.TEAM
STATE_PATH = conductor.STATE_PATH
OPEN_STATES = conductor.OPEN_STATES
ROLE_UNITS = conductor.ROLE_UNITS
EDITING_ROLES = conductor.EDITING_ROLES
READ_ONLY_ROLES = conductor.READ_ONLY_ROLES
ACTIVE_EVENTS = conductor.ACTIVE_EVENTS
BUSY_WINDOW_SECONDS = conductor.BUSY_WINDOW_SECONDS

# A unit silent for this long while holding an active card is stalled. Long
# enough to clear a multi-minute `cargo build` that emits no event between
# `tool_start` and `tool_end`; disabled entirely at 0 (VED-376, AC18).
STALL_SECONDS = int(os.environ.get("SWARM_STALL_SECONDS", "600"))

# Retry/backoff, mirroring the conductor's retry budget. The delay before the
# n-th recovery is `RECOVERY_BASE_SECONDS * 2 ** (n - 1)`, capped.
MAX_RECOVERIES = int(os.environ.get("SWARM_MAX_RECOVERIES", "3"))
RECOVERY_BASE_SECONDS = int(os.environ.get("SWARM_RECOVERY_BASE_SECONDS", "60"))
RECOVERY_MAX_SECONDS = int(os.environ.get("SWARM_RECOVERY_MAX_SECONDS", "900"))

# Findings a card can carry, and the actions the planner can choose.
HEALTHY = "healthy"
STALLED = "stalled"
ORPHANED = "orphaned"
DEAD = "dead"
UNKNOWN = "unknown"

# States whose cards the watchdog watches. `In Review` is watched only to flag
# it under Needs you, never to auto-retry (the reviewer owns it).
WATCHED_STATES = {"In Progress", "In Review"}


def is_busy(unit: dict, now: float) -> bool:
    """A unit is busy while it recently emitted an active event (shared helper)."""
    return conductor.is_busy(unit, now)


def classify(unit: dict | None, claim: dict | None, state_name: str, now: float, config: dict):
    """Pure classifier for one card.

    `(unit, claim, state_name, now, config) -> (kind, silent_for | None)`.

    - `healthy`  — a live unit emitted an active event inside the busy window.
    - `stalled`  — a live unit has been silent at least `stall_seconds`.
    - `dead`     — the unit holds a command sender whose worker thread crashed
                   (`running` is false and it is not suspended).
    - `orphaned` — the card has a claim but no unit/session, or the unit is
                   suspended by the reaper mid-card.
    - `unknown`  — no usable timestamp (`lastEventAt == 0`/missing).
    - `healthy`  — clock skew: `lastEventAt` in the future.
    """
    if claim is None:
        # A card open with no claim at all (a human moved it) is orphaned.
        return ORPHANED, None

    stall_seconds = config["stall_seconds"]
    if unit is None:
        return ORPHANED, None

    # A crashed worker is recoverable regardless of timing.
    if unit.get("dead"):
        return DEAD, None
    # A suspended unit mid-card is orphaned-suspended: recoverable by wake.
    if unit.get("suspended"):
        return ORPHANED, None

    if is_busy(unit, now):
        return HEALTHY, 0.0

    dispatched_at = float(claim.get("at") or 0)
    last_event_at = unit.get("lastEventAt")
    if not last_event_at:
        # Never active: unknown, not "stalled since epoch" (AC5).
        base = dispatched_at
        if base <= 0:
            return UNKNOWN, None
        silent_for = now - base
    else:
        last_event_at = float(last_event_at)
        if last_event_at > now:
            # Clock skew: a future timestamp is healthy, never stalled.
            return HEALTHY, 0.0
        silent_for = now - max(dispatched_at, last_event_at)

    if silent_for >= stall_seconds:
        return STALLED, silent_for
    return HEALTHY, silent_for


def recovery_delay(attempt: int) -> int:
    """Exponential backoff before the `attempt`-th recovery (1-based), capped."""
    if attempt < 1:
        return 0
    return min(RECOVERY_BASE_SECONDS * (2 ** (attempt - 1)), RECOVERY_MAX_SECONDS)


def find_findings(units, cards, claims, now, config):
    """Pure detection pass.

    `units`  — `UnitInfo` list from `GET /swarm`.
    `cards`  — Linear issues `{identifier, title, state:{name,type}}`.
    `claims` — conductor state `{identifier: record}`.
    `now`    — injected epoch seconds.
    `config` — `{stall_seconds, ...}`.

    Returns one finding per watched card that is not healthy, ordered
    deterministically (duplicate-claim reports first, then issue id).
    """
    if config["stall_seconds"] <= 0:
        return []  # disabled

    by_role = {}
    by_session = {}
    for unit in units or []:
        if unit.get("unit"):
            by_role.setdefault(unit["unit"], unit)
        if unit.get("sessionId"):
            by_session[unit["sessionId"]] = unit

    # Detect a role unit claimed by two cards; report, don't guess.
    claimed_by = {}
    for identifier, claim in (claims or {}).items():
        role = (claim or {}).get("role")
        if role:
            claimed_by.setdefault(role, []).append(identifier)

    findings = []
    for card in cards or []:
        identifier = card.get("identifier") or ""
        state = (card.get("state") or {}).get("name")
        if state not in WATCHED_STATES:
            continue
        claim = (claims or {}).get(identifier)
        role = (claim or {}).get("role")
        session_id = ROLE_UNITS.get(role) if role else None
        unit = by_session.get(session_id) if session_id else None
        if unit is None and role:
            unit = by_role.get(role)

        if role and len(claimed_by.get(role, [])) > 1:
            findings.append(
                {
                    "issue": identifier,
                    "role": role,
                    "unit": unit.get("sessionId") if unit else None,
                    "kind": "duplicate-claim",
                    "silent_for": None,
                    "attempt": _attempt(claim),
                }
            )
            continue

        kind, silent_for = classify(unit, claim, state, now, config)
        if kind == HEALTHY:
            continue
        findings.append(
            {
                "issue": identifier,
                "title": card.get("title") or "",
                "role": role,
                "unit": (unit or {}).get("sessionId") or session_id,
                "kind": kind,
                "silent_for": silent_for,
                "attempt": _attempt(claim),
            }
        )

    findings.sort(key=lambda f: (f["kind"] != "duplicate-claim", f["issue"]))
    return findings


def _attempt(claim: dict | None) -> int:
    if not claim:
        return 1
    return int(claim.get("recoveries") or 0) + 1


def plan_actions(findings, units, claims, config):
    """Pure recovery planner.

    `(findings, units, claims, config) -> [action]`. An action is
    `{issue, kind, action, unit, delay, reason, at}` where `action` is one of
    `retry`, `reassign`, `restart`, `needs-you`, `reconcile` and `at` is the
    injected `now`.

    Deterministic: stalls are recovered oldest-first (lowest current attempt,
    then issue id), so one free unit is not raced by several cards.
    """
    now = config["now"]
    by_role = {}
    idle_by_role = {}
    for unit in units or []:
        if unit.get("unit"):
            by_role.setdefault(unit["unit"], unit)
        if unit.get("unit") and unit.get("running") and not unit.get("busy"):
            idle_by_role.setdefault(unit["unit"], unit)

    actions = []
    ordered = sorted(findings, key=lambda f: (f["attempt"], f["issue"]))
    for finding in ordered:
        issue = finding["issue"]
        claim = (claims or {}).get(issue) or {}
        kind = finding["kind"]
        role = finding["role"]

        if claim.get("terminal"):
            # Already flagged; nothing to do (no retry storm).
            continue

        # Reconcile: the card is no longer active -> release, never resurrect.
        if finding.get("reconciled"):
            actions.append(
                {**finding, "action": "reconcile", "reason": "card-left-board", "at": now}
            )
            continue

        read_only = role in READ_ONLY_ROLES
        recoveries = int(claim.get("recoveries") or 0)

        # Read-only cards get exactly one recovery, then are flagged (VED-360).
        if read_only and recoveries >= 1:
            actions.append(
                {**finding, "action": "needs-you", "reason": "read-only-exhausted", "at": now}
            )
            continue

        if recoveries >= config["max_recoveries"]:
            actions.append(
                {**finding, "action": "needs-you", "reason": "budget-exhausted", "at": now}
            )
            continue

        attempt = recoveries + 1
        delay = recovery_delay(attempt)
        last_recovery = float(claim.get("recovery_at") or 0)
        if last_recovery and now - last_recovery < delay:
            continue  # still inside the backoff window; wait

        if kind == DEAD:
            action = "restart"
        elif kind == ORPHANED:
            # Reassign to another idle editing unit if one exists.
            target = _idle_editing_unit(idle_by_role, role)
            if target is None:
                actions.append(
                    {**finding, "action": "needs-you", "reason": "no-capable-unit", "at": now}
                )
                continue
            action = "reassign"
            finding = {**finding, "target": target}
        else:
            action = "retry"

        actions.append(
            {**finding, "action": action, "delay": delay, "at": now, "attempt": attempt}
        )
    return actions


def _idle_editing_unit(idle_by_role: dict, exclude_role: str | None):
    """Pick a deterministic idle editing unit that is not the excluded role."""
    for role in sorted(EDITING_ROLES):
        if role == exclude_role:
            continue
        unit = idle_by_role.get(role)
        if unit is not None:
            return unit.get("sessionId")
    return None


# ---- driver (the only impure part) -----------------------------------------


def fetch_units() -> list[dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=5) as response:
        return json.load(response).get("units", [])


def fetch_cards() -> list[dict]:
    result = subprocess.run(
        ["linear", "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    return [
        {
            "identifier": node["identifier"],
            "title": node.get("title") or "",
            "state": {
                "name": (node.get("state") or {}).get("name"),
                "type": (node.get("state") or {}).get("type"),
            },
        }
        for node in nodes
    ]


def load_state() -> dict:
    try:
        with open(STATE_PATH) as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save_state(state: dict) -> None:
    with open(STATE_PATH, "w") as handle:
        json.dump(state, handle, indent=2)


def post_comment(identifier: str, finding: dict) -> None:
    body = (
        f"## Watchdog: unit looks {finding['kind']}\n\n"
        f"The card is claimed by `{finding.get('role')}` "
        f"(`{finding.get('unit')}`) but the watchdog found no progress"
        + (f" for {int(finding['silent_for'])}s" if finding.get("silent_for") else "")
        + f". Recovery attempts: {finding.get('attempt')}.\n\n"
        f"Reason: `{finding.get('reason', finding['kind'])}`. "
        "This card is surfaced under **Needs you** so it is not retried forever.\n\n"
        "- [watchdog]"
    )
    subprocess.run(
        ["linear", "issue", "comment", "add", identifier, "--body", body],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )


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


def recovery_prompt(identifier: str) -> str:
    return (
        f"Watchdog recovery: continue Linear issue {identifier}.\n"
        f"Your previous turn appears to have stopped making progress. Re-read the issue with "
        f"`linear issue view {identifier}`, resume the work, run `cargo fmt --all`, "
        f"`cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`, "
        f"then commit referencing {identifier} (do NOT push) and comment the result.\n"
        f"— [watchdog]"
    )


def apply_action(action: dict, state: dict) -> None:
    """Apply one planned action: dispatch/restart/flag and record the event."""
    issue = action["issue"]
    record = state.setdefault(issue, {"attempts": 0})
    now = action["at"]

    if action["action"] == "needs-you":
        record["terminal"] = True
        record["status"] = "needs-you"
        record["recovery_kind"] = action["kind"]
        post_comment(issue, action)
    elif action["action"] == "reconcile":
        record["terminal"] = True
        record["status"] = "reconciled"
        record["at"] = 0.0
    else:
        target = action.get("target") or ROLE_UNITS.get(action.get("role"))
        if target:
            # Write `dispatched_at`/`recovery_at` BEFORE sending, so a
            # just-restarted unit is not instantly re-flagged (AC15).
            record["recovery_at"] = now
            record["recoveries"] = int(record.get("recoveries") or 0) + 1
            record["recovery_kind"] = action["kind"]
            record["status"] = "recovering"
            save_state(state)
            dispatch(target, recovery_prompt(issue))
        else:
            action = {**action, "action": "needs-you", "reason": "no-capable-unit"}
            record["terminal"] = True
            record["status"] = "needs-you"
            post_comment(issue, action)

    # Durable, auditable record of every action (AC13).
    log = record.setdefault("watchdog_log", [])
    log.append(
        {
            "issue": issue,
            "unit": action.get("unit"),
            "kind": action["kind"],
            "silent_for": action.get("silent_for"),
            "attempt": action.get("attempt"),
            "action": action["action"],
            "reason": action.get("reason"),
            "at": now,
        }
    )
    del log[:-20]  # keep the log bounded
    save_state(state)


def run_pass(now: float | None = None) -> list[dict]:
    """One full OBSERVE -> CLASSIFY -> PLAN -> ACT pass (used by the ticker)."""
    now = time.time() if now is None else now
    config = {
        "stall_seconds": STALL_SECONDS,
        "max_recoveries": MAX_RECOVERIES,
        "now": now,
    }
    units = fetch_units()
    cards = fetch_cards()
    state = load_state()
    findings = find_findings(units, cards, state, now, config)
    actions = plan_actions(findings, units, state, config)
    for action in actions:
        apply_action(action, state)
    return actions


def main() -> None:
    while True:
        try:
            for action in run_pass():
                print(
                    f"{action['kind']:<16} {action['issue']:<9} -> {action['action']}",
                    flush=True,
                )
        except Exception as error:  # keep the watchdog alive across transients
            print(f"watchdog error: {error}", flush=True)
        time.sleep(30)


if __name__ == "__main__":
    main()
