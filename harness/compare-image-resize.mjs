#!/usr/bin/env node
// Compare pi's photon resize against the native pipeline (VED-308).
//
// Runs pi's real `resizeImageInProcess` (photon WASM) on an image and prints the
// result plus peak RSS. Compare with `cargo run -p pi-image --example resize`.
//
// Usage: node harness/compare-image-resize.mjs <image>

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const DIST =
  process.env.PI_CODING_AGENT_DIST ??
  "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/dist";

const imagePath = process.argv[2];
if (!imagePath) {
  console.error("usage: compare-image-resize.mjs <image>");
  process.exit(2);
}

const { resizeImageInProcess } = await import(
  pathToFileURL(join(DIST, "utils", "image-resize-core.js")).href
);

const bytes = new Uint8Array(readFileSync(resolve(here, "..", imagePath).replace(/.*/, imagePath)));
const mime = imagePath.toLowerCase().endsWith(".png") ? "image/png" : "image/jpeg";

const started = Date.now();
const result = await resizeImageInProcess(bytes, mime, {
  maxWidth: 2000,
  maxHeight: 2000,
  maxBytes: 4.5 * 1024 * 1024,
  jpegQuality: 80,
});
const elapsed = Date.now() - started;

const status = readFileSync("/proc/self/status", "utf8");
const peakKb = Number((status.match(/VmHWM:\s+(\d+) kB/) ?? [])[1] ?? 0);

console.log(
  JSON.stringify({
    width: result?.width,
    height: result?.height,
    mime: result?.mimeType,
    base64_bytes: result?.data?.length,
    was_resized: result?.wasResized,
    elapsed_ms: elapsed,
  }),
);
console.log(`peak_rss_mb ${(peakKb / 1024).toFixed(1)}`);
