#!/usr/bin/env python3
"""Measure the interactive TUI's RSS via a pseudo-terminal.

The TUI only starts on a TTY, so this allocates a pty, spawns pi, samples RSS,
and kills it. Two taxonomies:

- `tui-idle`   : no session
- `tui-loaded` : a session JSONL loaded

Compare with the headless numbers from `mem_bench.py`.

Usage:
    python3 scripts/measure_tui.py
    python3 scripts/measure_tui.py --session path/to/session.jsonl --json artifacts/tui_loaded.json
"""

from __future__ import annotations

import argparse
import json
import os
import pty
import re
import signal
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path

ENV = {
    **os.environ,
    "PI_OFFLINE": "1",
    "PI_SKIP_VERSION_CHECK": "1",
    "NO_COLOR": "1",
}


def rss_mb(pid: int) -> float | None:
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        match = re.search(r"VmRSS:\s+(\d+) kB", status)
        return int(match.group(1)) / 1024 if match else None
    except OSError:
        return None


def measure(argv: list[str], settle: float, samples: int) -> list[float]:
    master, slave = pty.openpty()
    child = subprocess.Popen(
        argv,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=ENV,
        start_new_session=True,
    )
    os.close(slave)
    values: list[float] = []
    try:
        time.sleep(settle)
        for _ in range(samples):
            value = rss_mb(child.pid)
            if value is not None:
                values.append(value)
            time.sleep(1)
    finally:
        try:
            os.killpg(os.getpgid(child.pid), signal.SIGKILL)
        except OSError:
            child.kill()
        os.close(master)
    return values


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", help="session JSONL to load")
    parser.add_argument("--settle", type=float, default=6.0)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--json", help="write the result artifact here")
    args = parser.parse_args()

    argv = ["pi", "--session", args.session] if args.session else ["pi", "--no-session"]
    taxonomy = "tui-loaded" if args.session else "tui-idle"

    values = measure(argv, args.settle, args.samples)
    if not values:
        print("no samples collected; the TUI did not start", flush=True)
        return 1

    values.sort()
    median = values[len(values) // 2]
    print(f"{taxonomy}: median {median:.1f} MB  (min {values[0]:.1f}, max {values[-1]:.1f}, n={len(values)})")

    if args.json:
        artifact = {
            "schema": "pi.native.mem_bench.v1",
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "results": [
                {
                    "target": "pi-node",
                    "kind": "node",
                    "command": " ".join(argv),
                    "taxonomy": taxonomy,
                    "rss_bytes": int(median * 1048576),
                    "rss_min_bytes": int(values[0] * 1048576),
                    "rss_max_bytes": int(values[-1] * 1048576),
                    "sample_count": len(values),
                }
            ],
        }
        path = Path(args.json)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(artifact, indent=2) + "\n")
        print(f"artifact: {path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
