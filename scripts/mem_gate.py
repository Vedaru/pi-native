#!/usr/bin/env python3
"""Memory regression gate.

Two independent checks, both meaningful in CI:

1. **Absolute**: each target's RSS vs the committed baseline
   (`artifacts/mem_bench.json`), only for `(target, taxonomy)` keys present in
   both. Skipped when the baseline does not cover the current environment.
2. **Ratio**: each native target's RSS vs the Node target's RSS for the same
   taxonomy. This is environment-independent, so it works on a CI runner whose
   absolute numbers differ from a developer machine.

Exits 0 when no benchmark target is installed (CI without `pi`), but says so
loudly instead of passing silently.

Usage:
    python3 scripts/mem_gate.py [--threshold 0.10] [--ratio-threshold 0.6]
    python3 scripts/mem_gate.py --from-json artifacts/current.json   # evaluate a file
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

DEFAULT_THRESHOLD = 0.10
DEFAULT_RATIO_THRESHOLD = 0.60
# Below this absolute regression (MB) the relative check is ignored: for a tiny
# baseline (single-digit MB) a few hundred KB of run-to-run noise exceeds the
# percentage threshold and would false-fail CI.
DEFAULT_MIN_ABSOLUTE_DELTA_MB = 2.0
NODE_TARGET = "pi-node"
NATIVE_TARGETS = {"pi-rust", "pi-native"}
# Our build. The ceiling applies only to this; the reference is for comparison.
OUR_TARGET = "pi-native"


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


def absolute_regression_fails(
    rss: int, baseline_rss: int, threshold: float, min_delta_mb: float
) -> bool:
    """A regression fails only when it is over the relative threshold *and*
    at least the absolute noise floor. On a tiny baseline a few hundred KB of
    measurement noise exceeds the percentage threshold and must not fail CI."""
    delta = (rss - baseline_rss) / baseline_rss
    if delta <= threshold:
        return False
    return (rss - baseline_rss) / 1048576 >= min_delta_mb


def run_benchmark(current_path: Path) -> dict | None:
    run = subprocess.run(
        [
            sys.executable,
            str(project_root() / "scripts" / "mem_bench.py"),
            "--all",
            "--json",
            str(current_path),
        ],
        capture_output=True,
        text=True,
    )
    if run.returncode != 0:
        sys.stderr.write(run.stdout)
        sys.stderr.write(run.stderr)
        return None
    return load_json(current_path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    parser.add_argument("--ratio-threshold", type=float, default=DEFAULT_RATIO_THRESHOLD)
    parser.add_argument(
        "--min-absolute-delta-mb",
        type=float,
        default=DEFAULT_MIN_ABSOLUTE_DELTA_MB,
        help="ignore absolute regressions smaller than this many MB (noise floor)",
    )
    parser.add_argument("--from-json", help="evaluate this artifact instead of running the benchmark")
    parser.add_argument(
        "--no-absolute",
        action="store_true",
        help="skip the absolute baseline check (use on runners whose memory profile differs)",
    )
    parser.add_argument(
        "--max-native-mb",
        type=float,
        help="fail if any native target exceeds this many MB (environment-independent ceiling)",
    )
    args = parser.parse_args()

    root = project_root()
    baseline = index_results(load_json(root / "artifacts" / "mem_bench.json"))

    if args.from_json:
        # A missing or malformed artifact is a hard error: silently treating it as
        # "no targets" would let CI pass when the input file was never produced.
        from_path = Path(args.from_json)
        if not from_path.exists():
            print(f"FAIL: --from-json file not found: {from_path}")
            return 1
        try:
            current_artifact = json.loads(from_path.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            print(f"FAIL: --from-json file is not valid JSON: {from_path}: {exc}")
            return 1
    else:
        current_artifact = run_benchmark(root / "artifacts" / "mem_bench.current.json")
        if current_artifact is None:
            # The benchmark failed to run or produced an unreadable artifact;
            # that is distinct from a valid artifact with no installed targets.
            print("FAIL: memory benchmark did not produce a usable artifact.")
            return 1

    current = index_results(current_artifact)
    if not current:
        print(
            "NOTICE: no benchmark targets available in this environment; the memory gate "
            "did not run. Install `pi` (and optionally the reference native binary) to enable it."
        )
        return 0

    failed = False

    # 1. Absolute regression against the committed baseline.
    print("Absolute (vs committed baseline):")
    overlaps = 0
    if args.no_absolute:
        print("  (skipped by --no-absolute)")
    else:
        for (target, taxonomy), rss in sorted(current.items()):
            if (target, taxonomy) not in baseline:
                continue
            overlaps += 1
            base_rss = baseline[(target, taxonomy)]
            delta = (rss - base_rss) / base_rss
            abs_delta_mb = (rss - base_rss) / 1048576
            print(f"  {target} [{taxonomy}]: {human_mb(base_rss)} -> {human_mb(rss)} ({delta * 100:+.1f}%, {abs_delta_mb:+.2f} MB)")
            if absolute_regression_fails(rss, base_rss, args.threshold, args.min_absolute_delta_mb):
                print(f"    FAIL: over {args.threshold * 100:.0f}%")
                failed = True
            elif delta > args.threshold:
                print(
                    f"    ok: over {args.threshold * 100:.0f}% but only {abs_delta_mb:+.2f} MB "
                    f"(< {args.min_absolute_delta_mb:.1f} MB noise floor)"
                )
        if overlaps == 0:
            print("  (no comparable baseline keys in this environment; skipped)")

    # 2. Ratio of native targets to the Node target, same taxonomy.
    print(f"Ratio (native <= {args.ratio_threshold:.0%} of {NODE_TARGET}):")
    ratios = 0
    for (target, taxonomy), rss in sorted(current.items()):
        if target not in NATIVE_TARGETS:
            continue
        node_rss = current.get((NODE_TARGET, taxonomy))
        if not node_rss:
            continue
        ratios += 1
        ratio = rss / node_rss
        print(f"  {target}/{NODE_TARGET} [{taxonomy}]: {ratio:.2f} ({human_mb(rss)} / {human_mb(node_rss)})")
        if ratio > args.ratio_threshold:
            print(f"    FAIL: over {args.ratio_threshold:.0%}")
            failed = True
    if ratios == 0:
        print(f"  (no native target alongside {NODE_TARGET} in this environment; skipped)")

    # 3. Environment-independent absolute ceiling for native targets.
    if args.max_native_mb is not None:
        print(f"Ceiling (native <= {args.max_native_mb:.0f} MB):")
        ceilings = 0
        ceiling_bytes = int(args.max_native_mb * 1048576)
        for (target, taxonomy), rss in sorted(current.items()):
            if target != OUR_TARGET:
                continue
            ceilings += 1
            print(f"  {target} [{taxonomy}]: {human_mb(rss)}")
            if rss > ceiling_bytes:
                print(f"    FAIL: over {args.max_native_mb:.0f} MB")
                failed = True
        if ceilings == 0:
            print("  (no native target in this environment; skipped)")

    if failed:
        print("\nFAIL: memory gate failed.")
        return 1
    print("\nPASS: memory gate passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
