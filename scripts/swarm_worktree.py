#!/usr/bin/env python3
"""Isolated git worktrees for swarm work (Agent Orchestrator / Symphony style).

Both reference tools give every task its own workspace so parallel agents never
edit the same files. This is that workspace manager: it maps a task/issue id to
a deterministic worktree under a sibling directory, on a dedicated branch, and
keeps the mapping stable across runs.

The conductor (or a human) uses it before dispatching code-editing work:

    scripts/swarm_worktree.py create VED-348      # -> /…/pi-native-ws/VED-348
    scripts/swarm_worktree.py list
    scripts/swarm_worktree.py remove VED-348 --branch

A worker session must then be started with its cwd set to the printed path, so
every command the agent runs stays inside that workspace (Symphony's invariant
1). The main tree is never touched by a dispatched task.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess
import sys

REPO = pathlib.Path(os.environ.get("SWARM_REPO", "/home/vedaru/Projects/pi-native")).resolve()
ROOT = pathlib.Path(
    os.environ.get("SWARM_WORKTREE_ROOT", str(REPO.parent / f"{REPO.name}-ws"))
).resolve()
BRANCH_PREFIX = os.environ.get("SWARM_BRANCH_PREFIX", "swarm/")


def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=REPO, text=True, capture_output=True)


def path_for(work_id: str) -> pathlib.Path:
    return ROOT / work_id


def branch_for(work_id: str) -> str:
    return f"{BRANCH_PREFIX}{work_id}"


def create(work_id: str, base: str, force: bool) -> int:
    path = path_for(work_id)
    if path.exists():
        print(path)
        return 0
    path.parent.mkdir(parents=True, exist_ok=True)
    if force:
        git("branch", "-D", branch_for(work_id))
    result = git("worktree", "add", "-b", branch_for(work_id), str(path), base)
    if result.returncode != 0 and "already exists" in (result.stderr or ""):
        result = git("worktree", "add", str(path), branch_for(work_id))
    if result.returncode != 0:
        sys.stderr.write(result.stderr or result.stdout)
        return result.returncode
    print(path)
    return 0


def remove(work_id: str, force: bool, delete_branch: bool) -> int:
    args = ["worktree", "remove"]
    if force:
        args.append("--force")
    args.append(str(path_for(work_id)))
    result = git(*args)
    if result.returncode != 0:
        sys.stderr.write(result.stderr or result.stdout)
        return result.returncode
    if delete_branch:
        git("branch", "-D", branch_for(work_id))
    git("worktree", "prune")
    print(f"removed {path_for(work_id)}")
    return 0


def list_worktrees() -> int:
    result = git("worktree", "list", "--porcelain")
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        return result.returncode
    for block in result.stdout.strip().split("\n\n"):
        fields = dict(
            line.split(" ", 1) for line in block.splitlines() if " " in line
        )
        worktree = fields.get("worktree", "")
        branch = fields.get("branch", "").replace("refs/heads/", "")
        print(f"{branch or '(detached)'}\t{worktree}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    create_cmd = sub.add_parser("create", help="create (or reuse) a task worktree")
    create_cmd.add_argument("work_id")
    create_cmd.add_argument("--base", default="HEAD")
    create_cmd.add_argument("--force", action="store_true", help="reset the branch if it exists")

    remove_cmd = sub.add_parser("remove", help="remove a task worktree")
    remove_cmd.add_argument("work_id")
    remove_cmd.add_argument("--force", action="store_true")
    remove_cmd.add_argument("--branch", action="store_true", help="also delete the branch")

    sub.add_parser("list", help="list worktrees")
    sub.add_parser("root", help="print the workspace root")

    args = parser.parse_args()
    if args.command == "create":
        return create(args.work_id, args.base, args.force)
    if args.command == "remove":
        return remove(args.work_id, args.force, args.branch)
    if args.command == "list":
        return list_worktrees()
    if args.command == "root":
        print(ROOT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
