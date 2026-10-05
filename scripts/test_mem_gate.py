#!/usr/bin/env python3
"""Tests for the memory regression gate (VED-367).

Run with `python3 scripts/test_mem_gate.py`. Stdlib only.

Regression under test: the absolute check used a purely relative threshold, so
a tiny native target (~4 MB) false-failed on sub-MB run-to-run noise (+12-18%).
The gate now requires *both* the relative threshold *and* an absolute noise
floor before failing.
"""

from __future__ import annotations

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_gate():
    spec = importlib.util.spec_from_file_location(
        "mem_gate", os.path.join(HERE, "mem_gate.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


gate = load_gate()
MB = 1024 * 1024


class NoiseFloorTests(unittest.TestCase):
    def test_tiny_relative_regression_below_floor_passes(self):
        # Reproduces the observed false failure: 3.9 MB -> 4.4 MB is ~+13%,
        # but only +0.5 MB of noise, so it must not fail.
        self.assertFalse(
            gate.absolute_regression_fails(
                4_400_000, 4_090_000, threshold=0.10, min_delta_mb=2.0
            )
        )

    def test_regression_at_or_above_floor_fails(self):
        # 3.9 MB -> 6.5 MB is +~66% and +2.6 MB: a real regression, must fail.
        self.assertTrue(
            gate.absolute_regression_fails(
                int(6.5 * MB), int(3.9 * MB), threshold=0.10, min_delta_mb=2.0
            )
        )

    def test_relative_regression_under_threshold_passes(self):
        # +5% is under the 10% threshold even with a large absolute delta.
        self.assertFalse(
            gate.absolute_regression_fails(
                105 * MB, 100 * MB, threshold=0.10, min_delta_mb=2.0
            )
        )

    def test_shrink_is_never_a_regression(self):
        self.assertFalse(
            gate.absolute_regression_fails(
                3 * MB, 4 * MB, threshold=0.10, min_delta_mb=2.0
            )
        )

    def test_floor_of_zero_matches_old_relative_only_behavior(self):
        # With no floor, any relative regression fails (back-compat escape).
        # 4.09 MB -> 4.60 MB is +~12.5% and only +0.5 MB.
        self.assertTrue(
            gate.absolute_regression_fails(
                4_600_000, 4_090_000, threshold=0.10, min_delta_mb=0.0
            )
        )

    def test_default_floor_is_two_mb(self):
        self.assertEqual(gate.DEFAULT_MIN_ABSOLUTE_DELTA_MB, 2.0)


class IndexResultsTests(unittest.TestCase):
    def test_zero_and_missing_rss_are_ignored(self):
        artifact = {
            "results": [
                {"target": "pipelets", "taxonomy": "cold-idle", "rss_bytes": 0},
                {"target": "pi-node", "taxonomy": "cold-idle"},
                {"target": "pipelets", "taxonomy": "warm-idle", "rss_bytes": 123},
            ]
        }
        indexed = gate.index_results(artifact)
        self.assertEqual(indexed, {("pipelets", "warm-idle"): 123})


if __name__ == "__main__":
    unittest.main()
