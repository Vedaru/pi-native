#!/usr/bin/env python3
"""Swarm merge queue with risk-based gating (VED-377).

Accepted `swarm/<ID>` branches were merged by hand. This queue makes that
deterministic and ordered: an entry enters the queue, is rebased onto current
`master`, gated (fmt/clippy/test) with a receipt bound to the exact head, and
merged locally in FIFO order. Risky changes require a recorded reviewer
approval before they can merge. Conflicts are surfaced with exact paths and
recorded, never silently dropped. The queue does **not** push — the
synthesizer pushes (docs/swarm/README.md).

State lives beside the worktrees it references (not `/tmp`, which a reboot wipes
and which could otherwise re-merge a branch): `<worktree-root>/.merge-queue.json`,
overridable with `SWARM_MERGE_QUEUE_PATH`.

    scripts/swarm_merge_queue.py enqueue VED-123
    scripts/swarm_merge_queue.py list
    scripts/swarm_merge_queue.py status VED-123
    scripts/swarm_merge_queue.py run                 # process the head entry
    scripts/swarm_merge_queue.py approve VED-123
    scripts/swarm_merge_queue.py resolve VED-123
    scripts/swarm_merge_queue.py remove VED-123

The pure rules (risk classification, ordering, receipt binding, merge
eligibility) are separated from git/cargo I/O so they are unit-testable without
a repository or a build.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name: str):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, f"{name}.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


gate = _load("swarm_gate")
worktree = _load("swarm_worktree")

MAX_ATTEMPTS = 3

# Shared gate/CI files: touching any of these makes an entry high-risk because a
# mistake there affects every other branch (VED-377 risk classification).
HIGH_RISK_PATHS = (
    "Cargo.toml",
    "Cargo.lock",
    ".github/",
    "scripts/",
    "docs/swarm/",
)

# A cross-cutting crate boundary shared by other work.
CROSS_CUTTING_PATHS = (
    "crates/pi-agent/",
    "crates/pi-session/",
    "crates/pi-rpc/",
)

# Diffstat thresholds: over either makes an entry high-risk regardless of paths.
HIGH_RISK_FILES = 15
HIGH_RISK_LOC = 400

# Risk title signal, mirroring the conductor's REVIEW_TITLE_RE so there is one
# notion of "this card is about review/risk".
RISK_TITLE_RE = None  # set lazily by risk_title_signal

STATUS_QUEUED = "queued"
STATUS_REBASING = "rebasing"
STATUS_GATING = "gating"
STATUS_GATED = "gated"
STATUS_REVIEW_REQUIRED = "review_required"
STATUS_CONFLICT = "conflict"
STATUS_MERGED = "merged"
STATUS_FAILED = "failed"
STATUS_REMOVED = "removed"

# Statuses that count as "in flight": the queue is serial, so at most one entry
# may be in one of these at a time.
ACTIVE_STATUSES = {STATUS_REBASING, STATUS_GATING}


def queue_path() -> str:
    configured = os.environ.get("SWARM_MERGE_QUEUE_PATH")
    if configured:
        return configured
    return os.path.join(str(worktree.ROOT), ".merge-queue.json")


def load_queue() -> dict:
    path = queue_path()
    try:
        with open(path) as handle:
            payload = json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        payload = {}
    payload.setdefault("entries", [])
    payload.setdefault("seq", 0)
    return payload


def save_queue(queue: dict) -> None:
    """Atomic write (temp + rename) so a crash cannot corrupt the queue."""
    path = queue_path()
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = f"{path}.tmp"
    with open(tmp, "w") as handle:
        json.dump(queue, handle, indent=2)
    os.replace(tmp, path)


def find_entry(queue: dict, issue: str) -> dict | None:
    for entry in queue["entries"]:
        if entry.get("issue") == issue:
            return entry
    return None


# --- pure rules ------------------------------------------------------------


def classify_risk(
    paths: list[str],
    title: str = "",
    files_changed: int = 0,
    loc_changed: int = 0,
) -> str:
    """Deterministic risk classification: ``"high"`` or ``"low"``.

    High iff the diff touches a shared gate/CI file or a cross-cutting crate, the
    diffstat is over a threshold, or the title asks for review. No LLM.
    """
    if any(path.startswith(HIGH_RISK_PATHS) for path in paths):
        return "high"
    if any(path.startswith(CROSS_CUTTING_PATHS) for path in paths):
        return "high"
    if files_changed > HIGH_RISK_FILES or loc_changed > HIGH_RISK_LOC:
        return "high"
    if risk_title_signal(title):
        return "high"
    return "low"


def risk_title_signal(title: str) -> bool:
    import re

    return bool(re.search(r"^\s*(swarm\s+)?(audit|review|verify)\b", title or "", re.IGNORECASE))


def next_entry(queue: dict) -> dict | None:
    """The head of the FIFO queue eligible to act, or None.

    Serial invariant (AC2): if any entry is already rebasing/gating, nothing else
    runs this pass.
    """
    entries = sorted(queue.get("entries", []), key=lambda e: e.get("seq", 0))
    if any(e.get("status") in ACTIVE_STATUSES for e in entries):
        return None
    for entry in entries:
        if entry.get("status") in (STATUS_QUEUED, STATUS_GATING, STATUS_GATED, STATUS_REVIEW_REQUIRED):
            return entry
    return None


def receipt_is_green_for_head(entry: dict) -> bool:
    """AC1: a green receipt bound to the entry's reviewed head."""
    receipt = entry.get("receipt")
    if not receipt or receipt.get("gate") != "green":
        return False
    return receipt.get("sha") == entry.get("head")


