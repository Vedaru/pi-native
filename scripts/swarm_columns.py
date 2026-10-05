#!/usr/bin/env python3
"""Swarm board: derive Kanban columns at read time (VED-374).

The `/swarm` view shows raw unit cards. This module projects those units plus
the Linear board into four derived columns — **Working / Needs you / In review /
Ready to merge** — the way Agent Orchestrator and `openkanban` derive status from
facts at read time.

Nothing is stored. The projection is a pure function of `(units, issues, claims,
facts, now)`; the same facts always yield the same columns, and moving a card in
Linear changes only its column on the next read. This is the OBSERVE -> UPDATE ->
DERIVE pipeline: the caller observes (`GET /swarm` + Linear + conductor claims),
updates nothing, and derives the board.

Facts the swarm can supply today:

- **Units** (`GET /swarm`): `UnitInfo` with `sessionId`, `unit` (role), `name`,
  `lastEvent`, `lastEventAt`, `running`.
- **Issues** (Linear CLI): `identifier`, `title`, `state.name`, `state.type`,
  assignee, `updatedAt`, `url`.
- **Claims** (conductor state `/tmp/swarm-conductor-state.json`): per-issue
  `{attempts, role, title, at, status?, terminal?}`.
- **Optional facts** (may be absent): `blocked`, `ui_request`, `review_finding`,
  `pr`/`ci`. The projection degrades gracefully and never invents a claim it was
  not given (e.g. it will not report "CI green" when no CI fact exists).

The liveness predicate is shared with the conductor (`ACTIVE_EVENTS`,
`BUSY_WINDOW_SECONDS`, `ROLE_UNITS`, `OPEN_STATES` are imported from it) so the
UI and the recovery loop cannot disagree about who is busy.
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def load_conductor():
    """Import the conductor for its single-source-of-truth liveness constants."""
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
ACTIVE_EVENTS = conductor.ACTIVE_EVENTS
BUSY_WINDOW_SECONDS = conductor.BUSY_WINDOW_SECONDS

# The four active columns, left to right. A card in none of them is Backlog and
# is not part of the active board.
COLUMN_ORDER = ("working", "needs_you", "in_review", "ready_to_merge")
COLUMN_TITLES = {
    "working": "Working",
    "needs_you": "Needs you",
    "in_review": "In review",
    "ready_to_merge": "Ready to merge",
    "backlog": "Backlog",
}

# Reason vocabulary for a placement, shared with VED-376's recovery findings so
# the board can explain *why* a card is where it is.
REASONS = {
    "busy",
    "idle",
    "stalled",
    "orphaned",
    "blocked",
    "awaiting-input",
    "needs-review",
    "reviewing",
    "state",
    "branch",
    "duplicate-claim",
    "unclaimed",
    "unassigned",
}

# Linear state names that mean "a human must act", and the state types/names
# that mean review / finished / excluded. `state.type` is Linear's stable
# taxonomy (backlog|unstarted|started|completed|canceled); `state.name`
# refines it.
BLOCKED_STATE_NAMES = {"blocked", "needs input", "waiting"}
REVIEW_STATE_NAMES = {"in review", "review", "code review"}
DONE_STATE_NAMES = {"done", "completed"}


def is_busy(unit: dict, now: float) -> bool:
    """A unit is busy while it recently emitted an active event (shared with the
    conductor so liveness has one definition)."""
    if unit.get("lastEvent") not in ACTIVE_EVENTS:
        return False
    return now - float(unit.get("lastEventAt") or 0) < BUSY_WINDOW_SECONDS


def _state_type(issue: dict) -> str:
    return str(((issue.get("state") or {}).get("type")) or "").lower()


def _state_name(issue: dict) -> str:
    return str(((issue.get("state") or {}).get("name")) or "")


def _unit_for(issue: dict, claim: dict | None, units_by_role: dict, units_by_session: dict):
    """Resolve the unit a card belongs to, or `None` if it has no live unit.

    A card's claim records the role it was dispatched to; the role maps to a
    stable session id, and the live unit carries that id. Falling back to the
    role name keeps the join working when a session was reaped (the card then
    reads as orphaned).
    """
    if not claim:
        return None, None
    role = claim.get("role")
    session_id = ROLE_UNITS.get(role) if role else None
    unit = units_by_session.get(session_id) if session_id else None
    if unit is None and role:
        unit = units_by_role.get(role)
    return unit, session_id


def derive_column(issue: dict, claim: dict | None, unit: dict | None, facts: dict, now: float):
    """Pure classifier: `(issue, claim, unit, facts, now) -> (column, reason)`.

    Precedence is explicit and first-match-wins:
        Needs you > In review > Ready to merge > Working > Backlog
    Never reads the clock or the network; `now` is injected.
    """
    identifier = issue.get("identifier") or ""
    card_facts = facts.get(identifier, {}) if facts else {}
    state_name = _state_name(issue)
    state_type = _state_type(issue)
    lower_name = state_name.lower()

    # Canceled cards are excluded from the active board.
    if state_type == "canceled" or lower_name == "canceled":
        return None, "state"

    busy = bool(unit) and is_busy(unit, now)
    role = (unit or {}).get("unit") or (claim or {}).get("role")

    # --- Needs you: a human must act. Takes precedence over everything. ---
    if card_facts.get("blocked") or lower_name in BLOCKED_STATE_NAMES:
        return "needs_you", "blocked"
    if card_facts.get("ui_request"):
        return "needs_you", "awaiting-input"
    # A claim with no live unit is orphaned; a claimed unit that is not running
    # is stalled. Both need a human (cross-links VED-376).
    if claim and unit is None:
        return "needs_you", "orphaned"
    if unit is not None and not unit.get("running") and claim is not None:
        return "needs_you", "stalled"
    if unit is not None and unit.get("running") and not busy and claim is not None:
        # Running but silent past the busy window while its card is active.
        if state_type == "started" or lower_name in ("in progress",):
            return "needs_you", "stalled"

    # --- In review: state says review, a reviewer is auditing, or a finding. ---
    if lower_name in REVIEW_STATE_NAMES or card_facts.get("review_finding"):
        return "in_review", "state" if lower_name in REVIEW_STATE_NAMES else "needs-review"
    if role == "reviewer":
        return "in_review", "reviewing"

    # --- Ready to merge: work Done on its branch, nothing left to review. ---
    if state_type == "completed" or lower_name in DONE_STATE_NAMES:
        # PR/CI facts are absent in the swarm today; mark the source instead of
        # inventing a green CI. A CI fact, when present, gates the column.
        ci = card_facts.get("ci")
        if ci is not None and str(ci).lower() not in ("green", "passing", "success"):
            return "needs_you", "blocked"
        source = "ci" if ci is not None else "branch"
        return "ready_to_merge", source

    # --- Working: actively claimed and busy on an in-progress card. ---
    if busy and (state_type == "started" or lower_name in ("in progress",)):
        return "working", "busy"

    return "backlog", "unclaimed" if not claim else "idle"


def derive_columns(units, issues, claims, facts, now):
    """Project the board into columns. Pure; stores nothing.

    `units`   — list of `UnitInfo` from `GET /swarm`.
    `issues`  — list of Linear issues (the board).
    `claims`  — conductor state `{identifier: record}`.
    `facts`   — optional extra facts `{identifier: {...}}` (blocked, ui_request,
                pr, ci, review_finding); may be `{}`.
    `now`     — injected epoch seconds.

    Returns `{column_id: [card, ...]}` for every column in `COLUMN_ORDER` plus
    `backlog`. A card is placed in exactly one column. Ordering is deterministic
    (by identifier, then title) so the UI does not flicker.
    """
    claims = claims or {}
    facts = facts or {}
    units_by_role = {}
    units_by_session = {}
    for unit in units or []:
        if unit.get("unit"):
            units_by_role.setdefault(unit["unit"], unit)
        if unit.get("sessionId"):
            units_by_session[unit["sessionId"]] = unit

    columns = {column: [] for column in (*COLUMN_ORDER, "backlog")}
    for issue in issues or []:
        identifier = issue.get("identifier") or ""
        claim = claims.get(identifier)
        unit, session_id = _unit_for(issue, claim, units_by_role, units_by_session)
        column, reason = derive_column(issue, claim, unit, facts, now)
        if column is None:
            continue  # canceled / excluded
        if reason not in REASONS:  # keep the vocabulary honest
            reason = "state"
        columns[column].append(
            {
                "identifier": identifier,
                "title": issue.get("title") or "",
                "state": _state_name(issue),
                "stateType": _state_type(issue),
                "assignee": issue.get("assignee"),
                "url": issue.get("url"),
                "role": (unit or {}).get("unit") or (claim or {}).get("role"),
                "sessionId": (unit or {}).get("sessionId") or session_id,
                "busy": bool(unit) and is_busy(unit, now),
                "reason": reason,
            }
        )

    for cards in columns.values():
        cards.sort(key=lambda card: (card["identifier"] or "", card["title"]))
    return columns


def is_consistent(columns) -> bool:
    """Every card appears in exactly one column (an invariant the tests assert)."""
    seen = set()
    for column, cards in columns.items():
        if column == "backlog":
            continue
        for card in cards:
            key = card["identifier"]
            if key in seen:
                return False
            seen.add(key)
    return True


# ---- read-time drivers (OBSERVE) -------------------------------------------


def fetch_units() -> list[dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=5) as response:
        return json.load(response).get("units", [])


def fetch_issues() -> list[dict]:
    result = subprocess.run(
        ["linear", "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    issues = []
    for node in nodes:
        state = node.get("state") or {}
        issues.append(
            {
                "identifier": node["identifier"],
                "title": node.get("title") or "",
                "state": {"name": state.get("name"), "type": state.get("type")},
                "assignee": (node.get("assignee") or {}).get("name"),
                "updatedAt": node.get("updatedAt"),
                "url": node.get("url"),
            }
        )
    return issues


def load_claims() -> dict:
    try:
        with open(STATE_PATH) as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def main() -> None:
    import time

    columns = derive_columns(fetch_units(), fetch_issues(), load_claims(), {}, time.time())
    for column in (*COLUMN_ORDER, "backlog"):
        print(f"\n== {COLUMN_TITLES[column]} ==", flush=True)
        for card in columns[column]:
            busy = "*" if card["busy"] else " "
            print(
                f" {busy} {card['identifier']:<9} {card['state']:<12} "
                f"{card['reason']:<14} {card['title']}"
            )


if __name__ == "__main__":
    main()
