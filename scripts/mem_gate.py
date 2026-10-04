#!/usr/bin/env python3
"""Memory regression gate (VED-317).

Runs the memory benchmark and fails when a target's RSS regresses past a
threshold against the committed baseline (`artifacts/mem_bench.json`).

Designed to be safe in CI where no pi binary is installed: if no targets are
available, it prints a notice and exits 0 so the job is a no-op rather than a
false failure.

Usage:
    python3 scripts/mem_gate.py [--threshold 0.10]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

DEFAULT_THRESHOLD = 0.10


def project_root() -> Path:
    return Path(__file__).resolve().parents[1]


def load_json(path: Path) -> dict | None:
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def index_results(artifact: dict | None) -> dict[tuple[str, str], int]:
    indexed: dict[tuple[str, str], int] = {}
    if not artifact:
        return indexed
    for result in artifact.get("results", []):
        key = (result.get("target", ""), result.get("taxonomy", ""))
        rss = result.get("rss_bytes")
        if isinstance(rss, int) and rss > 0:
            indexed[key] = rss
    return indexed


def human_mb(value: int) -> str:
    return f"{value / 1048576:.1f} MB"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    args = parser.parse_args()

    root = project_root()
    baseline_path = root / "artifacts" / "mem_bench.json"
    current_path = root / "artifacts" / "mem_bench.current.json"

    baseline = index_results(load_json(baseline_path))
    if not baseline:
        print("No committed baseline; memory gate skipped.")
        return 0

    run = subprocess.run(
        [
            sys.executable,
            str(root / "scripts" / "mem_bench.py"),
            "--all",
            "--json",
            str(current_path),
        ],
        capture_output=True,
        text=True,
    )
    if run.returncode != 0 and "Unknown/unavailable targets" in (run.stderr or ""):
        print("No benchmark targets available in this environment; memory gate skipped.")
        return 0
    if run.returncode != 0:
        sys.stderr.write(run.stdout)
        sys.stderr.write(run.stderr)
        return run.returncode

    current = index_results(load_json(current_path))
    regressions: list[str] = []
    comparisons: list[str] = []
    for key, rss in current.items():
        if key not in baseline:
            continue
        previous = baseline[key]
        delta = (rss - previous) / previous
        line = (
            f"{key[0]} [{key[1]}]: {human_mb(previous)} -> {human_mb(rss)} "
            f"({delta * 100:+.1f}%)"
        )
        comparisons.append(line)
        if delta > args.threshold:
            regressions.append(line)

    print("Memory gate:")
    for line in comparisons:
        print(f"  {line}")

    if not comparisons:
        print(
            "\nFAIL: baseline exists but no (target, taxonomy) keys overlap the "
            "current run; refusing to pass vacuously."
        )
        return 1

    if regressions:
        print(f"\nFAIL: {len(regressions)} regression(s) over {args.threshold * 100:.0f}%:")
        for line in regressions:
            print(f"  {line}")
        return 1

    print("\nPASS: no regression over threshold.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
