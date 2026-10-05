#!/usr/bin/env python3
"""Canonical runtime-memory benchmark for the pi native-runtime project (VED-302).

Measures resident set size (RSS) of a pi runtime after it reaches a steady idle
state. Used to compare:

  - pi-node    : the current Node/V8 implementation
  - pi-rust    : the reference native port (Dicklesworthstone/pi_agent_rust)
  - pipelets   : our build, once it exists

The zero point is a fixed idle taxonomy so runs are comparable. Only
`cold-idle` is implemented today; the remaining taxonomies are declared so the
artifact schema does not churn when they land:

  1. cold-idle          : just spawned, no model loaded, no session
  2. warm-idle          : after extension warmup
  3. post-conversation  : after a model turn, then idle
  4. post-compaction    : after compaction, then idle
  5. post-tool-heavy    : after heavy tool output, then idle

Usage:
    python3 scripts/mem_bench.py --all
    python3 scripts/mem_bench.py --target pi-node
    python3 scripts/mem_bench.py --target pi-rust --settle 5 --samples 5
    python3 scripts/mem_bench.py --all --json artifacts/mem_bench.json

Exit codes:
    0  artifact written, schema valid, samples taken
    1  setup error (target missing, bad arguments)
    2  insufficient samples (process died or never settled)
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "pi.native.mem_bench.v1"
DEFAULT_SETTLE_SECONDS = 5.0
DEFAULT_SAMPLE_INTERVAL_SECONDS = 1.0
DEFAULT_SAMPLE_COUNT = 5
IDLE_TAXONOMY = "cold-idle"
TAXONOMIES = [
    "cold-idle",
    "warm-idle",
    "post-conversation",
    "post-compaction",
    "post-tool-heavy",
]


def project_root() -> Path:
    return Path(__file__).resolve().parents[1]


def sha256_file(path: Path) -> str | None:
    try:
        h = hashlib.sha256()
        with path.open("rb") as f:
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                h.update(chunk)
        return h.hexdigest()
    except OSError:
        return None


def rss_bytes(pid: int) -> int | None:
    """Read RSS in bytes for a live process, cross-platform where possible."""
    if sys.platform.startswith("linux"):
        try:
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1]) * 1024
        except OSError:
            return None
        return None
    # macOS / BSD fallback
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        kb = int(out.strip() or "0")
        return kb * 1024 if kb else None
    except (OSError, ValueError, subprocess.CalledProcessError):
        return None


def median(values: list[int]) -> float:
    ordered = sorted(values)
    n = len(ordered)
    mid = n // 2
    if n % 2 == 1:
        return float(ordered[mid])
    return (ordered[mid - 1] + ordered[mid]) / 2


def resolve_targets() -> dict[str, dict]:
    """Build the target table, honoring env overrides for binary locations."""
    node_pi = shutil.which("pi")
    rust_pi = os.environ.get("PI_RUST_BIN", str(Path(tempfile.gettempdir()) / "pi-rust" / "pi"))
    native_pi = os.environ.get("PIPELETS_BIN")

    targets: dict[str, dict] = {}

    if node_pi:
        targets["pi-node"] = {
            "kind": "node",
            "argv": [node_pi, "--mode", "rpc", "--no-session"],
            "env": {},
        }

    if rust_pi and Path(rust_pi).exists():
        # `pi-rust` is the THIRD-PARTY reference port
        # (Dicklesworthstone/pi_agent_rust), not this project. Our build is the
        # `pipelets` target below (PIPELETS_BIN).
        targets["pi-rust"] = {
            "kind": "native",
            "argv": [
                rust_pi,
                "--rpc",
                "--no-session",
                "--provider",
                os.environ.get("PI_RUST_PROVIDER", "anthropic"),
                "--model",
                os.environ.get("PI_RUST_MODEL", "claude-sonnet-4-5"),
            ],
            # A dummy key lets the runtime boot without network; no request is
            # made during an idle benchmark. The real key is never used.
            "env": {"ANTHROPIC_API_KEY": "sk-ant-mem-bench-dummy"},
        }

    if native_pi and Path(native_pi).exists():
        targets["pipelets"] = {
            "kind": "native",
            "argv": [native_pi, "--rpc", "--no-session"],
            "env": {},
        }

    return targets


def measure_target(
    name: str,
    spec: dict,
    settle: float,
    interval: float,
    count: int,
    extra_args: list[str] | None = None,
    taxonomy: str = IDLE_TAXONOMY,
) -> dict:
    argv = list(spec["argv"]) + list(extra_args or [])
    if "--session" in (extra_args or []):
        # `--no-session` and `--session` are mutually exclusive.
        argv = [arg for arg in argv if arg != "--no-session"]
    env = dict(os.environ)
    env.update(spec.get("env", {}))
    env.setdefault("PI_OFFLINE", "1")
    env.setdefault("PI_SKIP_VERSION_CHECK", "1")
    env.setdefault("NO_COLOR", "1")

    proc = subprocess.Popen(
        argv,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )

    result: dict = {
        "target": name,
        "kind": spec.get("kind", "unknown"),
        "command": " ".join(argv),
        "binary": argv[0],
        "binary_sha256": sha256_file(Path(argv[0])),
        "taxonomy": taxonomy,
        "settle_seconds": settle,
        "sample_interval_seconds": interval,
        "samples_bytes": [],
    }

    try:
        # Settle window: let startup allocations and lazy modules finish.
        deadline = time.monotonic() + settle
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                result["error"] = f"process exited early with code {proc.returncode}"
                return result
            time.sleep(0.25)

        samples: list[int] = []
        for _ in range(count):
            if proc.poll() is not None:
                result["error"] = f"process exited during sampling with code {proc.returncode}"
                return result
            value = rss_bytes(proc.pid)
            if value is not None:
                samples.append(value)
            time.sleep(interval)

        if len(samples) < max(1, count // 2):
            result["error"] = f"insufficient samples ({len(samples)}/{count})"
            result["samples_bytes"] = samples
            return result

        result["samples_bytes"] = samples
        result["sample_count"] = len(samples)
        result["rss_bytes"] = int(median(samples))
        result["rss_min_bytes"] = min(samples)
        result["rss_max_bytes"] = max(samples)
        result["rss_spread_bytes"] = max(samples) - min(samples)
        return result
    finally:
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except OSError:
                proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def human_mb(value: int | None) -> str:
    return "-" if value is None else f"{value / 1048576:.1f} MB"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--target", action="append", default=[], help="target name (repeatable)")
    parser.add_argument("--all", action="store_true", help="run every detected target")
    parser.add_argument("--settle", type=float, default=DEFAULT_SETTLE_SECONDS)
    parser.add_argument("--interval", type=float, default=DEFAULT_SAMPLE_INTERVAL_SECONDS)
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLE_COUNT)
    parser.add_argument("--json", help="write the artifact to this path")
    parser.add_argument("--list", action="store_true", help="list detected targets and exit")
    parser.add_argument(
        "--session",
        help="load this session JSONL in every target (taxonomy: session-loaded)",
    )
    parser.add_argument("--taxonomy", help="override the idle-taxonomy label")
    args = parser.parse_args()

    targets = resolve_targets()
    if args.list:
        for name, spec in targets.items():
            print(f"{name:12} {spec['argv'][0]}")
        return 0

    if args.all:
        selected = list(targets.keys())
    elif args.target:
        selected = args.target
    else:
        print("No target selected. Use --all, --target <name>, or --list.", file=sys.stderr)
        return 1

    missing = [name for name in selected if name not in targets]
    if missing:
        print(f"Unknown/unavailable targets: {', '.join(missing)}", file=sys.stderr)
        print(f"Detected: {', '.join(targets) or '(none)'}", file=sys.stderr)
        return 1

    results = []
    extra_args = ["--session", args.session] if args.session else []
    taxonomy = args.taxonomy or ("session-loaded" if args.session else IDLE_TAXONOMY)
    for name in selected:
        print(f"Measuring {name} ...", flush=True)
        res = measure_target(
            name,
            targets[name],
            args.settle,
            args.interval,
            args.samples,
            extra_args=extra_args,
            taxonomy=taxonomy,
        )
        results.append(res)
        if res.get("error"):
            print(f"  {name}: FAILED - {res['error']} ({len(res.get('samples_bytes', []))} samples)")
        else:
            print(
                f"  {name}: {human_mb(res['rss_bytes'])} "
                f"(min {human_mb(res['rss_min_bytes'])}, max {human_mb(res['rss_max_bytes'])}, "
                f"spread {res['rss_spread_bytes'] // 1024} KiB, n={res['sample_count']})"
            )

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "idle_taxonomy": taxonomy,
        "taxonomies_declared": TAXONOMIES,
        "results": results,
    }

    out_path = Path(args.json) if args.json else project_root() / "artifacts" / "mem_bench.last.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(artifact, indent=2) + "\n")
    print(f"\nArtifact: {out_path}")

    if any(r.get("error") for r in results):
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