def needs_approval(entry: dict) -> bool:
    return entry.get("risk") == "high" and not entry.get("approved_by")


def can_merge(entry: dict) -> bool:
    """AC1 + AC4: green receipt on the reviewed head and risk cleared."""
    return (
        entry.get("status") == STATUS_GATED
        and receipt_is_green_for_head(entry)
        and not needs_approval(entry)
    )


def bind_receipt(entry: dict, receipt: dict) -> None:
    """Attach a receipt; a red one fails the entry, a green one advances it."""
    entry["receipt"] = receipt
    if receipt.get("gate") == "green" and receipt.get("sha") == entry.get("head"):
        entry["status"] = STATUS_REVIEW_REQUIRED if needs_approval(entry) else STATUS_GATED
    else:
        entry["status"] = STATUS_FAILED


def enqueue(queue: dict, issue: str, head: str, base: str, risk: str,
            branch: str | None = None, now: float | None = None) -> dict:
    """Append (or update in place) an entry, keyed by issue (AC5, idempotent).

    Re-enqueuing the same issue with the same head is a no-op; a moved head
    replaces the entry and resets its receipt (the reviewed head rule).
    """
    existing = find_entry(queue, issue)
    if existing is not None:
        if existing.get("head") == head:
            return existing
        existing.update(
            head=head, base=base, risk=risk, branch=branch or existing.get("branch"),
            status=STATUS_QUEUED, receipt=None, conflict=None,
        )
        return existing
    queue["seq"] += 1
    entry = {
        "issue": issue,
        "branch": branch or worktree.branch_for(issue),
        "head": head,
        "base": base,
        "risk": risk,
        "status": STATUS_QUEUED,
        "attempts": 0,
        "seq": queue["seq"],
        "receipt": None,
        "conflict": None,
        "approved_by": None,
        "merge_sha": None,
        "enqueued_at": now if now is not None else time.time(),
    }
    queue["entries"].append(entry)
    return entry


def resolve_entry(queue: dict, issue: str, resolution: str, now: float | None = None) -> dict | None:
    """Record a conflict resolution and re-queue the entry (AC3)."""
    entry = find_entry(queue, issue)
    if entry is None:
        return None
    entry.setdefault("conflict", [])
    entry["resolution"] = resolution
    entry["resolved_at"] = now if now is not None else time.time()
    entry["status"] = STATUS_QUEUED
    return entry


def approve_entry(queue: dict, issue: str, reviewer: str = "reviewer", now: float | None = None) -> dict | None:
    """Record a reviewer approval, clearing a high-risk hold (AC4)."""
    entry = find_entry(queue, issue)
    if entry is None:
        return None
    entry["approved_by"] = reviewer
    entry["approved_at"] = now if now is not None else time.time()
    if entry.get("status") == STATUS_REVIEW_REQUIRED and receipt_is_green_for_head(entry):
        entry["status"] = STATUS_GATED
    return entry


# --- git / build I/O -------------------------------------------------------


def git(args: list[str], cwd: str, executor=subprocess.run) -> subprocess.CompletedProcess:
    return executor(["git", *args], cwd=cwd, capture_output=True, text=True)


def current_head(cwd: str, executor=subprocess.run) -> str:
    result = git(["rev-parse", "HEAD"], cwd, executor)
    return (result.stdout or "").strip()


def master_head(executor=subprocess.run) -> str:
    result = git(["rev-parse", "HEAD"], str(worktree.REPO), executor)
    return (result.stdout or "").strip()


def run_gate_in(worktree_path: str, executor=subprocess.run) -> dict:
    return gate.run_gate(worktree_path, executor=executor)


