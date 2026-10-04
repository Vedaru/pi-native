#!/usr/bin/env node
// Capture an exact provider request from the installed pi implementation.
//
// This is the TS side of the wire-parity gate (VED-313). It calls pi's real
// provider with an injected `fetch`, records the serialized request body, and
// writes a canonical-JSON fixture. The Rust builder must reproduce it.
//
// Usage:
//   node harness/capture.mjs harness/scenarios/anthropic-basic.json
//
// Scenario `provider` (default "anthropic"):
//   anthropic | openai-completions | openai-responses
//
// Env:
//   PI_AI_DIST  override path to the installed pi-ai dist directory.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");

const PI_AI_DIST =
  process.env.PI_AI_DIST ??
  "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist";

const load = (rel) => import(pathToFileURL(join(PI_AI_DIST, rel)).href);

const PROVIDERS = {
  anthropic: {
    api: "api/anthropic-messages.js",
    models: "providers/anthropic.models.js",
    export: "ANTHROPIC_MODELS",
  },
  "openai-completions": {
    api: "api/openai-completions.js",
    models: "providers/openai.models.js",
    export: "OPENAI_MODELS",
  },
  "openai-responses": {
    api: "api/openai-responses.js",
    models: "providers/openai.models.js",
    export: "OPENAI_MODELS",
  },
};

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
    console.error("usage: capture.mjs <scenario.json>");
    process.exit(2);
  }
  const scenario = JSON.parse(readFileSync(scenarioPath, "utf8"));
  const providerName = scenario.provider ?? "anthropic";
  const provider = PROVIDERS[providerName];
  if (!provider) throw new Error(`unknown provider: ${providerName}`);

  const { streamSimple } = await load(provider.api);
  const { normalizeContext } = await load("utils/transcript.js");
  const modelsModule = await load(provider.models);
  const catalog = modelsModule[provider.export];
  const model = catalog[scenario.model];
  if (!model) throw new Error(`unknown ${providerName} model: ${scenario.model}`);

  let capturedBody = null;
  let capturedParams = null;
  const fakeFetch = async (_url, init) => {
    capturedBody = typeof init?.body === "string" ? init.body : null;
    return new Response("data: [DONE]\n\n", {
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
    apiKey: "harness-dummy-key",
    sessionId: scenario.sessionId,
    cacheRetention: scenario.cacheRetention,
    onPayload: (params) => {
      capturedParams = params;
    },
    fetch: fakeFetch,
  };

  streamSimple(model, context, options);
  await new Promise((r) => setTimeout(r, 500));

  if (!capturedBody) {
    throw new Error("no request body captured; the provider did not reach fetch");
  }

  const fixture = canonicalize(JSON.parse(capturedBody));
  const outPath = join(repoRoot, "harness", "fixtures", `${basename(scenarioPath, ".json")}.json`);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, JSON.stringify(fixture, null, 2) + "\n");

  console.log(`captured: ${scenarioPath}`);
  console.log(`provider: ${providerName} / ${scenario.model}`);
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
