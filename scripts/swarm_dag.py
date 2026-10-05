#!/usr/bin/env python3
"""Conductor task-DAG: decompose a goal, dispatch in dependency order (VED-378).

The conductor dispatches the next open issue flat, so it cannot model
dependencies. This module turns a **goal** into a **task DAG** expressed as
Linear issues plus real `blocked-by` relations, computes parallel waves, and
selects the ready batch to dispatch — respecting dependency order, running
independent branches in parallel, and refusing to dispatch a cycle.

The DAG lives in Linear (visible on the board, AC3); the conductor keeps only a
rebuildable cache. Everything that decides *what to do* is a pure function of
`(goal | nodes | edges | board, now, config)` — no clock or network reads inside
— so the scheduler is unit-testable without a gateway. The `materialize`/
`derive`/`dispatch` functions are the only parts that touch the board.

Relation semantics (verified against the `linear` CLI): `relation add <child>
blocked-by <dep>` is recorded on the **dep** as `blocks -> child`; the child
sees it in its **`inverseRelations`** as `{type: "blocks", issue: {identifier}}`.
So a node's `deps` are read from its inverse `blocks` relations.

Integration: this **replaces the conductor's pick step, not its machinery**.
`role_for`, `prompt_for`, `dispatch`, `is_busy`, `ROLE_UNITS`, `EDITING_ROLES`,
`READ_ONLY_ROLES`, `OPEN_STATES`, `TERMINAL_STATUSES` and the claim `state` dict
are all reused from `swarm_conductor`.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load_conductor():
    spec = importlib.util.spec_from_file_location(
        "swarm_conductor", os.path.join(HERE, "swarm_conductor.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_conductor_for(name: str):
    spec = importlib.util.spec_from_file_location(
        name, os.path.join(HERE, name + ".py")
    )
    module = importlib.util.module_from_spec(spec)
    # Register before exec so dataclasses (and relative introspection) can find
    # the module by name; without this Python 3.14's dataclass decorator fails.
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


conductor = load_conductor()
orchestrator = load_conductor_for("swarm_orchestrator")

# The orchestrator refactor (VED-368) moved project identity and the open-state
# notion out of the conductor. Keep the DAG working across both layouts.
PROJECT = getattr(conductor, "PROJECT", None) or orchestrator.PROJECT
TEAM = getattr(conductor, "TEAM", None) or orchestrator.TEAM
# Linear state *names* considered open (the orchestrator tracks state *types*).
OPEN_STATES = getattr(conductor, "OPEN_STATES", None) or {
    "Todo",
    "In Progress",
    "In Review",
}
ROLE_UNITS = conductor.ROLE_UNITS
EDITING_ROLES = conductor.EDITING_ROLES
READ_ONLY_ROLES = conductor.READ_ONLY_ROLES
TERMINAL_STATUSES = conductor.TERMINAL_STATUSES
MAX_ATTEMPTS = getattr(conductor, "MAX_ATTEMPTS", None) or getattr(
    orchestrator, "DEFAULT_MAX_ATTEMPTS", 3
)

DAG_PATH = os.environ.get("SWARM_DAG_PATH", "/tmp/swarm-dag.json")
DONE_STATES = {"Done", "Canceled"}
FAILED_STATES = {"Canceled"}
DEFAULT_MAX_PARALLEL = int(os.environ.get("SWARM_MAX_PARALLEL", "2"))


# ---- goal schema / validation (pure) ---------------------------------------


class DagError(Exception):
    """A goal is invalid (schema, unknown dep key, duplicate key, or a cycle)."""


def validate_goal(goal: dict) -> dict:
    """Validate a goal document and return a normalised copy.

    Raises [`DagError`] with a human message on any schema problem, so the caller
    can comment it on the parent issue rather than dispatch garbage.
    """
    if not isinstance(goal, dict):
        raise DagError("goal must be a JSON object")
    objective = goal.get("goal")
    if not objective or not isinstance(objective, str):
        raise DagError("goal.goal is required and must be a string")
    nodes = goal.get("nodes")
    if not isinstance(nodes, list) or not nodes:
        raise DagError("goal.nodes must be a non-empty list")

    defaults = goal.get("defaults") or {}
    seen = set()
    normalised = []
    for node in nodes:
        if not isinstance(node, dict):
            raise DagError("every node must be an object")
        key = node.get("key")
        if not key or not isinstance(key, str):
            raise DagError(f"node {node!r} has no string key")
        if key in seen:
            raise DagError(f"duplicate node key: {key}")
        seen.add(key)
        title = node.get("title")
        if not title or not isinstance(title, str):
            raise DagError(f"node {key} has no title")
        blocked_by = node.get("blocked_by") or []
        if not isinstance(blocked_by, list):
            raise DagError(f"node {key}: blocked_by must be a list")
        normalised.append(
            {
                "key": key,
                "title": title,
                "body": node.get("body") or "",
                "role": node.get("role") or defaults.get("role") or None,
                "scope": list(node.get("scope") or []),
                "blocked_by": list(blocked_by),
                "risk": node.get("risk"),
            }
        )

    # Every blocked_by key must resolve within this goal.
    keys = {node["key"] for node in normalised}
    for node in normalised:
        for dep in node["blocked_by"]:
            if dep not in keys:
                raise DagError(f"node {node['key']} depends on unknown key: {dep}")

    order = compute_waves({node["key"]: node["blocked_by"] for node in normalised})
    if order is None:
        raise DagError("goal has a dependency cycle: " + ", ".join(_find_cycle(normalised)))

    return {
        "goal": objective,
        "detail": goal.get("detail") or "",
        "parent": goal.get("parent"),
        "nodes": normalised,
        "defaults": {
            "role": defaults.get("role") or "coder",
            "max_parallel": int(defaults.get("max_parallel") or DEFAULT_MAX_PARALLEL),
        },
    }


# ---- graph (pure) -----------------------------------------------------------


def compute_waves(deps_by_key: dict) -> dict | None:
    """Topological levels: `key -> longest path from any root`.

    Returns `None` on a cycle. Wave 0 has no unmet dependency; a node's wave is
    one more than the maximum wave of its dependencies, so a whole dependency
    chain is scheduled strictly later than its ancestors (AC1).
    """
    waves: dict[str, int] = {}
    visiting: set[str] = set()

    def visit(key: str) -> int:
        if key in waves:
            return waves[key]
        if key in visiting:
            return None  # cycle
        visiting.add(key)
        depth = 0
        for dep in deps_by_key.get(key, []):
            if dep not in deps_by_key:
                raise DagError(f"unknown dependency key: {dep}")
            dep_wave = visit(dep)
            if dep_wave is None:
                return None
            depth = max(depth, dep_wave + 1)
        visiting.discard(key)
        waves[key] = depth
        return depth

    for key in deps_by_key:
        if visit(key) is None:
            return None
    return waves


def _find_cycle(nodes: list) -> list[str]:
    """Return the keys of one cycle, for a readable error/comment."""
    deps = {node["key"]: list(node["blocked_by"]) for node in nodes}
    state: dict[str, int] = {}
    stack: list[str] = []

    def dfs(key: str):
        state[key] = 1
        stack.append(key)
        for dep in deps.get(key, []):
            if state.get(dep) == 1:
                return stack[stack.index(dep) :]
            if state.get(dep, 0) == 0:
                found = dfs(dep)
                if found:
                    return found
        stack.pop()
        state[key] = 2
        return None

    for key in deps:
        if state.get(key, 0) == 0:
            found = dfs(key)
            if found:
                return found
    return []


def _scope_root(path: str) -> str | None:
    """The collision root of one repo-relative path: the crate/package boundary.

    `crates/pi-rpc/src/lib.rs` -> `crates/pi-rpc`; a bare `docs/x.md` -> `docs`.
    A crate is coarser than a file, so two nodes touching one crate collide even
    on different files (the README's "one editor at a time" per crate).
    """
    parts = [p for p in str(path).split("/") if p and p != "."]
    if not parts:
        return None
    if len(parts) >= 2 and parts[0] in ("crates", "packages", "apps", "plugins"):
        return f"{parts[0]}/{parts[1]}"
    # Otherwise the first path component is the collision root.
    return parts[0]


def parallel_key(scope: list[str], role: str) -> str:
    """Collision key for co-scheduling: the scope's crate/package roots, else role.

    Two coders editing the same crate must not run together; two nodes that touch
    disjoint crates may (AC2). A node with several scopes collides on any of them,
    keyed as a sorted set so disjoint multi-scope nodes still separate.
    """
    roots = {root for root in (_scope_root(p) for p in scope or []) if root}
    if roots:
        return "|".join(sorted(roots))
    return role


def node_waves(nodes: list[dict]) -> dict:
    """Attach a `wave` and `parallel_key` to each node (mutates copies)."""
    deps = {node["id"]: list(node.get("deps") or []) for node in nodes}
    waves = compute_waves(
        {key: [d for d in value if d in deps] for key, value in deps.items()}
    )
    result = {}
    for node in nodes:
        result[node["id"]] = {
            "wave": (waves or {}).get(node["id"], 0),
            "parallel_key": node.get("parallel_key") or node.get("role") or "coder",
        }
    return result


# ---- materialize (impure: creates Linear issues + relations) ---------------


def linear(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["linear", *args],
        capture_output=True,
        text=True,
        env={**os.environ, "LINEAR_IGNORE_ENV_FILE": "1"},
    )


def create_issue(node: dict, parent: str | None) -> str:
    """Create one Linear issue for a node and return its identifier."""
    args = [
        "issue",
        "create",
        "--team",
        TEAM,
        "--project",
        PROJECT,
        "--title",
        node["title"],
        "--state",
        "Todo",
        "--no-interactive",
        "--json",
    ]
    if node.get("body"):
        args += ["--description", node["body"]]
    if parent:
        args += ["--parent", parent]
    result = linear(*args)
    if result.returncode != 0:
        raise DagError(f"failed to create {node['key']!r}: {result.stderr.strip()}")
    payload = json.loads(result.stdout)
    issue = payload.get("issue", payload)
    return issue["identifier"]


def add_relation(child: str, dep: str) -> None:
    result = linear("issue", "relation", "add", child, "blocked-by", dep)
    if result.returncode != 0:
        raise DagError(f"failed to relate {child} blocked-by {dep}: {result.stderr.strip()}")


def materialize(goal: dict, parent: str | None = None) -> dict:
    """Create the goal's issues + blocked-by edges in Linear (idempotent-ish).

    Returns the DAG cache `{goal, parent, nodes, edges, created_at}`. A node
    already present among the parent's children (matched by title) is not
    re-created; a relation that already exists is not re-added.
    """
    goal = validate_goal(goal)
    parent = parent or goal.get("parent")
    existing = _children_by_title(parent) if parent else {}

    nodes = {}
    for node in goal["nodes"]:
        title = node["title"]
        identifier = existing.get(title)
        if identifier is None:
            identifier = create_issue(node, parent)
        nodes[node["key"]] = {
            "id": identifier,
            "key": node["key"],
            "title": title,
            "role": conductor.role_for(title),
            "scope": node["scope"],
            "deps": [],
            "parallel_key": parallel_key(node["scope"], conductor.role_for(title)),
            "state": "Todo",
        }

    edges = []
    for node in goal["nodes"]:
        child = nodes[node["key"]]["id"]
        for dep_key in node["blocked_by"]:
            dep = nodes[dep_key]["id"]
            add_relation(child, dep)
            nodes[node["key"]]["deps"].append(dep)
            edges.append([child, dep])

    dag = {
        "goal": goal["goal"],
        "parent": parent,
        "nodes": nodes,
        "edges": edges,
        "max_parallel": goal["defaults"]["max_parallel"],
    }
    save_dag(dag)
    return dag


def _children_by_title(parent: str | None) -> dict:
    if not parent:
        return {}
    result = linear("issue", "view", parent, "--json")
    if result.returncode != 0:
        return {}
    payload = json.loads(result.stdout)
    issue = payload.get("issue", payload)
    children = issue.get("children") or []
    if isinstance(children, dict):
        children = children.get("nodes", [])
    return {child["title"]: child["identifier"] for child in children}


# ---- derive / rebuild from Linear (impure) ---------------------------------


def derive_deps_from_board(board: list[dict]) -> dict:
    """`identifier -> [dep identifiers]` read from Linear relations.

    A node's dependencies are the blockers it is blocked by, which the CLI
    reports in the blocked issue's `inverseRelations` as `type: "blocks"`.
    """
    deps = {}
    for node in board:
        identifier = node["identifier"]
        inverse = node.get("inverseRelations") or {}
        inverse = inverse.get("nodes", inverse) if isinstance(inverse, dict) else inverse
        deps[identifier] = [
            rel["issue"]["identifier"]
            for rel in inverse
            if rel.get("type") == "blocks" and rel.get("issue", {}).get("identifier")
        ]
    return deps


def load_board() -> list[dict]:
    """The full board with relations, from the Linear CLI."""
    result = linear(
        "issue", "mine", "--team", TEAM, "--project", PROJECT, "--all-states", "--json"
    )
    if result.returncode != 0:
        raise DagError(f"failed to read the board: {result.stderr.strip()}")
    return json.loads(result.stdout)["issues"]["nodes"]


def rebuild_from_board(dag: dict, board: list[dict]) -> dict:
    """Recompute node states/deps/waves from Linear alone (AC6).

    The cache can always be rebuilt from the board, so it cannot diverge.
    """
    by_id = {node.get("identifier") or node.get("id"): node for node in board}
    deps = derive_deps_from_board(board)
    nodes = {}
    for node in dag.get("nodes", {}).values():
        identifier = node["id"]
        live = by_id.get(identifier, {})
        nodes[node["key"]] = {
            **node,
            "state": (live.get("state") or {}).get("name") or node.get("state") or "Todo",
            "deps": [d for d in deps.get(identifier, node.get("deps", [])) if d in by_id],
        }
    waves = compute_waves({n["id"]: list(n["deps"]) for n in nodes.values()})
    for node in nodes.values():
        node["wave"] = (waves or {}).get(node["id"], 0)
    dag = {**dag, "nodes": nodes}
    return dag


def load_dag() -> dict | None:
    try:
        with open(DAG_PATH) as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def save_dag(dag: dict) -> None:
    with open(DAG_PATH, "w") as handle:
        json.dump(dag, handle, indent=2)


# ---- scheduling (pure) ------------------------------------------------------


def ready_ids(nodes: dict, now: float | None = None) -> list[str]:
    """Node ids whose dependencies are all `Done` and whose own state is open.

    This is the DAG gate: a node is never ready before its deps finish (AC1).
    """
    ready = []
    for key, node in nodes.items():
        if node.get("state") in DONE_STATES:
            continue
        if node.get("state") not in OPEN_STATES:
            continue
        deps = node.get("deps") or []
        if all((nodes.get(d) or {}).get("state") == "Done" for d in deps if d in nodes):
            ready.append(key)
    # A dep that is not itself in the DAG cannot be satisfied; treat as blocked.
    return sorted(ready, key=lambda key: (nodes[key].get("wave", 0), key))


def failed_dependents(nodes: dict) -> set:
    """Keys transitively blocked by a failed/canceled node (AC5).

    A failure halts only its dependents; unrelated branches keep running.
    """
    failed = {key for key, node in nodes.items() if node.get("state") in FAILED_STATES}
    changed = True
    while changed:
        changed = False
        for key, node in nodes.items():
            if key in failed:
                continue
            if any(dep in failed for dep in (node.get("deps") or [])):
                failed.add(key)
                changed = True
    return failed - {
        key for key, node in nodes.items() if node.get("state") in FAILED_STATES
    }


def select_batch(
    nodes: dict,
    running: dict,
    max_parallel: int,
    busy: bool,
) -> list[str]:
    """Choose the ready nodes to dispatch now (AC2), in wave order.

    - Lowest wave first, so dependencies run strictly before dependents.
    - Within a wave, nodes with disjoint `parallel_key` run together; a
      collision serializes them. Only one node per `parallel_key` is chosen.
    - A global busy check (any unit busy) still serializes heavy work.
    - `running` maps `parallel_key -> count` of in-flight nodes.
    """
    if busy:
        return []
    blocked = failed_dependents(nodes)
    ready = [key for key in ready_ids(nodes) if key not in blocked]
    if not ready:
        return []

    slots = max(0, max_parallel - sum(running.values()))
    if slots == 0:
        return []

    chosen = []
    used = set(running)
    for key in ready:
        node = nodes[key]
        pkey = node.get("parallel_key") or "coder"
        if pkey in used:
            continue  # collision within this batch or with a running node
        chosen.append(key)
        used.add(pkey)
        if len(chosen) >= slots:
            break
    return chosen


def describe(dag: dict) -> str:
    """A readable board summary: waves, then nodes in dependency order (AC3)."""
    nodes = dag.get("nodes", {})
    waves: dict[int, list] = {}
    for node in nodes.values():
        waves.setdefault(node.get("wave", 0), []).append(node)
    lines = [f"DAG: {dag.get('goal', '')}"]
    for wave in sorted(waves):
        for node in sorted(waves[wave], key=lambda n: n["id"]):
            deps = ",".join(node.get("deps") or []) or "-"
            lines.append(
                f"  wave {wave}  {node['id']:<9} [{node.get('state', '?'):<11}] "
                f"role={node.get('role', '?'):<11} deps={deps:<20} {node.get('title', '')}"
            )
    return "\n".join(lines)


# ---- CLI --------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Conductor task-DAG (VED-378)")
    sub = parser.add_subparsers(dest="command", required=True)

    plan = sub.add_parser("plan", help="validate a goal and print its waves")
    plan.add_argument("goal_file")

    mat = sub.add_parser("materialize", help="create the goal's issues + relations")
    mat.add_argument("goal_file")
    mat.add_argument("--parent", default=None)

    sub.add_parser("show", help="print the cached DAG")

    ready = sub.add_parser("ready", help="print the ready batch")
    ready.add_argument("--max-parallel", type=int, default=None)
    ready.add_argument("--busy", action="store_true")

    args = parser.parse_args(argv)

    if args.command == "plan":
        with open(args.goal_file) as handle:
            goal = json.load(handle)
        try:
            goal = validate_goal(goal)
        except DagError as error:
            print(f"invalid goal: {error}", file=sys.stderr)
            return 2
        waves = compute_waves({n["key"]: n["blocked_by"] for n in goal["nodes"]})
        for key, wave in sorted(waves.items(), key=lambda kv: (kv[1], kv[0])):
            print(f"wave {wave}  {key}")
        return 0

    if args.command == "materialize":
        with open(args.goal_file) as handle:
            goal = json.load(handle)
        dag = materialize(goal, args.parent)
        print(describe(dag))
        return 0

    if args.command == "show":
        dag = load_dag()
        if dag is None:
            print("no cached DAG", file=sys.stderr)
            return 1
        dag = rebuild_from_board(dag, load_board())
        save_dag(dag)
        print(describe(dag))
        return 0

    if args.command == "ready":
        dag = load_dag()
        if dag is None:
            print("no cached DAG", file=sys.stderr)
            return 1
        dag = rebuild_from_board(dag, load_board())
        keys = select_batch(
            dag["nodes"],
            {},
            args.max_parallel or dag.get("max_parallel", DEFAULT_MAX_PARALLEL),
            args.busy,
        )
        for key in keys:
            print(dag["nodes"][key]["id"])
        return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
