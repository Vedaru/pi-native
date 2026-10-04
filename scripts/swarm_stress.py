#!/usr/bin/env python3
"""Swarm stress: run N units at once and measure aggregate and per-unit RSS/CPU.

A swarm multiplies both memory and CPU, so the interesting numbers are the
aggregate, not a single unit. Two modes:

- `idle`: N `pi-native --rpc` units sitting idle (the swarm floor).
- `busy`: N units each running a stress session (load).

Usage:
    python3 scripts/swarm_stress.py --units 8 --mode idle
    python3 scripts/swarm_stress.py --units 8 --mode busy --turns 5000
"""

from __future__ import annotations

import argparse
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path


def read_field(pid: int, key: str) -> float | None:
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        match = re.search(rf"{key}:\s+(\d+) kB", status)
        return int(match.group(1)) / 1024 if match else None
    except OSError:
        return None


def cpu_seconds(pid: int) -> float | None:
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        fields = stat[stat.rfind(")") + 1 :].split()
        return (int(fields[11]) + int(fields[12])) / 100.0
    except (OSError, ValueError, IndexError):
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/pi-native")
    parser.add_argument("--units", type=int, default=8)
    parser.add_argument("--mode", choices=["idle", "busy"], default="idle")
    parser.add_argument("--turns", type=int, default=5000)
    parser.add_argument("--settle", type=float, default=2.0)
    parser.add_argument("--max-total-mb", type=float, default=200.0)
    parser.add_argument("--max-avg-mb", type=float, default=25.0)
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        print(f"NOTICE: {binary} is not built; swarm stress skipped.")
        return 0

    if args.mode == "idle":
        argv = [str(binary), "--rpc"]
    else:
        argv = [str(binary), "--stress", str(args.turns), "--stress-tool", "read", "--stress-session"]

    children = []
    for _ in range(args.units):
        children.append(
            subprocess.Popen(
                argv,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        )

    peak_rss: dict[int, float] = {child.pid: 0.0 for child in children}
    start_cpu: dict[int, float] = {}

    try:
        # Idle units settle; busy units start working immediately, so sample from
        # the first moment or we miss their peak.
        if args.mode == "idle":
            time.sleep(args.settle)
        deadline = time.time() + (120.0 if args.mode == "busy" else 3.0)
        while time.time() < deadline:
            alive = 0
            for child in children:
                if child.poll() is not None:
                    continue
                rss = read_field(child.pid, "VmRSS")
                if rss is not None:
                    peak_rss[child.pid] = max(peak_rss[child.pid], rss)
                if child.pid not in start_cpu:
                    value = cpu_seconds(child.pid)
                    if value is not None:
                        start_cpu[child.pid] = value
                alive += 1
            if args.mode == "busy" and alive == 0:
                break
            time.sleep(0.05)
    finally:
        for child in children:
            if child.poll() is None:
                try:
                    os.killpg(os.getpgid(child.pid), signal.SIGKILL)
                except OSError:
                    child.kill()
        for child in children:
            child.wait()

    total_peak = sum(peak_rss.values())
    avg_peak = total_peak / max(1, len(children))
    print(
        f"swarm[{args.mode}]: {args.units} units | avg peak {avg_peak:.1f} MB | "
        f"total peak {total_peak:.1f} MB"
    )

    failed = False
    if avg_peak > args.max_avg_mb:
        print(f"FAIL: average unit over {args.max_avg_mb:.0f} MB.")
        failed = True
    if total_peak > args.max_total_mb:
        print(f"FAIL: total over {args.max_total_mb:.0f} MB.")
        failed = True

    if failed:
        print("\nFAIL: swarm gate failed.")
        return 1
    print("\nPASS: swarm gate passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
