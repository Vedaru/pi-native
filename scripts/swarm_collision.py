#!/usr/bin/env python3
"""Swarm collision protocol: declare intent/scope before editing (VED-375).

Parallel runs can collide when two units edit the same files or symbols. This
module makes that collision explicit and deterministic, in the spirit of
`foremerge`/`concord-mcp`: before a unit writes, it (or the orchestrator on its
behalf) declares an *intent* — ``{issue, role, session_id, operation, scope,
symbols}`` — and a coordinator checks it against the other in-flight intents.
An overlapping write intent is *blocked* and the collision is recorded, rather
than discovered later at merge time.

The protocol is pure: no network, no clock reads except the caller-supplied
``declared_at`` tie-breaker. The conductor/orchestrator owns persistence and
comment delivery; this module owns the rules.

Intents are plain dicts (JSON-serialisable), so they persist in the same
durable store as the claim table and survive a restart:

    {
      "issue": "VED-375",
      "role": "coder",
      "session_id": "...",
      "operation": "edit",              # edit | docs | test | review
      "scope": ["crates/pi-rpc/src/lib.rs", "crates/pi-agent"],
      "symbols": ["SessionState::switch"],
      "branch": "swarm/VED-375",
      "declared_at": 1730000000
    }

Rules (VED-375 acceptance):

* Overlap is ``scope ∩ scope`` (path prefix/glob) OR ``symbols ∩ symbols``,
  between two *write* intents (``edit``/``docs``) whose cards are open.
* Read-only intents (``review``/``test``) never block; they may carry an
  informational tag only.
* The later ``declared_at`` write intent is blocked by the earlier one.
* Empty/unknown scope is fail-safe: it is never treated as disjoint. The
  resolver blocks requiring explicit resolution rather than silently allowing.
* A blocked intent unblocks automatically when its blocker is terminal/released.
"""

from __future__ import annotations

import fnmatch
import os

# Operations that write to the shared codebase. Everything else is read-only
# for collision purposes (a `test`/`review` unit reads but does not edit).
WRITE_OPERATIONS = {"edit", "docs"}
READ_ONLY_OPERATIONS = {"test", "review"}

# A card in one of these states has released its claim: it never blocks and its
# own intent is not re-evaluated as blocked.
TERMINAL_ISSUE_STATES = {"Done", "Canceled", "Cancelled"}

# Resolution decisions.
DECISION_ALLOW = "allow"
DECISION_BLOCK = "block"
DECISION_WARN = "warn"


def is_write_operation(operation: str) -> bool:
    return operation in WRITE_OPERATIONS


def normalize_scope_entry(entry: str, root: str | None = None) -> str:
    """Normalise one scope entry to a repo-relative path.

    Handles ``./``, an absolute path under ``root``, redundant ``..``
    segments, a trailing ``/``, and backslashes. A glob pattern keeps its
    wildcard characters; only its static prefix is normalised.
    """
    text = entry.strip().replace("\\", "/")
    if not text:
        return ""
    if os.path.isabs(text):
        if root:
            root_abs = os.path.abspath(root)
            text = os.path.relpath(os.path.abspath(text), root_abs)
        else:
            text = text.lstrip("/")
    text = os.path.normpath(text)
    if text == ".":
        return ""
    if any(ch in text for ch in "*?["):
        prefix, _, tail = text.partition("*")
        prefix = os.path.normpath(prefix)
        if prefix == ".":
            prefix = ""
        return f"{prefix}*{tail}" if prefix else f"*{tail}"
    return text


def normalize_scope(scope: list[str] | None, root: str | None = None) -> list[str]:
    """Normalise a whole scope list, dropping empties and duplicates (stable)."""
    seen: list[str] = []
    for entry in scope or []:
        normalised = normalize_scope_entry(entry, root)
        if normalised and normalised not in seen:
            seen.append(normalised)
    return seen


