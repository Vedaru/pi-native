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
import subprocess
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
    # The independent verifier (VED-369). A dedicated session so it never shares
    # context with the owner: verification always runs in a fresh session.
    "verifier": "18db863902a10000-6",
}

# The unit that runs the completion gate. It is read-only and must be distinct
# from the owner, so the owner can never verify its own card.
VERIFIER_ROLE = "verifier"

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

# Lifecycle states used by the verification gate (VED-369). An owner reports
# completion by moving its card to IN_REVIEW, never straight to DONE. Only a
# stored verifier pass promotes IN_REVIEW -> DONE; a rejection goes back to
# IN_PROGRESS with the verifier's refutation attached.
IN_PROGRESS = "In Progress"
IN_REVIEW = "In Review"
DONE = "Done"
VERIFY_STATES = {IN_REVIEW}

# Verification verdicts stored on a claim (VED-369).
VERIFY_PASS = "pass"
VERIFY_FAIL = "fail"
# A verification dispatch is in flight; the owner must not be re-dispatched while
# a card is awaiting a verdict.
AWAITING_VERIFICATION = "awaiting_verification"


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


# --- Independent verification gate (VED-369) -------------------------------
#
# Every card must be verified by a unit that did not do the work. The owner
# reports completion by moving the card to `In Review`; the conductor dispatches
# the fresh [`VERIFIER_ROLE`] session, which re-runs the acceptance check,
# attempts to refute completion, and reports test counts before/after. The
# conductor stores the verdict and enforces the deterministic tamper guard: a
# net reduction in tests is always rejected, regardless of what the verifier
# claims.

# `cargo test`/`cargo nextest` print a trailing summary such as
# `test result: ok. 228 passed; 0 failed; 3 ignored`. Match the passed count.
TEST_RESULT_RE = re.compile(r"test result:\s*\w+\.\s*(\d+)\s+passed", re.IGNORECASE)


def parse_test_count(output: str) -> int | None:
    """Return the number of passing tests in cargo test output, or None.

    Multiple `test result:` lines (one per binary) are summed, so a workspace
    run reports the total across all test targets.
    """
    counts = [int(match) for match in TEST_RESULT_RE.findall(output or "")]
    if not counts:
        return None
    return sum(counts)


def tests_decreased(before: int | None, after: int | None) -> bool:
    """True when the after count is a net reduction against a known baseline.

    Unknown counts are not treated as a reduction: the tamper guard only fires
    on positive evidence that tests were removed.
    """
    if before is None or after is None:
        return False
    return after < before


def verification_verdict(
    acceptance_passed: bool,
    refuted: bool,
    tests_before: int | None,
    tests_after: int | None,
) -> tuple[str, list[str]]:
    """Decide whether a completion report passes the gate.

    A card passes only if the acceptance check passed, the verifier failed to
    refute completion, and there is no net test reduction. Returns the verdict
    and the human-readable reasons (empty on a pass).
    """
    reasons: list[str] = []
    if not acceptance_passed:
        reasons.append("acceptance check failed on a fresh run")
    if refuted:
        reasons.append("verifier refuted completion")
    if tests_decreased(tests_before, tests_after):
        reasons.append(
            f"net test reduction rejected: {tests_before} -> {tests_after}"
        )
    return (VERIFY_FAIL if reasons else VERIFY_PASS), reasons


def should_verify(state: str, record: dict) -> bool:
    """True when a card is awaiting independent verification.

    A card in `In Review` needs a verifier unless it already holds a stored
    pass (then it is Done) or a verification is already in flight.
    """
    if state not in VERIFY_STATES:
        return False
    if record.get("verification") == VERIFY_PASS:
        return False
    if record.get("status") == AWAITING_VERIFICATION:
        return False
    return True


def gate_decision(state: str, record: dict) -> str:
    """Resolve a stored verdict into the next lifecycle action.

    Returns one of:
      - `"done"`   — verifier passed, promote the card to Done,
      - `"reopen"` — verification failed, reopen with the refutation,
      - `"verify"` — dispatch the independent verifier,
      - `"skip"`   — nothing to do (e.g. a stored pass on an open card).
    """
    if record.get("verification") == VERIFY_PASS and state == IN_REVIEW:
        return "done"
    if record.get("verification") == VERIFY_FAIL:
        return "reopen"
    if should_verify(state, record):
        return "verify"
    return "skip"


def set_issue_state(identifier: str, state: str) -> None:
    """Move a Linear card to a workflow state (conductor-owned, VED-369)."""
    subprocess.run(
        ["linear", "issue", "update", identifier, "-s", state],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
        check=False,
    )


def record_verdict(record: dict, verdict: str, reasons: list[str]) -> None:
    """Store a verifier verdict and the refutation reasons on a claim."""
    record["verification"] = verdict
    record["refutation"] = reasons
    if verdict == VERIFY_PASS:
        # A stored pass is the only thing that authorises Done.
        record["status"] = "done"
        record["terminal"] = True
    else:
        # A failed verification reopens the card: clear the in-flight marker
        # and the terminal flag so the owner can be dispatched again with the
        # refutation attached.
        record["status"] = "reopened"
        record["terminal"] = False
        record.pop("verification_inflight", None)


# The verifier writes a machine-readable verdict block into its Linear comment.
# The conductor parses it to store the pass/fail rather than trusting a human
# summary. Required lines: `verifier: pass` (or fail) and `tests: N -> M`.
VERDICT_LINE_RE = re.compile(r"^\s*verifier:\s*(pass|fail)\s*$", re.IGNORECASE | re.MULTILINE)
TESTS_LINE_RE = re.compile(
    r"^\s*tests:\s*(\d+|\?)\s*->\s*(\d+|\?)\s*$", re.IGNORECASE | re.MULTILINE
)


