#!/usr/bin/env bash
# Wire-parity check (VED-313): regenerate fixtures from pi, then run the Rust
# parity tests. Requires the installed pi-ai dist (set PI_AI_DIST to override).
set -euo pipefail
cd "$(dirname "$0")/.."

for scenario in harness/scenarios/anthropic-*.json; do
  node harness/capture-anthropic.mjs "$scenario"
done

cargo test --workspace
