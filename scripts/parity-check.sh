#!/usr/bin/env bash
# Wire-parity check (VED-313): regenerate fixtures from pi, then run the Rust
# parity tests. Requires the installed pi-ai dist (set PI_AI_DIST to override).
set -euo pipefail
cd "$(dirname "$0")/.."

for scenario in scripts/harness/scenarios/*.json; do
  node scripts/harness/capture.mjs "$scenario" crates/pi-providers/tests/fixtures
done

cargo test --workspace
