#!/usr/bin/env python3
"""Profile a native unit across the benchmark states and print a table.

States:
  idle              fresh `--rpc` unit, settled
  tool-heavy        long tool session (streamed output)
  compaction-heavy  the same with a small context window (frequent compaction)

Reports peak RSS and user+sys CPU for each. Complements mem_bench.py (which
compares targets) by covering the *loaded* native states.

Usage:
    python3 scripts/native_states.py [--binary target/release/pi-native] [--turns 200000]
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import time
from pathlib import Path


def run_stress(binary: Path, turns: int, tool: str, context_tokens: int) -> dict[str, float]:
    command = [
        str(binary),
        "--stress",
        str(turns),
        "--stress-tool",
        tool,
        "--stress-session",
        "--stress-context-tokens",
        str(context_tokens),
    ]
    proc = subprocess.run(command, capture_output=True, text=True, timeout=600)
    out = proc.stdout
    peak = re.search(r"peak RSS: ([0-9.]+) MB", out)
    user = re.search(r"cpu: user ([0-9.]+)s", out)
    sys_ = re.search(r"sys ([0-9.]+)s", out)
    return {
        "rss": float(peak.group(1)) if peak else 0.0,
        "cpu": (float(user.group(1)) if user else 0.0)
        + (float(sys_.group(1)) if sys_ else 0.0),
    }


def idle_rss(binary: Path) -> float:
    child = subprocess.Popen(
        [str(binary), "--rpc"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(2.0)
    rss = 0.0
    try:
        status = Path(f"/proc/{child.pid}/status").read_text()
        match = re.search(r"VmRSS:\s+(\d+) kB", status)
        if match:
            rss = int(match.group(1)) / 1024
    except OSError:
        pass
    finally:
        child.kill()
        child.wait()
    return rss


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/pi-native")
    parser.add_argument("--turns", type=int, default=200000)
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        print(f"NOTICE: {binary} is not built; native states skipped.")
        return 0

    rows = [
        ("idle", idle_rss(binary), 0.0),
    ]
    for label, tool, window in [
        ("tool-heavy (read)", "read", 200000),
        ("tool-heavy (grep)", "grep", 200000),
        ("compaction-heavy", "read", 64000),
    ]:
        result = run_stress(binary, args.turns, tool, window)
        rows.append((label, result["rss"], result["cpu"]))

    print(f"native states ({args.turns} turns where applicable):\n")
    print(f"{'state':22} {'RSS':>8} {'CPU':>8}")
    for label, rss, cpu in rows:
        print(f"{label:22} {rss:7.1f}M {cpu:7.2f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
