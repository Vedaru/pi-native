# pi-native

Workspace for the **pi native runtime** project: reduce pi's runtime memory by
rewriting only the memory-heavy subsystems in Rust and removing the Node runtime.

Linear project: *pi native runtime: Rust memory-heavy rewrite, drop Node*
(team `VED`).

## Non-negotiable constraint

The native runtime MUST be indistinguishable from pi at the provider API for an
equivalent session. Any change that alters outbound request bytes or lowers the
prompt-cache hit rate versus pi is a regression that blocks merge and release.
Tracked by the release gate (VED-315), gated on VED-313 (wire parity) and
VED-314 (cache hit rate). If a Rust rewrite cannot meet this, it is reverted to
a pi-compatible implementation.

## Layout

```
crates/pi-cache/       provider prompt-cache primitives, ported from pi (VED-314)
crates/pi-providers/   OpenAI request builders with pi's cache placement
crates/pi-cli/         `pi-native` CLI
scripts/               benchmark, stress, and parity harnesses
artifacts/             committed measurement history
docs/adr/              architecture decision records
```

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Packaging builds a single self-contained binary and tars it with the license:

```bash
scripts/package.sh                      # -> dist/pi-native-<version>-<target>.tar.gz
scripts/package.sh x86_64-unknown-linux-musl   # static build (needs musl-tools)
```

`pi-native --version` reports the version, git revision, and target triple.

## Headless host

`pi-native` runs the agent loop over the JSON-lines protocol
(`docs/rpc-protocol.md`). One generic path: pick a provider, the loop and tools
are the same.

```bash
# one-shot (provider, base URL, model, and key are explicit — no baked-in defaults)
OPENAI_API_KEY=… pi-native \
  --provider openai-completions --base-url https://api.deepseek.com \
  --model deepseek-flash --thinking-format deepseek -p "summarize README.md"

# a unit: protocol on stdio (approval-required tools ask the client; --yolo allows all)
pi-native --serve --provider openai-completions --base-url … --model …

# drive a local unit from the terminal
pi-native --client

# OpenAI Responses
pi-native --serve --provider openai-responses --base-url https://api.openai.com/v1 --model gpt-4o
```

Flags: `--provider openai-completions|openai-responses`, `--base-url` (or
`OPENAI_BASE_URL`), `--api-key` (or `OPENAI_API_KEY`), `--model`,
`--max-tokens`, `--thinking-format none|deepseek`, `--session <path>` (seed and
persist the transcript), `--context-window <tokens>` (compaction; 0 disables;
dropped history is summarized by the model), `--yolo`. Events stream as the turn
produces them.

Load pi extensions/plugins with `--extension <path>` (repeatable). Tools they
register via `pi.registerTool` are exposed to the agent and run in QuickJS:

```bash
pi-native --serve --extension ./extensions/my-tool.ts
```

## Prompt-cache primitives (`pi-cache`)

The provider-parity gate (VED-315) requires exact cache behavior. `pi-cache`
ports the primitives from pi and is unit-tested against pi's semantics:

- retention resolution and Anthropic `cache_control` markers,
- OpenAI `prompt_cache_key` clamping and wiring,
- cache-miss detection and waste accounting,
- cache-warming delay, replayability, and economics.

```bash
cargo run -q -p pi-native -- cache-control --retention long
# {"cache_control":{"ttl":"1h","type":"ephemeral"},"retention":"long"}

cargo run -q -p pi-native -- prompt-cache-key --session-id sess-123 --responses
# {"prompt_cache_key":"sess-123","retention":"short"}
```

## Provider wire parity (VED-313)

The gate requires byte-identical provider requests. `scripts/harness/capture.mjs`
captures pi's real outgoing request by injecting a fake `fetch` into pi's
provider, then writes canonical JSON fixtures. The Rust builder is tested
against those fixtures.

```bash
./scripts/parity-check.sh
```

Scenarios live in `scripts/harness/scenarios/` and select a `provider`
(`openai-completions`, `openai-responses`). Fixtures live in
each crate's `tests/fixtures/`. JSON key order is canonicalized so only semantic changes
count. `PI_AI_DIST` overrides the installed pi-ai path.

## Attribution

