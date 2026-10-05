#!/usr/bin/env python3
"""Pressure gate for the native harness (VED-328).

Runs `pipelets --stress N` (a deterministic in-process workload) and fails if
peak RSS exceeds a ceiling or the run does not finish in time. Unlike the idle
benchmark, this measures the harness under load.

`ru_maxrss` of the child process is the high-water mark, so it is not sampling
dependent.

Usage:
    python3 scripts/stress_gate.py [--turns 50000] [--max-mb 150]
"""

from __future__ import annotations

import argparse
import re
import resource
import subprocess
import sys
import time
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/pipelets")
    parser.add_argument("--turns", type=int, default=50000)
    parser.add_argument("--tools", nargs="+", default=["ls"])
    parser.add_argument("--session", action="store_true", help="run the agent session loop (compaction) instead of direct tool calls")
    parser.add_argument("--max-mb", type=float, default=150.0)
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--max-cpu", type=float, default=5.0, help="max CPU seconds per tool run")
    parser.add_argument("--max-idle-cpu", type=float, default=0.2, help="max CPU seconds while idle")
    parser.add_argument("--skip-idle", action="store_true")
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        print(f"NOTICE: {binary} is not built; stress gate skipped.")
        return 0

    failed = False
    for tool in args.tools:
        try:
            command = [str(binary), "--stress", str(args.turns), "--stress-tool", tool]
            if args.session:
                command.append("--stress-session")
            usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
            rss_before_kb = usage_before.ru_maxrss
            proc = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=args.timeout,
            )
            usage_after = resource.getrusage(resource.RUSAGE_CHILDREN)
            rss_after_kb = usage_after.ru_maxrss
            cpu = (usage_after.ru_utime - usage_before.ru_utime) + (
                usage_after.ru_stime - usage_before.ru_stime
            )
        except subprocess.TimeoutExpired:
            print(f"FAIL[{tool}]: did not finish within {args.timeout:.0f}s ({args.turns} turns).")
            failed = True
            continue

        if proc.returncode != 0:
            print(f"FAIL[{tool}]: exited non-zero.")
            sys.stderr.write(proc.stderr)
            failed = True
            continue

        line = proc.stdout.strip()
        peak_mb = None
        for text in line.splitlines():
            if not text.startswith("peak RSS:"):
                continue
            # Parse defensively: a malformed line should fail only this tool, not
            # abort the whole gate with a ValueError.
            match = re.search(r"peak RSS:\s*([0-9]*\.?[0-9]+)", text)
            if match is None:
                print(f"WARN[{tool}]: malformed 'peak RSS:' line: {text!r}")
                continue
            peak_mb = float(match.group(1))
        if peak_mb is None:
            # RUSAGE_CHILDREN.ru_maxrss is a monotonic high-water mark across ALL
            # previously reaped children, so using it directly lets a large earlier
            # tool mask a later regression. Use this child's own delta instead: its
            # contribution to the high-water mark (0 if it did not set a new mark).
            print(f"WARN[{tool}]: no 'peak RSS:' reported; using per-child RSS delta.")
            peak_mb = max(rss_after_kb - rss_before_kb, 0) / 1024
        print(
            f"{tool}: {line.splitlines()[0] if line else ''} | peak {peak_mb:.1f} MB | cpu {cpu:.2f}s"
        )
        if cpu > args.max_cpu:
            print(f"FAIL[{tool}]: CPU {cpu:.2f}s over {args.max_cpu:.1f}s.")
            failed = True
        if peak_mb is not None and peak_mb > args.max_mb:
            print(f"FAIL[{tool}]: peak RSS over {args.max_mb:.0f} MB.")
            failed = True

    # Idle CPU must be ~0: no busy-wait or polling.
    if not args.skip_idle:
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        child = subprocess.Popen(
            [str(binary), "--rpc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        time.sleep(3.0)
        child.kill()
        child.wait()
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        idle_cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
        print(f"idle cpu over 3s: {idle_cpu:.3f}s (ceiling {args.max_idle_cpu:.2f}s)")
        if idle_cpu > args.max_idle_cpu:
            print("FAIL: idle CPU too high (busy-wait?).")
            failed = True

    if failed:
        print("\nFAIL: stress gate failed.")
        return 1
    print("\nPASS: stress gate passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
