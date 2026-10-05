#!/usr/bin/env python3
"""Receipt producer for the swarm merge queue (VED-377).

Today the "gate" is three shell commands embedded in the conductor's dispatch
prompt: nothing writes a machine-readable result, so a merge queue cannot know
whether a branch actually passed. This module runs the gate and writes a
**receipt** bound to the exact ``git rev-parse HEAD`` it ran against:

    {
      "sha": "<head at gate time>",
      "fmt": true, "clippy": true,
      "test": {"passed": N, "failed": 0},
      "gate": "green",
      "at": 1730000000.0
    }

`gate` is green only when all three pass. A receipt is only valid for the commit
it records, so a branch that moves after gating must be re-gated (VED-377 AC1).

Usage:

    scripts/swarm_gate.py [--cwd <dir>] [--json <path>] [--skip-tests]

With no ``--cwd`` the gate runs in the current directory. ``--json -`` prints the
receipt; otherwise it is written to ``--json`` (default: stdout only).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import time

FMT_CHECK_COMMAND = ["cargo", "fmt", "--all", "--", "--check"]
CLIPPY_COMMAND = ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]
TEST_COMMAND = ["cargo", "test", "--workspace"]

_TEST_RESULT_RE = re.compile(r"test result:\s*(?:ok|FAILED)\.\s*(\d+) passed;\s*(\d+) failed")


def _run(command: list[str], cwd: str, executor=subprocess.run) -> subprocess.CompletedProcess:
    return executor(command, cwd=cwd, capture_output=True, text=True)


def head_sha(cwd: str, executor=subprocess.run) -> str:
    """The commit the receipt is bound to."""
    result = _run(["git", "rev-parse", "HEAD"], cwd, executor)
    return (result.stdout or "").strip()


def parse_test_counts(output: str) -> tuple[int, int]:
    """Sum `test result:` lines into (passed, failed).

    A workspace run prints one line per test binary; partial output still yields
    what it can, defaulting to zero.
    """
    passed = failed = 0
    for passed_n, failed_n in _TEST_RESULT_RE.findall(output or ""):
        passed += int(passed_n)
        failed += int(failed_n)
    return passed, failed


def run_gate(
    cwd: str,
    executor=subprocess.run,
    skip_tests: bool = False,
    now: float | None = None,
) -> dict:
    """Run the gate in ``cwd`` and return a receipt bound to HEAD.

    ``executor`` is injectable so tests can supply canned command results.
    """
    sha = head_sha(cwd, executor)

    fmt = _run(FMT_CHECK_COMMAND, cwd, executor).returncode == 0
    clippy = _run(CLIPPY_COMMAND, cwd, executor).returncode == 0
    if skip_tests:
        test = {"passed": 0, "failed": 0}
        tests_ok = True
    else:
        test_result = _run(TEST_COMMAND, cwd, executor)
        test = dict(zip(("passed", "failed"), parse_test_counts(test_result.stdout or "")))
        tests_ok = test_result.returncode == 0

    green = fmt and clippy and tests_ok
    return {
        "sha": sha,
        "fmt": fmt,
        "clippy": clippy,
        "test": test,
        # A red run is still a receipt; `gate` is the summary consumers key on.
        "gate": "green" if green else "red",
        "at": now if now is not None else time.time(),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cwd", default=".", help="directory to gate (default: cwd)")
    parser.add_argument("--json", default="-", help="receipt output path, or - for stdout")
    parser.add_argument("--skip-tests", action="store_true", help="fmt+clippy only (fast)")
    args = parser.parse_args(argv)

    receipt = run_gate(args.cwd, skip_tests=args.skip_tests)
    payload = json.dumps(receipt, indent=2)
    if args.json == "-":
        print(payload)
    else:
        with open(args.json, "w") as handle:
            handle.write(payload)
        print(args.json)
    return 0 if receipt["gate"] == "green" else 1


if __name__ == "__main__":
    raise SystemExit(main())