Derived in part from pi (<https://github.com/earendil-works/pi>), MIT. See
`LICENSE`.

## Architecture

See [`docs/adr/0001-host-architecture.md`](docs/adr/0001-host-architecture.md):
native Rust host, embedded QuickJS (`rquickjs`) + `swc` for JS/TS extensions,
native providers/HTTP, `crossterm` TUI. No Node runtime.

## Session store (`pi-session`)

Parses and writes pi's JSONL session format. Entries keep their common fields
typed (`type`/`id`/`parentId`/`timestamp`) and preserve all other fields
verbatim, so new entry kinds round-trip without code changes.

- Loading is streamed line by line (`BufReader`); the raw file is never held as
  one string.
- `build_context` walks the leaf branch, applies the latest compaction, and
  applies `context_edit` entries.
- Measured: a 3.2 MB session reads losslessly at **10.3 MB peak RSS** (the same
  session costs pi ~124 MB total).

```bash
cargo run -p pi-session --example read_session -- path/to/session.jsonl
```


## Memory benchmark

```bash
python3 scripts/mem_bench.py --list          # show detected targets
python3 scripts/mem_bench.py --all           # measure all targets
python3 scripts/mem_bench.py --target pi-node --target pi-rust
```

Targets are auto-detected:

- `pi-node` — the current `pi` on `PATH` (Node/V8).
- `pi-rust` — reference port, path from `PI_RUST_BIN` (default `/tmp/pi-rust/pi`).
- `pi-native` — our build, path from `PI_NATIVE_BIN`.

Idle taxonomy (VED-302): only `cold-idle` is implemented; the remaining four
states are declared in the artifact schema so it does not churn later.

## Baseline (2026-10-04)

| Target | cold-idle RSS | session-loaded (3.2 MB JSONL) |
| --- | --- | --- |
| `pi-node` (headless RPC) | 111.7 MB | 124.8 MB |
| `pi-rust` (reference port, headless) | 38.9 MB | 50.7 MB |
| **`pi-native` (our build, headless)** | **2.6 MB** | — |
| `pi-node` (interactive TUI) | 117.2 MB | **207.1 MB** |

`pi-native` is the idle floor of our binary today (it only idles); it will grow
as the runtime lands. The reference `pi-rust` is the third-party port, included
only for comparison — our build is `pi-native` (`PI_NATIVE_BIN`).

Headless, a 3.2 MB session costs ~12-13 MB. The **interactive TUI adds ~90 MB**
for the same session (retained rendered transcript components) — the variable
cost the native renderer and windowed transcript must remove. Target for our
build: **<= 25 MB idle headless**, and a bounded TUI transcript.

## Image pipeline (`pi-image`)

Native decode/orient/resize/encode, replacing photon WASM. Matches pi's strategy:
keep the original if within limits, otherwise fit to `maxWidth`/`maxHeight`,
then return the first encoding under `maxBytes` in pi's order (PNG, then JPEG at
the configured quality and 85/70/55/40), shrinking by 25% per retry.

Comparison on a 6000x4000 PNG (native, via
`cargo run --release -p pi-image --example resize`):

| | pi (photon WASM) | pi-image (native) |
| --- | --- | --- |
| dimensions | 2000x1333 | 2000x1333 |
| format | image/jpeg | image/jpeg |
| base64 size | 2,405,452 B | 2,405,532 B |
| time | 1,485 ms | 336 ms |
| peak RSS | 493 MB | 207 MB |

## Pressure (`--stress`)

`pi-native --stress N` runs a deterministic in-process workload (N turns of an
`ls` tool call, no network, no subprocesses). `scripts/stress_gate.py` fails CI
if peak RSS exceeds a ceiling or the run does not finish in time.

Two modes:

- **Tool** (default): `--stress N --stress-tool <ls|grep|find|edit|read>` calls
  the tool directly, no agent loop and no transcript, so the numbers are the
  tool's own memory/CPU.
- **Session**: add `--stress-session` to run the agent loop over one growing
  transcript with token-based compaction (where compaction is exercised).

Tool mode, 20,000 calls:

| Tool | Wall | Peak RSS |
| --- | --- | --- |
| `ls` | 0.03 s | 3.9 MB |
| `grep` | 0.35 s | 5.2 MB |
| `find` | 0.43 s | 5.2 MB |
| `edit` | 0.06 s | 4.0 MB |
| `read` (40 KB each, 819 MB total) | 0.37 s | 4.0 MB |

CPU is gated too: each tool run must stay under a CPU-second ceiling, and the
idle process must use ~0 CPU. Measured idle CPU over 3 s: **0.001 s** (no
busy-wait). Per-tool CPU for 20,000 calls ranges 0.03 s (`ls`) to 0.41 s
(`find`).

Without the window, 50,000 `ls` turns: 0.08 s / 39 MB (and 1M turns stay flat
at ~5 MB with the window).

Context is bounded by **token-based compaction** (pi's rule: compact when
estimated tokens exceed `contextWindow - reserveTokens`, reserve 16,384). With a
realistic 200,000-token window, 20,000 turns compact only occasionally:

| Workload | Compactions / 20k turns | Peak RSS |
| --- | --- | --- |
| `ls` (tiny messages) | 5 | 7.0 MB |
| `read` (40 KB results) | 1,176 (~1 per 17 turns) | 4.9 MB |

The kept tail is capped at half the threshold, so compaction cannot churn (a
misconfigured window/reserve previously caused ~2 compactions per turn).

Growth is proportional to the retained transcript (~900 B/turn), with no leak.
Two fixes came out of pressure testing:

- The loop **cloned the whole transcript every iteration** (O(n²)). It now
  borrows (`CompletionRequest` holds slices): 10k turns went from 9.7 s to
  0.09 s, and 50k from a timeout to under 3 s.
- The loop **accumulated every event**, duplicating tool output for the whole
  turn. `run_with` streams events to a sink; `run` collects them for callers that
  want a `Vec`.

The remaining growth is the retained transcript; bounding it needs **context
compaction**, which is the real fix for very long sessions.

Swarm (`scripts/swarm_stress.py`): N units at once, aggregate and per-unit.

| Swarm | Per unit | Total |
| --- | --- | --- |
| 8 idle `--rpc` | 3.9 MB | 30.9 MB |
| 32 idle `--rpc` | 3.8 MB | 121.8 MB |
| 8 busy (`read` sessions, 200k turns) | 4.9 MB | 39.1 MB |

Native states (from `--stress --stress-session`), 200k turns where applicable:

| State | RSS | CPU |
| --- | --- | --- |
| idle | 3.9 MB | 0.00 s |
| tool-heavy (`read`) | 4.9 MB | 3.73 s |
| tool-heavy (`grep`) | 9.8 MB | 3.49 s |
| compaction-heavy | 4.3 MB | 4.08 s |
