#!/usr/bin/env python3
"""Deterministic swarm orchestrator: claims, bounded concurrency, retries, reconcile.

The conductor (see `swarm_conductor.py`) is the planning/prioritisation layer.
This module is the *dispatch* layer and is deliberately deterministic — no LLM
is involved in deciding what runs. It owns:

* a **durable claim table**: issue -> (worker session, attempt, next_retry_at),
  persisted as JSON so the claim survives a restart;
* **bounded concurrency** via ``max_concurrent_agents`` (default 10);
* **exponential backoff** retries on transient failure;
* a **reconcile pass** that stops a run when its issue leaves an active state
  and releases claims on Done/Canceled;
* **startup cleanup** of claims whose cards are already terminal.

The tracker -> worktree -> session loop mirrors OpenAI Symphony (SPEC sections
7-8) and the `cyrus` / `contrabass` / `needle` / `sortie` / `machinist` swarms.

Usage as a daemon::

    python3 scripts/swarm_orchestrator.py            # poll forever
    python3 scripts/swarm_orchestrator.py --once     # single tick (used by tests/CI)

The daemon can also be imported as a library; `Orchestrator.tick()` takes a
board snapshot and a liveness snapshot and mutates the claim table without doing
any I/O, which is what the unit tests exercise.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
import urllib.request
from dataclasses import asdict, dataclass, field

GATEWAY = os.environ.get("SWARM_GATEWAY", "http://127.0.0.1:30142")
PROJECT = "4d5e47500fa6"
TEAM = "VED"

# Durable state: the claim table survives conductor restarts (VED-368).
STATE_PATH = os.environ.get("SWARM_STATE_PATH", "/tmp/swarm-orchestrator-state.json")
POLL_SECONDS = 20
DEFAULT_MAX_CONCURRENT_AGENTS = 10
DEFAULT_MAX_ATTEMPTS = 3

# Exponential backoff for transient failures: base * 2**(attempt-1), capped.
DEFAULT_BACKOFF_BASE_SECONDS = 30.0
DEFAULT_BACKOFF_MAX_SECONDS = 900.0

# Linear state `type` values that mean "the card is still live". Anything else
# (completed/canceled) releases the claim and stops the run.
ACTIVE_STATE_TYPES = {"unstarted", "started"}
TERMINAL_STATE_TYPES = {"completed", "canceled"}

# Claim statuses.
STATUS_PENDING = "pending"
STATUS_DISPATCHED = "dispatched"
STATUS_TERMINAL = "terminal"
STATUS_FAILED = "failed"

# Events that mean a unit is actively working, mirroring the conductor.
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

# A freshly dispatched claim is not judged failed until the unit has had time to
# start emitting events. Without this grace window the tick right after dispatch
# would see an idle unit and schedule a spurious retry.
DISPATCH_GRACE_SECONDS = 30.0


def backoff_delay(
    attempt: int,
    base: float = DEFAULT_BACKOFF_BASE_SECONDS,
    cap: float = DEFAULT_BACKOFF_MAX_SECONDS,
) -> float:
    """Exponential backoff for the *next* retry after ``attempt`` failures.

    ``attempt`` is 1-based: the first failure schedules ``base`` seconds, the
    second ``2*base``, and so on, clamped at ``cap``.
    """
    if attempt <= 0:
        return 0.0
    return min(cap, base * (2 ** (attempt - 1)))


@dataclass
class Claim:
    """One issue's dispatch bookkeeping.

    ``next_retry_at`` is a unix timestamp (0.0 means "eligible now"). A claim
    with ``terminal=True`` is never dispatched again until the card reopens.
    """

    issue: str
    title: str = ""
    role: str = ""
    session_id: str = ""
    attempt: int = 0
    status: str = STATUS_PENDING
    next_retry_at: float = 0.0
    dispatched_at: float = 0.0
    terminal: bool = False
    read_only: bool = False
    rerouted: bool = False

    def eligible(self, now: float, max_attempts: int) -> bool:
        if self.terminal:
            return False
        if self.attempt >= max_attempts:
            return False
        return now >= self.next_retry_at


@dataclass
class OrchestratorConfig:
    max_concurrent_agents: int = DEFAULT_MAX_CONCURRENT_AGENTS
    max_attempts: int = DEFAULT_MAX_ATTEMPTS
    backoff_base: float = DEFAULT_BACKOFF_BASE_SECONDS
    backoff_cap: float = DEFAULT_BACKOFF_MAX_SECONDS


@dataclass
class Decision:
    """What the orchestrator decided to do with one issue this tick."""

    action: str  # "dispatch" | "stop" | "release" | "retry" | "skip"
    issue: str
    role: str = ""
    session_id: str = ""
    reason: str = ""


@dataclass
class Orchestrator:
    """Claim-table owner. Pure logic; all I/O is done by the caller."""

    config: OrchestratorConfig = field(default_factory=OrchestratorConfig)
    claims: dict[str, Claim] = field(default_factory=dict)
    running: dict[str, str] = field(default_factory=dict)  # session_id -> issue
    last_action: list[Decision] = field(default_factory=list)

    # -- persistence -----------------------------------------------------
    @classmethod
    def load(cls, path: str, config: OrchestratorConfig | None = None) -> "Orchestrator":
        orchestrator = cls(config=config or OrchestratorConfig())
        try:
            with open(path) as handle:
                payload = json.load(handle)
        except (FileNotFoundError, json.JSONDecodeError):
            return orchestrator
        for issue, raw in (payload.get("claims") or {}).items():
            known = {f for f in Claim.__dataclass_fields__}
            orchestrator.claims[issue] = Claim(**{k: v for k, v in raw.items() if k in known})
        # The live `running` map is rebuilt from the claim table: a claim that
        # says "dispatched" is assumed still in flight until reconcile proves
        # otherwise, so a restart does not double-dispatch.
        for claim in orchestrator.claims.values():
            if claim.status == STATUS_DISPATCHED and claim.session_id and not claim.terminal:
                orchestrator.running[claim.session_id] = claim.issue
        return orchestrator

    def save(self, path: str) -> None:
        payload = {"claims": {issue: asdict(claim) for issue, claim in self.claims.items()}}
        directory = os.path.dirname(path)
        if directory:
            os.makedirs(directory, exist_ok=True)
        tmp = f"{path}.tmp"
        with open(tmp, "w") as handle:
            json.dump(payload, handle, indent=2, sort_keys=True)
        os.replace(tmp, path)

    # -- helpers ---------------------------------------------------------
    def active_claims(self) -> list[Claim]:
        return [
            claim
            for claim in self.claims.values()
            if claim.status == STATUS_DISPATCHED and not claim.terminal
        ]

    def claim_for(self, issue: str) -> Claim:
        claim = self.claims.get(issue)
        if claim is None:
            claim = Claim(issue=issue)
            self.claims[issue] = claim
        return claim

    # -- lifecycle -------------------------------------------------------
    def startup_cleanup(self, active_issues: set[str]) -> list[Decision]:
        """Drop claims whose cards are already terminal (or vanished).

        Called once when the daemon starts: a claim table that survived a
        restart may still hold cards that were completed/canceled while the
        orchestrator was down, and those must not be retried.
        """
        decisions: list[Decision] = []
        for issue in list(self.claims):
            if issue not in active_issues:
                claim = self.claims.pop(issue)
                self.running.pop(claim.session_id, None)
                decisions.append(
                    Decision(action="release", issue=issue, reason="card not active at startup")
                )
        self.last_action = decisions
        return decisions

    def release(self, issue: str, reason: str) -> Decision:
        """Release a claim and stop its run (Done/Canceled/left active state)."""
        claim = self.claims.get(issue)
        session_id = claim.session_id if claim else ""
        self.running.pop(session_id, None)
        if claim is not None:
            claim.status = STATUS_TERMINAL
            claim.terminal = True
            claim.next_retry_at = 0.0
        return Decision(action="stop", issue=issue, session_id=session_id, reason=reason)

    def record_failure(self, issue: str, now: float, reason: str) -> Decision:
        """A dispatched run failed transiently; schedule a backoff retry."""
        claim = self.claim_for(issue)
        self.running.pop(claim.session_id, None)
        claim.attempt += 1
        claim.status = STATUS_FAILED
        if claim.attempt >= self.config.max_attempts:
            claim.status = STATUS_TERMINAL
            claim.terminal = True
            return Decision(action="release", issue=issue, reason=f"exhausted retries: {reason}")
        claim.next_retry_at = now + backoff_delay(
            claim.attempt, self.config.backoff_base, self.config.backoff_cap
        )
        return Decision(
            action="retry",
            issue=issue,
            role=claim.role,
            session_id=claim.session_id,
            reason=f"{reason}; retry at {claim.next_retry_at:.0f}",
        )

    # -- the tick --------------------------------------------------------
    def tick(
        self,
        board: dict[str, dict],
        now: float,
        is_busy=None,
        route=None,
        session_for=None,
        read_only_roles=None,
        blocked=None,
        reroute=None,
    ) -> list[Decision]:
        """One deterministic reconcile + dispatch pass.

        ``board`` maps issue identifier -> a snapshot with at least ``state``
        (a name) and ``stateType`` (Linear's state ``type``) and ``title``.

        ``is_busy(session_id)`` reports whether a unit is currently working.
        ``route(title)`` returns the role for an issue. ``session_for(role)``
        returns the role unit's session id.

        ``blocked`` is an optional set of issue identifiers that must not be
        dispatched yet (for example task-DAG dependencies that are not Done,
        VED-378). Blocked issues are skipped for dispatch but keep any existing
        claim, so finishing a dependency does not lose in-flight work.

        ``reroute(issue)`` returns a role to force for an issue whose reviewer
        posted a route verdict (VED-365), or ``None``. A re-route revives a
        terminal read-only claim exactly once so the review -> coder handoff
        happens without ping-ponging back to the reviewer.
        """
        is_busy = is_busy or (lambda _sid: False)
        read_only_roles = read_only_roles or set()
        blocked = blocked or set()
        decisions: list[Decision] = []

        # 1. Reconcile: release claims whose card left an active state, and
        #    stop any run whose issue is gone from the board entirely.
        for issue, claim in list(self.claims.items()):
            snapshot = board.get(issue)
            if claim.terminal:
                # A terminal claim stays closed unless a human reopens the card
                # (moved it back to an active state). Read-only audits and
                # exhausted retries are never revived.
                if (
                    not claim.read_only
                    and claim.attempt < self.config.max_attempts
                    and snapshot is not None
                    and snapshot.get("stateType") in ACTIVE_STATE_TYPES
                ):
                    claim.terminal = False
                    claim.status = STATUS_PENDING
                    claim.next_retry_at = 0.0
                    decisions.append(Decision(action="retry", issue=issue, reason="card reopened"))
                continue
            if snapshot is None:
                decisions.append(self.release(issue, "issue left the board"))
                continue
            state_type = snapshot.get("stateType")
            if state_type in TERMINAL_STATE_TYPES or (
                state_type not in ACTIVE_STATE_TYPES and state_type is not None
            ):
                decisions.append(self.release(issue, f"issue is {snapshot.get('state')}"))
                continue
            # A dispatched claim whose unit is no longer busy has finished or
            # failed. A finished card normally leaves the active state, so a
            # still-active card here is a transient failure -> backoff retry.
            # Skip the grace window so a just-dispatched unit is not judged.
            if now - claim.dispatched_at < DISPATCH_GRACE_SECONDS:
                continue
            if claim.status == STATUS_DISPATCHED and not is_busy(claim.session_id):
                decisions.append(self.record_failure(issue, now, "run ended without completing"))
                continue

        # 2. Release a claim if another claim now owns the same busy session
        #    (the unit was reassigned). The claim table is the source of truth.
        for session_id, issue in list(self.running.items()):
            claim = self.claims.get(issue)
            if claim is None or claim.session_id != session_id or claim.terminal:
                self.running.pop(session_id, None)

        # 3. Startup cleanup equivalent: any active card with no claim is new
        #    work to be dispatched below.

        # 4. Dispatch, bounded by max_concurrent_agents. A card is eligible when
        #    it is active, not terminal, under max attempts, and past backoff.
        in_flight = len(self.running)
        # Sessions already targeted this tick, so one unit never receives two
        # prompts in the same pass (the units run one turn at a time).
        targeted = set(self.running)
        for issue in sorted(board):
            if in_flight >= self.config.max_concurrent_agents:
                decisions.append(Decision(action="skip", issue=issue, reason="concurrency cap"))
                continue
            snapshot = board[issue]
            if snapshot.get("stateType") not in ACTIVE_STATE_TYPES:
                continue
            if issue in blocked:
                # A task-DAG dependency has not finished (VED-378): do not
                # dispatch, but leave any existing claim untouched.
                decisions.append(
                    Decision(action="skip", issue=issue, reason="blocked by task DAG")
                )
                continue
            claim = self.claim_for(issue)
            if claim.session_id and claim.session_id in self.running:
                continue
            verdict_role = None
            if reroute is not None:
                verdict_role = reroute(issue)
                if verdict_role:
                    # A reviewer verdict re-opens a terminal read-only claim for
                    # one write dispatch (VED-365).
                    claim.terminal = False
                    claim.read_only = False
                    claim.rerouted = True
            if not claim.eligible(now, self.config.max_attempts) and verdict_role is None:
                continue
            if verdict_role:
                role = verdict_role
            else:
                role = route(snapshot.get("title", "")) if route else "coder"
            session_id = session_for(role) if session_for else ""
            if session_id in targeted:
                decisions.append(
                    Decision(action="skip", issue=issue, reason=f"unit {role} already busy")
                )
                continue
            if is_busy(session_id):
                decisions.append(
                    Decision(action="skip", issue=issue, reason=f"unit {role} still working")
                )
                continue
            claim.title = snapshot.get("title", claim.title)
            claim.role = role
            claim.session_id = session_id
            claim.attempt += 1
            claim.status = STATUS_DISPATCHED
            claim.dispatched_at = now
            claim.next_retry_at = 0.0
            claim.read_only = role in read_only_roles
            targeted.add(session_id)
            if claim.read_only:
                # A read-only audit is dispatched exactly once.
                claim.terminal = True
                claim.status = STATUS_TERMINAL
            else:
                self.running[session_id] = issue
                in_flight += 1
            decisions.append(
                Decision(
                    action="dispatch",
                    issue=issue,
                    role=role,
                    session_id=session_id,
                    reason=f"attempt {claim.attempt}",
                )
            )

        self.last_action = decisions
        return decisions

    def snapshot(self) -> dict:
        """Serialisable view of the claim table (for /swarm-style status)."""
        return {
            "maxConcurrentAgents": self.config.max_concurrent_agents,
            "inFlight": len(self.running),
            "claims": {issue: asdict(claim) for issue, claim in self.claims.items()},
        }


# ---------------------------------------------------------------------------
# Board + gateway adapters (I/O). Kept thin so the tick logic stays pure.
# ---------------------------------------------------------------------------

def fetch_board() -> dict[str, dict]:
    """Read the open + terminal cards for the project from Linear."""
    result = subprocess.run(
        ["linear", "issue", "list", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )
    nodes = json.loads(result.stdout)["issues"]["nodes"]
    board: dict[str, dict] = {}
    for node in nodes:
        state = node.get("state") or {}
        board[node["identifier"]] = {
            "title": node.get("title") or "",
            "state": state.get("name") or "",
            "stateType": state.get("type") or "",
        }
    return board


def fetch_units() -> dict[str, dict]:
    with urllib.request.urlopen(f"{GATEWAY}/swarm", timeout=5) as response:
        payload = json.load(response)
    return {unit["sessionId"]: unit for unit in payload["units"]}


def unit_is_busy(unit: dict | None, now: float) -> bool:
    if not unit:
        return False
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


def stop(session_id: str) -> None:
    """Abort a unit's current run (best-effort; reconcile already freed it)."""
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true", help="run a single tick and exit")
    parser.add_argument("--state", default=STATE_PATH)
    parser.add_argument(
        "--max-concurrent-agents", type=int, default=DEFAULT_MAX_CONCURRENT_AGENTS
    )
    args = parser.parse_args()

    # Imported lazily so the orchestrator stays usable without the conductor.
    import swarm_conductor as conductor

    config = OrchestratorConfig(max_concurrent_agents=args.max_concurrent_agents)
    orchestrator = Orchestrator.load(args.state, config)

    def tick() -> None:
        board = fetch_board()
        active = {
            issue
            for issue, snap in board.items()
            if snap.get("stateType") in ACTIVE_STATE_TYPES
        }
        if not orchestrator.claims:
            decisions = orchestrator.startup_cleanup(active)
            if decisions:
                print(f"startup cleanup: {[d.issue for d in decisions]}", flush=True)
        units = fetch_units()

        def busy(session_id: str) -> bool:
            return unit_is_busy(units.get(session_id), time.time())

        decisions = orchestrator.tick(
            board,
            now=time.time(),
            is_busy=busy,
            route=conductor.role_for,
            session_for=lambda role: conductor.ROLE_UNITS[role],
            read_only_roles=conductor.READ_ONLY_ROLES,
        )
        for decision in decisions:
            if decision.action == "dispatch":
                try:
                    dispatch(
                        decision.session_id,
                        conductor.prompt_for(decision.issue, board[decision.issue]["title"], decision.role),
                    )
                except OSError as error:
                    orchestrator.record_failure(decision.issue, time.time(), f"dispatch failed: {error}")
            elif decision.action == "stop":
                stop(decision.session_id)
            print(f"{decision.action} {decision.issue} {decision.reason}", flush=True)
        orchestrator.save(args.state)

    if args.once:
        tick()
        return
    print("orchestrator started", flush=True)
    while True:
        try:
            tick()
        except Exception as error:  # keep the daemon alive across transient errors
            print(f"orchestrator error: {error}", flush=True)
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    main()