def _count_or_none(token: str) -> int | None:
    return None if token.strip() == "?" else int(token)


def parse_verifier_report(text: str) -> tuple[str | None, int | None, int | None]:
    """Parse the verifier's structured verdict from a comment body.

    Returns `(verdict, tests_before, tests_after)`; the verdict is `pass`,
    `fail`, or None when the comment does not carry a valid verdict block.
    """
    verdict_match = VERDICT_LINE_RE.search(text or "")
    verdict = verdict_match.group(1).lower() if verdict_match else None
    before = after = None
    tests_match = TESTS_LINE_RE.search(text or "")
    if tests_match:
        before = _count_or_none(tests_match.group(1))
        after = _count_or_none(tests_match.group(2))
    return verdict, before, after


def issue_comments(identifier: str) -> list[str]:
    """Return the comment bodies on a Linear issue, oldest first."""
    result = subprocess.run(
        ["linear", "issue", "view", identifier, "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        return []
    nodes = ((payload or {}).get("comments") or {}).get("nodes") or []
    return [node.get("body") or "" for node in nodes]


def ingest_verifier_verdict(identifier: str, record: dict) -> None:
    """Store the newest verifier verdict found in the issue comments.

    The conductor re-applies the deterministic tamper guard to the reported
    test counts, so a verifier cannot pass a card that shrank its tests.
    """
    for body in reversed(issue_comments(identifier)):
        verdict, before, after = parse_verifier_report(body)
        if verdict is None:
            continue
        reasons: list[str] = []
        if tests_decreased(before, after):
            reasons.append(f"net test reduction rejected: {before} -> {after}")
        if verdict == VERIFY_FAIL:
            reasons.append("verifier refuted completion")
        if reasons:
            record_verdict(record, VERIFY_FAIL, reasons)
        else:
            record_verdict(record, VERIFY_PASS, [])
        return


# Fixed prompt appended when a card is reopened after a failed verification.
REOPEN_PROMPT = (
    "Verification of your previous completion FAILED. Re-read the refutation above, "
    "fix the card, and report completion again by moving it to In Review. Do NOT "
    "set it Done. A net reduction in tests is rejected automatically."
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
    """Prompt for a write-capable unit: implement, gate, report for verification.

    The owner never sets the card Done (VED-369): it moves the card to
    `In Review` so the conductor can dispatch a fresh verifier. Only a verifier
    pass promotes it to Done.
    """
    return (
        f"Conductor assignment: work Linear issue {identifier}.\n"
        f"Title: {title}\n"
        f"Read it with `linear issue view {identifier}`. Set it In Progress, implement the fix, "
        f"then run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, "
        f"and `cargo test --workspace`. Commit only the files you changed with a message that "
        f"references {identifier}; do NOT push. Comment the result on {identifier}, then move it "
        f"to In Review — do NOT set it Done: an independent verifier must re-run the acceptance "
        f"check first. Report the exact test count from `cargo test --workspace` in your comment. "
        f"Sign — [{role}]."
    )


def verifier_prompt(identifier: str, title: str) -> str:
    """Prompt for the independent verifier (fresh context, read-only).

    The verifier must re-run the card's acceptance check, actively try to refute
    the completion claim, and compare test counts before/after. It never edits,
    commits, or changes the issue state — it only reports a verdict, and the
    conductor applies the deterministic tamper guard.
    """
    return (
        f"Conductor verification: verify Linear issue {identifier}.\n"
        f"Title: {title}\n"
        f"You are the independent verifier in a FRESH session; you did not write this code. "
        f"Read the card with `linear issue view {identifier}` and inspect the owner's completion "
        f"comment and diff. Do not trust the summary; re-run the card's acceptance check "
        f"yourself. Then actively attempt to REFUTE the completion claim: look for missing "
        f"requirements, unexercised edge cases, and unverified claims. Count tests before and "
        f"after (`cargo test --workspace`) and report whether the net test count decreased. "
        f"Do NOT edit files, commit, push, or change the issue state. Comment on {identifier} "
        f"with these machine-readable lines exactly (the conductor parses them):\n"
        f"verifier: pass\n"
        f"tests: <before> -> <after>\n"
        f"Use `verifier: fail` if the acceptance check failed or you refuted completion; "
        f"use `?` for a test count you could not determine. Then add the acceptance check "
        f"you ran, your refutation attempt and its result, and any residual gaps. A net "
        f"test reduction is a fail. Sign — [verifier]."
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
    if role == VERIFIER_ROLE:
        return verifier_prompt(identifier, title)
    if role in READ_ONLY_ROLES:
        return review_prompt(identifier, title, role)
    return assignment_prompt(identifier, title, role)


def dag_gate() -> set[str] | None:
    """Issue identifiers that are not ready yet, when a task DAG is cached.

    When `$SWARM_DAG_PATH` holds a DAG (VED-378), the conductor dispatches in
    dependency order: an issue whose `blocked-by` deps are not all `Done` is
    skipped. Returns `None` when no DAG is present, so the flat loop is
    unchanged. The DAG module is imported lazily so a missing/broken cache can
    never stop the conductor.
    """
    try:
        import importlib.util

        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "swarm_dag.py")
        spec = importlib.util.spec_from_file_location("swarm_dag", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        dag = module.load_dag()
        if not dag:
            return None
        dag = module.rebuild_from_board(dag, module.load_board())
        module.save_dag(dag)
        ready = set(module.ready_ids(dag["nodes"]))
        blocked = {
            node["id"]
            for key, node in dag["nodes"].items()
            if key not in ready
        }
        return blocked
    except Exception as error:  # never let the DAG stop the flat conductor
        print(f"dag gate error: {error}", flush=True)
        return None


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
                blocked=dag_gate(),
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
