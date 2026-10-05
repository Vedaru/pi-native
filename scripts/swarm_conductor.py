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
audit/verify prompt that must not commit. A read-only dispatch is terminal by
default (VED-360), but the reviewer may post a machine-readable verdict asking
the conductor to route the issue to a write-capable unit; `tick()` honors that
verdict by re-routing the claim instead of dropping it (VED-365).

Before an editing card is dispatched it declares an **intent** — the files and
symbols it expects to touch — and the conductor checks it against the other
in-flight write intents. An overlapping intent is blocked and the collision is
recorded on both cards, so parallel runs cannot silently race onto the same
files (VED-375; detector in `scripts/swarm_collision.py`).

Code-editing cards run in the shared main tree, one editor at a time; the
orchestrator serialises edits so parallel runs cannot race.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time
import urllib.request


def _load_memory():
    """Load the sibling swarm memory module (VED-371) without a package."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "swarm_memory.py")
    spec = importlib.util.spec_from_file_location("swarm_memory", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module




def _load_collision():
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "swarm_collision.py")
    spec = importlib.util.spec_from_file_location("swarm_collision", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


memory = _load_memory()
collision = _load_collision()

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
POLL_SECONDS = 20

# Events that mean a unit is actively working, mirroring `swarm_orchestrator`
# (the single source of truth for liveness). `state`/`response` poll replies are
# never active; a busy unit emitted an active event inside the window.
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

# Long-lived units bloat and drift: a role unit that kept every card's
# transcript would send a growing, irrelevant context to the provider. By
# default the conductor clears a unit's context before each card so every card
# starts fresh (the unit keeps its session id). Set SWARM_FRESH_CONTEXT=0 to
# keep one growing transcript per unit instead.
FRESH_CONTEXT = os.environ.get("SWARM_FRESH_CONTEXT", "1") not in {"0", "false", "no"}

# A read-only unit that finds the issue needs code changes posts this marker in
# a Linear comment; the conductor then re-routes the claim to a write unit.
ROUTE_VERDICT_RE = re.compile(r"@conductor\s+route-to:\s*(\w+)", re.IGNORECASE)

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

# A claim in one of these statuses is terminal: never re-dispatch it. A
# read-only dispatch is terminal *unless* the reviewer posts a route verdict
# (see `pending_route`), so it is checked separately from the hard terminals.
# `collision_blocked` is deliberately absent: it is a *re-evaluable* hold,
# not a verdict, so the card dispatches once its blocker clears (VED-375).
TERMINAL_STATUSES = {"done", "declined", "blocked", "dispatching", "dispatched_readonly"}
HARD_TERMINAL_STATUSES = TERMINAL_STATUSES - {"dispatched_readonly"}

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

# The repo this conductor lives in: <repo>/scripts/swarm_conductor.py.
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def repo_root() -> str:
    """The repository root, resolved from the common git dir."""
    result = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode == 0 and result.stdout.strip():
        return os.path.dirname(result.stdout.strip())
    return REPO_ROOT



# Card states that mean the work is over.
TERMINAL_ISSUE_STATES = {"Done", "Canceled", "Cancelled"}


# Titles that ask for review-only work route to a read-only unit. Matched only
# as a leading verb ("review …", "verify …") so a fix that merely mentions the
# reviewer or a "read-only" path is still routed as write work.
#
# `audit` is deliberately *not* in this list: the VED project titles its write
# issues `Swarm audit: <fix list>`, so an audit prefix is implementation work
# (VED-365). An audit title is only read-only when it says so explicitly
# (read-only / report only).
REVIEW_TITLE_RE = re.compile(r"^\s*(swarm\s+)?(review|verify)\b", re.IGNORECASE)
AUDIT_TITLE_RE = re.compile(r"^\s*(swarm\s+)?audit\b", re.IGNORECASE)
READ_ONLY_QUALIFIER_RE = re.compile(
    r"\b(read[- ]?only|report[- ]?only|no\s+code\s+changes?|findings[- ]?only)\b",
    re.IGNORECASE,
)


def role_for(title: str) -> str:
    """Route an issue to a role by keyword. The units share one workspace, so
    this only decides who owns the fix, not which files are safe.

    Write-intent issues must resolve to an [`EDITING_ROLES`] unit: a read-only
    role cannot implement or commit, so routing one here would loop forever
    (VED-360). Only titles that explicitly ask for a review/audit go to a
    read-only role; an `audit` title is write work unless it asks for a
    read-only report (VED-365).
    """
    t = title.lower()
    if REVIEW_TITLE_RE.search(title):
        return "reviewer"
    if AUDIT_TITLE_RE.search(title) and READ_ONLY_QUALIFIER_RE.search(title):
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


def all_issue_states() -> dict[str, str]:
    """Every card's identifier -> Linear state name, for the reconcile pass."""
    result = subprocess.run(
        ["linear", "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    return {
        node["identifier"]: (node.get("state") or {}).get("name") or ""
        for node in nodes
    }


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

















def reset_context(session_id: str) -> None:
    """Clear a unit's context in place, keeping its session id, so the next
    card starts from a fresh transcript (VED-373)."""
    if not session_id:
        return
    request = urllib.request.Request(
        f"{GATEWAY}/sessions/{session_id}/reset",
        data=b"{}",
        headers={"content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=10):
            pass
    except OSError:
        pass


def memory_context() -> str:
    """The shared project memory block, or an empty string when there is none.

    Injected into every dispatch so a decision recorded by one unit is visible
    to every later unit and to the conductor's prompts (VED-371).
    """
    try:
        return memory.context_block()
    except Exception as error:  # memory must never block dispatch
        print(f"conductor memory error: {error}", flush=True)
        return ""




def assignment_prompt(identifier: str, title: str, role: str) -> str:
    """Prompt for a write-capable unit: implement, gate, report for verification.

    The owner never sets the card Done (VED-369): it moves the card to
    `In Review` so the conductor can dispatch a fresh verifier. Only a verifier
    pass promotes it to Done.

    """
    lines = [
        f"Conductor assignment: work Linear issue {identifier}.",
        f"Title: {title}",
    ]
    lines.append(
        f"Read it with `linear issue view {identifier}`. Set it In Progress, implement the fix, "
        f"then run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, "
        f"and `cargo test --workspace`. Commit only the files you changed with a message that "
        f"references {identifier}; do NOT push."
    )
    lines.append(
        f"Comment the result on {identifier}, then move it "
        f"to In Review — do NOT set it Done: an independent verifier must re-run the acceptance "
        f"check first. Report the exact test count from `cargo test --workspace` in your comment. "
        f"Sign — [{role}]."
    )
    prompt = "\n".join(lines)
    context = memory_context()
    if context:
        prompt += (
            "\n\n"
            + context
            + "\nRecord anything a later unit must know with "
            "`python3 scripts/swarm_memory.py record --kind <kind> --text \"...\" --issue "
            + identifier
            + "`."
        )
    return prompt


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
    context = memory_context()
    if context:
        prompt += (
            "\n\n"
            + context
            + "\nRecord anything a later unit must know with "
            "`python3 scripts/swarm_memory.py record --kind <kind> --text \"...\" --issue "
            + identifier
            + "`."
        )
    return prompt


def review_prompt(identifier: str, title: str, role: str) -> str:
    """Prompt for a read-only unit: audit, report, and stop.

    Never asks for a commit. The unit records its verdict on the issue; if the
    issue needs code changes it posts a machine-readable route verdict and the
    conductor re-routes it to a write-capable unit instead of retrying this
    read-only one.
    """
    prompt = (
        f"Conductor assignment: audit Linear issue {identifier}.\n"
        f"Title: {title}\n"
        f"Read it with `linear issue view {identifier}`. You are the read-only {role} unit: "
        f"do NOT edit files, commit, or change the issue state. Verify the claim, inspect the "
        f"relevant code, and comment your findings on {identifier} (exact files and commands). "
        f"If a code change is required, post a comment containing the exact line "
        f"`@conductor route-to: coder` so the conductor re-routes it to a write-capable unit, "
        f"and say explicitly that it is out of your scope. Sign — [{role}]."
    )
    context = memory_context()
    if context:
        prompt += "\n\n" + context
    return prompt


def issue_comments(identifier: str) -> list[str]:
    """Return the bodies of an issue's comments via the Linear CLI."""
    result = subprocess.run(
        ["linear", "issue", "view", identifier, "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    payload = json.loads(result.stdout)
    return [node.get("body") or "" for node in payload.get("comments", {}).get("nodes", [])]


def route_target(comments: list[str]) -> str | None:
    """Parse the last `@conductor route-to: <role>` verdict from comments.

    Returns the requested editing role, or None when there is no verdict. A
    verdict naming a read-only role is ignored: it would re-route to another
    unit that cannot fix the issue.
    """
    target = None
    for body in comments:
        match = ROUTE_VERDICT_RE.search(body)
        if match:
            target = match.group(1).lower()
    if target in EDITING_ROLES:
        return target
    return None


def pending_route(identifier: str) -> str | None:
    """Route target requested by a read-only reviewer verdict, or None.

    A verdict naming a read-only role is ignored: re-routing there would not
    fix the issue. The caller records `rerouted` so this fires at most once.
    """
    try:
        return route_target(issue_comments(identifier))
    except Exception:
        return None


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


# --- collision protocol (VED-375) -----------------------------------------

# Title fragments that map to a repo scope, so the conductor can declare a
# useful intent even before the unit has read the card. Conservative: anything
# unmatched yields an empty scope, which the detector treats as fail-safe
# (never silently disjoint).
SCOPE_HINTS = (
    (r"pi-gateway|gateway", "crates/pi-gateway"),
    (r"pi-host|host::|Host::", "crates/pi-host"),
    (r"pi-rpc|rpc\b", "crates/pi-rpc"),
    (r"pi-agent", "crates/pi-agent"),
    (r"pi-tui", "crates/pi-tui"),
    (r"pi-cli", "crates/pi-cli"),
    (r"pi-plugins|plugin", "crates/pi-plugins"),
    (r"pi-providers|provider", "crates/pi-providers"),
    (r"pi-session|session", "crates/pi-session"),
    (r"pi-triggers|trigger", "crates/pi-triggers"),
    (r"swarm_conductor|conductor", "scripts/swarm_conductor.py"),
    (r"swarm_collision|collision|intent", "scripts/swarm_collision.py"),
    (r"mem_gate", "scripts/mem_gate.py"),
    (r"ONBOARDING|swarm guide|docs/swarm", "docs/swarm/README.md"),
)

# Read-only roles' operations never block (VED-375 AC6).
OPERATION_FOR = {
    "reviewer": "review",
    "planner": "review",
    "researcher": "review",
    "coder": "edit",
    "tester": "edit",
    "synthesizer": "docs",
}


def scope_for(title: str) -> list[str]:
    """Best-effort repo scope for a card from its title.

    Empty when nothing matches; the detector treats an empty scope as unknown
    and fails safe, so a wrong guess never silently permits a collision.
    """
    scope: list[str] = []
    for pattern, path in SCOPE_HINTS:
        if re.search(pattern, title, re.IGNORECASE) and path not in scope:
            scope.append(path)
    return scope


def linear_comment(identifier: str, body: str) -> None:
    """Post a signed comment on a card. Best-effort: never kills the loop."""
    result = subprocess.run(
        ["linear", "issue", "comment", "add", identifier, "--body", body],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    if result.returncode != 0:
        print(f"comment on {identifier} failed: {result.stderr.strip()}", flush=True)


def record_collision(
    issue_a: str, issue_b: str, decision: dict, role: str, forced: bool = False
) -> None:
    """Record a collision + its resolution on both cards (VED-375 AC10/AC13)."""
    intersection = ", ".join(decision.get("intersection") or []) or "declared scope"
    verb = "forced" if forced else decision.get("decision")
    for self_id, other_id in ((issue_a, issue_b), (issue_b, issue_a)):
        body = (
            f"Collision protocol: intent for {self_id} "
            f"{'overrides' if forced else 'overlaps'} {other_id} on {intersection} "
            f"(kind={decision.get('kind')}). Resolution: {verb}. "
            f"Reason: {decision.get('reason')}."
        )
        linear_comment(self_id, f"{body}\n\n- [{role}]")


def declare_card_intent(
    state: dict, identifier: str, title: str, role: str, session_id: str,
    scope: list[str] | None = None, symbols: list[str] | None = None,
    branch: str | None = None, now: float | None = None,
) -> dict:
    """Declare (or update) a card's intent in the durable state table.

    Prefers an explicitly supplied scope; otherwise derives one from the title.
    Idempotent for the same card (AC3) and refused for terminal cards.
    """
    intents = state.setdefault("intents", {})
    record = {
        "issue": identifier,
        "role": role,
        "session_id": session_id,
        "operation": OPERATION_FOR.get(role, "edit"),
        "scope": scope if scope is not None else scope_for(title),
        "symbols": symbols or [],
        "branch": branch or f"swarm/{identifier}",
        "declared_at": now if now is not None else time.time(),
    }
    return collision.declare_intent(intents, record)


def check_collisions(
    state: dict, candidate: dict, terminal_issues: set[str] | None = None,
    force: bool = False,
) -> dict:
    """Resolve a candidate intent against the in-flight table.

    Returns the candidate's decision. On a blocking overlap the collision is
    recorded on both cards; a `--force` candidate is allowed but the override
    is still recorded (AC13).
    """
    intents = state.setdefault("intents", {})
    order = list(intents.values()) + [candidate]
    decisions = collision.resolve_collisions(
        order,
        terminal_issues=terminal_issues or set(),
        force_issues={candidate["issue"]} if force else set(),
    )
    decision = decisions.get(
        candidate["issue"],
        {"decision": collision.DECISION_ALLOW, "reason": "no overlap", "intersection": [], "kind": None},
    )
    if decision.get("blocked_by"):
        record_collision(
            candidate["issue"], decision["blocked_by"], decision,
            candidate.get("role", "coder"), forced=bool(decision.get("forced")),
        )
    return decision


def release_terminal_intents(state: dict, terminal_issues: set[str]) -> int:
    """Drop intents for cards that left the board (AC8/AC11)."""
    intents = state.setdefault("intents", {})
    released = 0
    for identifier in list(intents):
        if identifier in terminal_issues:
            collision.release_intent(intents, identifier)
            released += 1
    return released


# The conductor's in-flight intent table (VED-375) lives beside the
# orchestrator's claim state so it survives a restart.
INTENTS_PATH = os.environ.get("SWARM_INTENTS_PATH", "/tmp/swarm-intents.json")


def load_intents() -> dict:
    try:
        with open(INTENTS_PATH) as handle:
            data = json.load(handle)
        if isinstance(data, dict):
            return data
    except (OSError, json.JSONDecodeError):
        pass
    return {}


def save_intents(intents: dict) -> None:
    try:
        with open(INTENTS_PATH, "w") as handle:
            json.dump(intents, handle)
    except OSError as error:  # intent persistence must never stop dispatch
        print(f"conductor intent save error: {error}", flush=True)


# an isolated checkout, so the reconcile pass can remove the tree and branch
# once the card is terminal. Persisted beside the claim/intent state.






def terminal_issues_from_board() -> set[str]:
    """Card ids currently in a terminal Linear state."""
    import swarm_orchestrator as orchestrator_module

    result = subprocess.run(
        [
            "linear",
            "issue",
            "mine",
            "--team",
            orchestrator_module.TEAM,
            "--project",
            orchestrator_module.PROJECT,
            "--all-states",
            "--json",
        ],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    try:
        nodes = json.loads(result.stdout)["issues"]["nodes"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return set()
    return {
        node["identifier"]
        for node in nodes
        if (node.get("state") or {}).get("name") in collision.TERMINAL_ISSUE_STATES
    }


def collision_gate(
    intents: dict,
    issue: str,
    title: str,
    role: str,
    session_id: str,
    now: float,
    terminal_issues: set[str] | None = None,
) -> dict | None:
    """Declare an editing card's intent and resolve it against the table.

    Returns the collision decision, or `None` for a read-only role (which never
    blocks and never declares a write intent, VED-375 AC6). The caller must not
    dispatch when the decision is BLOCK or WARN.
    """
    if role not in EDITING_ROLES:
        return None
    candidate = declare_card_intent(
        intents, issue, title, role, session_id, now=now
    )
    return check_collisions(intents, candidate, terminal_issues=terminal_issues or set())






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
    intents = load_intents()
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
            # Fold the merge queue's outcome into the claim table (VED-377): a
            # merged issue is terminal and never re-dispatched; a failed or
            # conflicted one re-opens (keeping its attempt count) for a fix.
            # Cards that left the board release their intent so a finished card
            # never blocks new work (VED-375 AC8/AC11).
            terminal = terminal_issues_from_board()
            if release_terminal_intents(intents, terminal):
                save_intents(intents)
            decisions = orchestrator.tick(
                board,
                now=now,
                is_busy=lambda sid: orchestrator_module.unit_is_busy(units.get(sid), now),
                route=role_for,
                session_for=lambda role: ROLE_UNITS[role],
                read_only_roles=READ_ONLY_ROLES,
                blocked=dag_gate(),
                reroute=pending_route,
            )
            for decision in decisions:
                if decision.action == "dispatch":
                    title = board.get(decision.issue, {}).get("title", "")
                    # Before an editing card is dispatched it declares an intent
                    # and checks it against the in-flight table (VED-375). An
                    # overlapping write intent is blocked; the collision is
                    # recorded on both cards and the dispatch is skipped.
                    if decision.role in EDITING_ROLES:
                        collision_decision = collision_gate(
                            intents,
                            decision.issue,
                            title,
                            decision.role,
                            decision.session_id,
                            now,
                            terminal,
                        )
                        if collision_decision is not None and collision_decision["decision"] in (
                            collision.DECISION_BLOCK,
                            collision.DECISION_WARN,
                        ):
                            print(
                                f"blocked {decision.issue}: {collision_decision['reason']} "
                                f"(blocked_by={collision_decision.get('blocked_by')})",
                                flush=True,
                            )
                            continue
                        save_intents(intents)
                    # Cards run in the shared main tree, one editor at a time.
                    session_id = decision.session_id
                    # Fresh context per card: drop the unit's transcript before
                    # handing it the next task, so it never grows across cards
                    # (VED-373).
                    if FRESH_CONTEXT:
                        reset_context(session_id)
                    try:
                        dispatch(
                            session_id,
                            prompt_for(decision.issue, title, decision.role),
                        )
                    except OSError as error:
                        orchestrator.record_failure(
                            decision.issue, time.time(), f"dispatch failed: {error}"
                        )
                elif decision.action == "stop":
                    abort(decision.session_id)
                    release_terminal_intents(intents, {decision.issue})
                    save_intents(intents)
                print(
                    f"{decision.action} {decision.issue} {decision.reason}", flush=True
                )
            orchestrator.save(orchestrator_module.STATE_PATH)
        except Exception as error:  # keep the conductor alive across transient errors
            print(f"conductor error: {error}", flush=True)
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    raise SystemExit(main())