def _path_covers(a: str, b: str) -> bool:
    """Whether scope entry ``a`` covers ``b`` (or they are the same file)."""
    if a == b:
        return True
    if any(ch in a for ch in "*?["):
        if fnmatch.fnmatch(b, a) or fnmatch.fnmatch(os.path.dirname(b) + "/", a):
            return True
    if any(ch in b for ch in "*?["):
        if fnmatch.fnmatch(a, b):
            return True
    return b.startswith(a.rstrip("/") + "/") or a.startswith(b.rstrip("/") + "/")


def scope_intersection(
    scope_a: list[str], scope_b: list[str], root: str | None = None
) -> list[str]:
    """Repo-relative paths present in both scopes (prefix/glob aware)."""
    a = normalize_scope(scope_a, root)
    b = normalize_scope(scope_b, root)
    hits: list[str] = []
    for left in a:
        for right in b:
            if _path_covers(left, right):
                hits.append(right if len(right) >= len(left) else left)
    matched: list[str] = []
    for hit in hits:
        if hit not in matched:
            matched.append(hit)
    return matched


def symbol_intersection(symbols_a: list[str], symbols_b: list[str]) -> list[str]:
    """Symbols declared by both intents (exact match, order-stable)."""
    b = set(symbols_a or [])
    hits = []
    for symbol in symbols_b or []:
        if symbol in b and symbol not in hits:
            hits.append(symbol)
    return hits


def _both_scopes_known(intent_a: dict, intent_b: dict) -> bool:
    return bool(intent_a.get("scope")) and bool(intent_b.get("scope"))


def detect_overlap(intent_a: dict, intent_b: dict, root: str | None = None) -> dict | None:
    """A structured finding when two intents overlap, else ``None``.

    Symmetric in ``(a, b)``. Only write-vs-write overlaps raise a blocking
    finding; a read-only intent produces an informational tag instead.
    """
    if intent_a.get("issue") == intent_b.get("issue"):
        return None

    # Canonical order so `detect_overlap(a, b) == detect_overlap(b, a)` (AC7).
    first_issue, second_issue = sorted(
        (str(intent_a.get("issue")), str(intent_b.get("issue")))
    )

    write_a = is_write_operation(intent_a.get("operation", ""))
    write_b = is_write_operation(intent_b.get("operation", ""))
    files = sorted(
        scope_intersection(intent_a.get("scope") or [], intent_b.get("scope") or [], root)
    )
    symbols = sorted(
        symbol_intersection(intent_a.get("symbols") or [], intent_b.get("symbols") or [])
    )

    if not write_a or not write_b:
        if files or symbols:
            return {
                "issue_a": first_issue,
                "issue_b": second_issue,
                "kind": "informational",
                "blocking": False,
                "intersection": symbols or files,
            }
        return None

    if not _both_scopes_known(intent_a, intent_b):
        return {
            "issue_a": first_issue,
            "issue_b": second_issue,
            "kind": "unknown-scope",
            "blocking": True,
            "intersection": files or symbols,
        }

    if not files and not symbols:
        return None

    if symbols:
        return {
            "issue_a": first_issue,
            "issue_b": second_issue,
            "kind": "symbol",
            "blocking": True,
            "intersection": symbols,
        }

    return {
        "issue_a": first_issue,
        "issue_b": second_issue,
        "kind": "file",
        "blocking": True,
        "intersection": files,
    }


def detect_all(intents: list[dict], root: str | None = None) -> list[dict]:
    """Every pairwise finding, deterministic and order-independent."""
    findings: dict[tuple[str, str], dict] = {}
    for i, first in enumerate(intents):
        for second in intents[i + 1 :]:
            finding = detect_overlap(first, second, root)
            if finding is None:
                continue
            key = tuple(sorted((str(finding["issue_a"]), str(finding["issue_b"]))))
            findings[key] = finding
    return [findings[key] for key in sorted(findings)]


def _earlier(first: dict, second: dict) -> tuple[dict, dict]:
    """Order two intents by ``declared_at``, then issue id for determinism."""
    first_key = (float(first.get("declared_at") or 0), str(first.get("issue")))
    second_key = (float(second.get("declared_at") or 0), str(second.get("issue")))
    return (first, second) if first_key <= second_key else (second, first)