def rebase_onto_master(worktree_path: str, executor=subprocess.run) -> tuple[bool, list[str]]:
    """Rebase the worktree onto `master`. Returns (clean, conflicting_paths).

    On conflict the rebase is aborted and the conflicting paths are returned so
    the caller can surface + record them (AC3). Nothing is auto-resolved here:
    silently guessing at a conflict is worse than surfacing it.
    """
    result = git(["rebase", "master"], worktree_path, executor)
    if result.returncode == 0:
        return True, []
    conflicted = git(["diff", "--name-only", "--diff-filter=U"], worktree_path, executor)
    paths = [p for p in (conflicted.stdout or "").splitlines() if p.strip()]
    if not paths:
        # The rebase failed for another reason (dirty tree, missing base):
        # report a sentinel so the caller records *something*.
        paths = ["<rebase failed: non-conflict>"]
    git(["rebase", "--abort"], worktree_path, executor)
    return False, paths


def merge_to_master(issue: str, head: str, executor=subprocess.run) -> tuple[bool, str]:
    """Merge `head` into the local `master` (no push). Returns (ok, sha).

    `--no-ff` keeps a merge commit so the landed head is auditable and the
    branch is never silently fast-forwarded to an unreviewed tip.
    """
    repo = str(worktree.REPO)
    git(["checkout", "master"], repo, executor)
    result = git(["merge", "--no-ff", "--no-edit", head], repo, executor)
    sha = current_head(repo, executor)
    return result.returncode == 0, sha


def teardown_worktree(issue: str, executor=subprocess.run) -> None:
    """Remove the merged worktree and its branch (AC6)."""
    worktree.remove(issue, force=True, delete_branch=True)


def linear_comment(identifier: str, body: str, executor=subprocess.run) -> None:
    executor(
        ["linear", "issue", "comment", "add", identifier, "--body", body],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )


def report(identifier: str, body: str) -> None:
    linear_comment(identifier, body)


def comment_result(entry: dict, role: str = "synthesizer") -> None:
    """AC6: comment the receipt + merge_sha on the issue."""
    receipt = entry.get("receipt") or {}
    body = (
        f"Merge queue: {entry['issue']} {entry['status']} "
        f"(head={entry['head'][:12]}, risk={entry['risk']}, "
        f"gate={receipt.get('gate')}, tests={receipt.get('test')}, "
        f"merge_sha={(entry.get('merge_sha') or '-')[:12]})."
    )
    if entry.get("conflict"):
        body += f" Conflicts: {', '.join(entry['conflict'])}."
    report(entry["issue"], f"{body}\n\n- [{role}]")


def process_entry(
    entry: dict,
    executor=subprocess.run,
    now: float | None = None,
    is_busy_fn=None,
) -> dict:
    """Advance one entry one step: rebase -> gate -> (approve) -> merge.

    Returns the entry. The queue is serial: the caller only ever calls this for
    the single `next_entry`, and it refuses while a unit is busy building
    (`is_busy_fn`) so two heavy builds never race (AC2).
    """
    if is_busy_fn is not None and is_busy_fn():
        return entry

    issue = entry["issue"]
    path = str(worktree.path_for(issue))

    if entry["status"] == STATUS_QUEUED:
        entry["status"] = STATUS_REBASING
        clean, conflicts = rebase_onto_master(path, executor)
        if not clean:
            entry["status"] = STATUS_CONFLICT
            entry["conflict"] = conflicts
            entry["attempts"] = entry.get("attempts", 0) + 1
            comment_result(entry)
            return entry
        entry["status"] = STATUS_GATING
        return entry

    if entry["status"] == STATUS_GATING:
        receipt = run_gate_in(path, executor)
        # Always bind to the head we actually gated, so a moved branch cannot
        # reuse an old receipt (AC1).
        entry["head"] = receipt.get("sha") or entry["head"]
        bind_receipt(entry, receipt)
        entry["attempts"] = entry.get("attempts", 0) + 1
        comment_result(entry)
        return entry

    if can_merge(entry):
        ok, sha = merge_to_master(issue, entry["head"], executor)
        if ok:
            entry["status"] = STATUS_MERGED
            entry["merge_sha"] = sha
            teardown_worktree(issue, executor)
            comment_result(entry)
        else:
            entry["status"] = STATUS_FAILED
            entry["attempts"] = entry.get("attempts", 0) + 1
            comment_result(entry)
        return entry

    return entry


def run_once(queue: dict, executor=subprocess.run, is_busy_fn=None, now: float | None = None) -> dict | None:
    """Process the head entry if eligible; returns it (or None)."""
    entry = next_entry(queue)
    if entry is None:
        return None
    process_entry(entry, executor=executor, now=now, is_busy_fn=is_busy_fn)
    save_queue(queue)
    return entry


# --- CLI -------------------------------------------------------------------


