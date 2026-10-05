#!/usr/bin/env python3
"""Swarm conductor: planning/prioritisation over the open Linear board.

A pi-native unit is request-driven: it runs one agent loop for a prompt and then
idles. Nothing schedules the *next* prompt, which is why the swarm "stops".
The conductor reads the open issues in the VED "pi native runtime" project and
maps each to a role unit.

Dispatch is delegated to the deterministic `swarm_orchestrator` (VED-368): the
conductor decides *who* owns an issue (role routing + prompt), the orchestrator
owns *what runs* — a durable claim table, bounded concurrency, exponential
backoff, and reconciliation. No LLM is involved in either decision.

Routing only sends write-intent work to units that are allowed to edit code
(per ONBOARDING scope). Read-only units (`reviewer`, `planner`) receive an
audit/verify prompt that must not commit, and a read-only dispatch is terminal
after one attempt so it cannot be retried forever (VED-360).
"""

from __future__ import annotations

import json
import os
import re
import time
import urllib.request

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
POLL_SECONDS = 20

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


def abort(session_id: str) -> None:
    """Abort a unit's current run (used by reconcile when a card closes)."""
    if not session_id:
        return
    body = json.dumps({"type": "abort"}).encode()
    request = urllib.request.Request(
        f"{GATEWAY}/sessions/{session_id}/commands",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=10):
            pass
    except OSError:
        pass


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
    """Conductor loop.

    Planning/prioritisation stays here (role routing and prompts). Dispatch,
    claims, concurrency, retries, and reconciliation are delegated to the
    deterministic `swarm_orchestrator` (VED-368), which owns the claim table.
    """
    import swarm_orchestrator as orchestrator_module

    config = orchestrator_module.OrchestratorConfig(
        max_concurrent_agents=orchestrator_module.DEFAULT_MAX_CONCURRENT_AGENTS
    )
    orchestrator = orchestrator_module.Orchestrator.load(
        orchestrator_module.STATE_PATH, config
    )
    cleaned_up = False
    print("conductor started", flush=True)
    while True:
        try:
            board = orchestrator_module.fetch_board()
            active = {
                issue
                for issue, snap in board.items()
                if snap.get("stateType") in orchestrator_module.ACTIVE_STATE_TYPES
            }
            # Prune terminal cards once at startup, after the board is known.
            if not cleaned_up:
                decisions = orchestrator.startup_cleanup(active)
                for decision in decisions:
                    print(
                        f"{decision.action} {decision.issue} {decision.reason}", flush=True
                    )
                cleaned_up = True
            units = orchestrator_module.fetch_units()
            now = time.time()
            decisions = orchestrator.tick(
                board,
                now=now,
                is_busy=lambda sid: orchestrator_module.unit_is_busy(units.get(sid), now),
                route=role_for,
                session_for=lambda role: ROLE_UNITS[role],
                read_only_roles=READ_ONLY_ROLES,
            )
            for decision in decisions:
                if decision.action == "dispatch":
                    title = board.get(decision.issue, {}).get("title", "")
                    try:
                        dispatch(
                            decision.session_id,
                            prompt_for(decision.issue, title, decision.role),
                        )
                    except OSError as error:
                        orchestrator.record_failure(
                            decision.issue, time.time(), f"dispatch failed: {error}"
                        )
                elif decision.action == "stop":
                    abort(decision.session_id)
                print(
                    f"{decision.action} {decision.issue} {decision.reason}", flush=True
                )
            orchestrator.save(orchestrator_module.STATE_PATH)
        except Exception as error:  # keep the conductor alive across transient errors
            print(f"conductor error: {error}", flush=True)
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    main()
