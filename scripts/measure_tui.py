#!/usr/bin/env python3
"""Measure the interactive TUI's idle RSS via a pseudo-terminal.

The TUI only starts on a TTY, so this allocates a pty, spawns pi, samples RSS,
and kills it. Compare with the headless (RPC) baseline from mem_bench.py.
"""

import os
import pty
import re
import signal
import subprocess
import time

ENV = {
    **os.environ,
    "PI_OFFLINE": "1",
    "PI_SKIP_VERSION_CHECK": "1",
    "NO_COLOR": "1",
}


def rss_mb(pid: int) -> float | None:
    try:
        status = open(f"/proc/{pid}/status").read()
        match = re.search(r"VmRSS:\s+(\d+) kB", status)
        return int(match.group(1)) / 1024 if match else None
    except OSError:
        return None


def main() -> int:
    master, slave = pty.openpty()
    child = subprocess.Popen(
        ["pi", "--no-session"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=ENV,
        start_new_session=True,
    )
    os.close(slave)

    samples = []
    try:
        # Let the TUI boot, then sample a few times.
        time.sleep(6)
        for _ in range(5):
            value = rss_mb(child.pid)
            if value is not None:
                samples.append(value)
            time.sleep(1)
    finally:
        try:
            os.killpg(os.getpgid(child.pid), signal.SIGKILL)
        except OSError:
            child.kill()
        os.close(master)

    if not samples:
        print("no samples (TUI did not start)")
        return 1
    samples.sort()
    print(f"TUI idle RSS: median {samples[len(samples) // 2]:.1f} MB  (samples {samples})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
