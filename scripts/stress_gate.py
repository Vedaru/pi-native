#!/usr/bin/env python3
"""Pressure gate for the native harness (VED-328).

Runs `pi-native --stress N` (a deterministic in-process workload) and fails if
peak RSS exceeds a ceiling or the run does not finish in time. Unlike the idle
benchmark, this measures the harness under load.

`ru_maxrss` of the child process is the high-water mark, so it is not sampling
dependent.

Usage:
    python3 scripts/stress_gate.py [--turns 50000] [--max-mb 150]
"""

from __future__ import annotations

import argparse
import resource
import subprocess
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/pi-native")
    parser.add_argument("--turns", type=int, default=50000)
    parser.add_argument("--max-mb", type=float, default=150.0)
    parser.add_argument("--timeout", type=float, default=60.0)
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        print(f"NOTICE: {binary} is not built; stress gate skipped.")
        return 0

    try:
        proc = subprocess.run(
            [str(binary), "--stress", str(args.turns)],
            capture_output=True,
            text=True,
            timeout=args.timeout,
        )
    except subprocess.TimeoutExpired:
        print(f"FAIL: stress did not finish within {args.timeout:.0f}s ({args.turns} turns).")
        return 1

    peak_kb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    peak_mb = peak_kb / 1024

    if proc.stdout.strip():
        print(proc.stdout.strip())
    print(f"peak RSS: {peak_mb:.1f} MB (ceiling {args.max_mb:.0f} MB, {args.turns} turns)")

    if proc.returncode != 0:
        print("FAIL: stress exited non-zero.")
        sys.stderr.write(proc.stderr)
        return 1
    if peak_mb > args.max_mb:
        print(f"FAIL: peak RSS over {args.max_mb:.0f} MB.")
        return 1

    print("PASS: stress gate passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