def _reason(finding: dict, blocker: str) -> str:
    if finding.get("kind") == "unknown-scope":
        return (
            f"scope unknown for {finding['issue_a']}/{finding['issue_b']}; "
            "explicit resolution required"
        )
    what = ", ".join(finding.get("intersection") or []) or "declared scope"
    return f"overlaps {blocker} on {what}"


def resolve_collisions(
    intents: list[dict],
    terminal_issues: set[str] | None = None,
    root: str | None = None,
    force_issues: set[str] | None = None,
) -> dict[str, dict]:
    """Resolve each intent to a decision: ``allow`` / ``block`` / ``warn``.

    Terminal and ``--force`` intents are released (AC8/AC11/AC13). An
    overlapping pair blocks the *later* declaration (AC9).
    """
    terminal_issues = terminal_issues or set()
    force_issues = force_issues or set()
    active = [intent for intent in intents if intent.get("issue") not in terminal_issues]
    decisions: dict[str, dict] = {
        str(intent.get("issue")): {
            "decision": DECISION_ALLOW,
            "reason": "no overlapping in-flight write intent",
            "blocked_by": None,
            "intersection": [],
            "kind": None,
        }
        for intent in active
    }

    for i, first in enumerate(active):
        for second in active[i + 1 :]:
            finding = detect_overlap(first, second, root)
            if finding is None or not finding.get("blocking"):
                continue
            earlier, later = _earlier(first, second)
            earlier_issue = str(earlier.get("issue"))
            later_issue = str(later.get("issue"))
            if later_issue in force_issues:
                decisions[later_issue] = {
                    "decision": DECISION_ALLOW,
                    "reason": f"forced past {earlier_issue}",
                    "blocked_by": earlier_issue,
                    "intersection": finding.get("intersection") or [],
                    "kind": finding.get("kind"),
                    "forced": True,
                }
                continue
            decision = (
                DECISION_WARN if finding.get("kind") == "unknown-scope" else DECISION_BLOCK
            )
            if decisions[later_issue]["decision"] == DECISION_BLOCK:
                continue
            if (
                decisions[later_issue]["decision"] == DECISION_WARN
                and decision == DECISION_BLOCK
            ):
                pass
            decisions[later_issue] = {
                "decision": decision,
                "reason": _reason(finding, earlier_issue),
                "blocked_by": earlier_issue,
                "intersection": finding.get("intersection") or [],
                "kind": finding.get("kind"),
            }

    return decisions


def declare_intent(
    intents: dict[str, dict],
    intent: dict,
    terminal_issues: set[str] | None = None,
) -> dict:
    """Add or update an intent in the mutable ``intents`` table (keyed by issue).

    Idempotent for the same ``(issue, session_id)``: an existing record is
    updated in place, not duplicated (AC1/AC3). Declaring for a terminal card
    is rejected. Returns the stored record; raises ``ValueError`` on rejection.
    """
    terminal_issues = terminal_issues or set()
    issue = str(intent.get("issue") or "")
    if not issue:
        raise ValueError("intent requires an issue")
    if issue in terminal_issues:
        raise ValueError(f"{issue} is terminal; refusing to declare an intent")

    existing = intents.get(issue)
    record = dict(existing) if existing else {
        "issue": issue,
        "declared_at": intent.get("declared_at", 0),
    }
    for field in (
        "role",
        "session_id",
        "operation",
        "scope",
        "symbols",
        "branch",
        "declared_at",
    ):
        if field in intent and intent[field] is not None:
            record[field] = intent[field]
    record.setdefault("scope", [])
    record.setdefault("symbols", [])
    intents[issue] = record
    return record


def release_intent(intents: dict[str, dict], issue: str) -> bool:
    """Drop an intent (its claim was released/its card went terminal)."""
    return intents.pop(str(issue), None) is not None
