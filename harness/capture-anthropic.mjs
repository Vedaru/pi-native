#!/usr/bin/env node
// Capture an exact provider request from the installed pi implementation.
//
// This is the TS side of the wire-parity gate (VED-313). It calls pi's real
// Anthropic provider with an injected `fetch`, records the serialized request
// body, and writes a canonical-JSON fixture. The Rust builder must reproduce it.
//
// Usage:
//   node harness/capture-anthropic.mjs harness/scenarios/anthropic-basic.json
//
// Env:
//   PI_AI_DIST  override path to the installed pi-ai dist directory.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");

const PI_AI_DIST =
  process.env.PI_AI_DIST ??
  "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist";

const load = (rel) => import(pathToFileURL(join(PI_AI_DIST, rel)).href);

/** Recursively sort object keys so JSON key order cannot cause false diffs. */
export function canonicalize(value) {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value && typeof value === "object") {
    const out = {};
    for (const key of Object.keys(value).sort()) out[key] = canonicalize(value[key]);
    return out;
  }
  return value;
}

async function main() {
  const scenarioPath = process.argv[2];
  if (!scenarioPath) {
    console.error("usage: capture-anthropic.mjs <scenario.json>");
    process.exit(2);
  }
  const scenario = JSON.parse(readFileSync(scenarioPath, "utf8"));

  const { streamSimple } = await load("api/anthropic-messages.js");
  const { normalizeContext } = await load("utils/transcript.js");
  const { ANTHROPIC_MODELS } = await load("providers/anthropic.models.js");

  const model = ANTHROPIC_MODELS[scenario.model];
  if (!model) throw new Error(`unknown anthropic model: ${scenario.model}`);

  let capturedBody = null;
  let capturedParams = null;
  const fakeFetch = async (_url, init) => {
    capturedBody = typeof init?.body === "string" ? init.body : null;
    // Return a minimal empty SSE response so the provider does not retry.
    return new Response("event: message_stop\ndata: {}\n\n", {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  };

  const context = normalizeContext({
    systemPrompt: scenario.systemPrompt,
    tools: scenario.tools,
    messages: scenario.messages,
  });

  const options = {
    apiKey: "sk-ant-harness-dummy",
    sessionId: scenario.sessionId,
    cacheRetention: scenario.cacheRetention,
    onPayload: (params) => {
      capturedParams = params;
    },
    fetch: fakeFetch,
  };

  streamSimple(model, context, options);
  // Let the async setup run to the point of building params and issuing fetch.
  await new Promise((r) => setTimeout(r, 500));

  if (!capturedBody) {
    throw new Error("no request body captured; the provider did not reach fetch");
  }

  const parsed = JSON.parse(capturedBody);
  const fixture = canonicalize(parsed);

  const outPath = join(repoRoot, "harness", "fixtures", `anthropic-${scenario.model}.json`);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, JSON.stringify(fixture, null, 2) + "\n");

  console.log(`captured: ${scenarioPath}`);
  console.log(`model:    ${scenario.model}`);
  console.log(`fixture:  ${outPath}`);
  if (capturedParams) {
    console.log(`payload keys: ${Object.keys(capturedParams).sort().join(", ")}`);
  }
  console.log(`body keys:    ${Object.keys(fixture).sort().join(", ")}`);
}

main().catch((error) => {
  console.error(error?.stack ?? String(error));
  process.exit(1);
});
