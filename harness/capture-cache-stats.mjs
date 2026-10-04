#!/usr/bin/env node
// Cache-stats parity capture (VED-314).
//
// Runs pi's real `computeCacheWaste` over a synthetic session scenario and writes
// the expected totals. The Rust `pi-cache` scan must reproduce them.
//
// Usage:
//   node harness/capture-cache-stats.mjs
//
// Env:
//   PI_CODING_AGENT_DIST  override path to the installed coding-agent dist.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");
const DIST =
  process.env.PI_CODING_AGENT_DIST ??
  "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/dist";

const { computeCacheWaste } = await import(
  pathToFileURL(join(DIST, "core", "cache-stats.js")).href
);

const scenarioPath = join(repoRoot, "harness", "fixtures", "cache-stats-scenario.json");
const scenario = JSON.parse(readFileSync(scenarioPath, "utf8"));

const models = {
  getModel(provider, modelId) {
    return scenario.models?.[provider]?.[modelId];
  },
};

const totals = computeCacheWaste(scenario.entries, models);

const outPath = join(repoRoot, "harness", "fixtures", "cache-stats-expected.json");
writeFileSync(outPath, JSON.stringify(totals, null, 2) + "\n");
console.log(`wrote ${outPath}`);
console.log(JSON.stringify(totals));