def _print_entry(entry: dict) -> None:
    print(
        f"{entry['issue']}\t{entry['status']}\t{entry['risk']}\t"
        f"head={entry['head'][:12]}\tseq={entry['seq']}"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    enq = sub.add_parser("enqueue", help="enqueue a branch for merge")
    enq.add_argument("issue")
    enq.add_argument("--head", help="reviewed head (default: branch tip)")
    enq.add_argument("--base", help="base sha (default: current master)")
    enq.add_argument("--risk", choices=["low", "high"])
    enq.add_argument("--title", default="")

    sub.add_parser("list", help="list entries in order")

    st = sub.add_parser("status", help="show one entry")
    st.add_argument("issue")
    st.add_argument("--json", action="store_true")

    runp = sub.add_parser("run", help="process one queue step")
    runp.add_argument("--all", action="store_true", help="keep processing until idle")

    ap = sub.add_parser("approve", help="record a reviewer approval (high risk)")
    ap.add_argument("issue")
    ap.add_argument("--reviewer", default="reviewer")

    rs = sub.add_parser("resolve", help="record a conflict resolution and re-queue")
    rs.add_argument("issue")
    rs.add_argument("--resolution", required=True)

    rm = sub.add_parser("remove", help="drop an entry")
    rm.add_argument("issue")

    args = parser.parse_args(argv)
    queue = load_queue()

    if args.command == "enqueue":
        branch = worktree.branch_for(args.issue)
        head = args.head or current_head(str(worktree.path_for(args.issue)))
        base = args.base or master_head()
        paths = changed_paths(args.issue)
        if args.risk:
            risk = args.risk
        else:
            files, loc = diffstat(args.issue)
            risk = classify_risk(paths, args.title, files, loc)
        entry = enqueue(queue, args.issue, head, base, risk, branch=branch)
        save_queue(queue)
        _print_entry(entry)
        return 0

    if args.command == "list":
        for entry in sorted(queue["entries"], key=lambda e: e.get("seq", 0)):
            _print_entry(entry)
        return 0

    if args.command == "status":
        entry = find_entry(queue, args.issue)
        if entry is None:
            print(f"not queued: {args.issue}", file=sys.stderr)
            return 1
        print(json.dumps(entry, indent=2) if args.json else "")
        if not args.json:
            _print_entry(entry)
        return 0

    if args.command == "approve":
        entry = approve_entry(queue, args.issue, args.reviewer)
        if entry is None:
            print(f"not queued: {args.issue}", file=sys.stderr)
            return 1
        comment_result(entry, role=args.reviewer)
        save_queue(queue)
        print(f"approved {args.issue} by {args.reviewer}")
        return 0

    if args.command == "resolve":
        entry = resolve_entry(queue, args.issue, args.resolution)
        if entry is None:
            print(f"not queued: {args.issue}", file=sys.stderr)
            return 1
        save_queue(queue)
        print(f"resolved {args.issue}: {args.resolution}")
        return 0

    if args.command == "remove":
        entry = find_entry(queue, args.issue)
        if entry is None:
            print(f"not queued: {args.issue}", file=sys.stderr)
            return 1
        entry["status"] = STATUS_REMOVED
        save_queue(queue)
        print(f"removed {args.issue}")
        return 0

    if args.command == "run":
        while True:
            advanced = run_once(queue, is_busy_fn=swarm_busy)
            if advanced is None:
                print("queue idle")
                return 0
            _print_entry(advanced)
            if not args.all or advanced["status"] in (STATUS_MERGED, STATUS_FAILED, STATUS_CONFLICT):
                return 0

    return 0


def changed_paths(issue: str, executor=subprocess.run) -> list[str]:
    result = git(["diff", "--name-only", f"master...{worktree.branch_for(issue)}"], str(worktree.REPO), executor)
    return [p for p in (result.stdout or "").splitlines() if p.strip()]


def diffstat(issue: str, executor=subprocess.run) -> tuple[int, int]:
    """(files_changed, loc_changed) for the branch against master."""
    result = git(["diff", "--numstat", f"master...{worktree.branch_for(issue)}"], str(worktree.REPO), executor)
    files = 0
    loc = 0
    for line in (result.stdout or "").splitlines():
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        files += 1
        for value in parts[:2]:
            try:
                loc += int(value)
            except ValueError:
                pass  # binary files report "-"
    return files, loc


def swarm_busy() -> bool:
    """Whether any unit is mid-build, shared with the conductor's heuristic."""
    import urllib.request

    gateway = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
    try:
        with urllib.request.urlopen(f"{gateway}/swarm", timeout=5) as response:
            payload = json.load(response)
    except Exception:
        # If the gateway is unreachable, do not block the queue.
        return False
    now = time.time()
    active = {
        "agent_start", "turn_start", "message_start", "message_update",
        "assistant_delta", "thinking_delta", "tool_start", "tool_end",
    }
    for unit in payload.get("units", []):
        if unit.get("inFlight"):
            return True
        if unit.get("lastEvent") in active and now - float(unit.get("lastEventAt") or 0) < 90:
            return True
    return False


if __name__ == "__main__":
    raise SystemExit(main())
