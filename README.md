# pi-native

Workspace for the **pi native runtime** project: a headless-first Rust rewrite of
pi that drops Node, ships as one small binary, and stays byte-identical to pi at
the provider API — so a swarm of agent units is cheap to run and keeps pi's
prompt-cache behavior.

Linear project: *pi native runtime: Rust memory-heavy rewrite, drop Node*
(team `VED`).

## Why pi-native

A headless-first Rust runtime for a **swarm of coding-agent units**: one small
self-contained binary each, no Node/V8, provider-identical to pi.

| | pi-native | pi-node |
| --- | --- | --- |
| shipped runtime | one ~7.5 MB static-capable binary | Node + `node_modules` |
| idle RSS (`--rpc`) | **4.1 MB** | 111.7 MB |
| idle RSS (full unit, `--serve`) | **5.3 MB** | 111.7 MB |
| RSS per real agent turn (tools) | **7.7 MB** | — |
| idle CPU | **~0.001 s / 3 s** (no busy-wait) | — |

- **Small overhead.** ~27× less idle memory than pi-node; tools stream I/O with
  bounded buffers, so a 200 MB file read peaks at ~5 MB. A static build idles at
  ~3.3 MB (measured in a throwaway container).
- **Small binary.** One ~7.5 MB stripped binary (`lto=fat`, `opt-level=2`,
  `panic=abort`). Plugins run in an embedded QuickJS engine — no Node, no V8.
- **Swarm friendly.** 32 idle units ≈ 4.1 MB each; 8 busy read-session units
  ≈ 6.3 MB each. The dynamic build shares libc across units, so a swarm costs
  less than the per-unit sum suggests.
- **High cache hit rate.** Outbound requests are byte-identical to pi
  (verified against pi's real requests), so the prompt cache behaves the same:
  on DeepSeek the system+tools prefix (1,408 tokens) is cached and a multi-turn
  session runs at ~86% hit per turn — exactly pi's numbers.

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
persist the transcript), `--no-session` (do not persist), `--context-window
<tokens>` (compaction; 0 disables; dropped history is summarized by the model),
`--yolo`, `--extension-allow <caps>`. Events stream as the turn produces them.

Without `--session`, a run creates a session file under the pi agent directory
(`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`) in pi's layout:
`sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`. That is the same layout pi
and pi-web read, so their session browsers can list and resume native sessions.

Tools are jailed to the working directory by default: absolute paths, `..`
escapes, and symlinks that leave it are rejected, and `--yolo` is the explicit
opt-out. `--serve` without `--yolo` routes both tool calls **and** the
out-of-band `bash` command through the client approval (`ui_request`); a client
that cannot write a `ui_response` cannot get a shell.

Load pi extensions/plugins with `--extension <path>` (repeatable). Tools they
register via `pi.registerTool` are exposed to the agent and run in QuickJS.
Extensions are deny-by-default for ambient access: they may register tools,
commands, and event handlers, but filesystem, process, and network access must
be granted with `--extension-allow read,write,exec,http`, and file/process paths
are jailed to the working directory. Extension tools always require approval
(they are gated exactly like `bash`/`write`/`edit`).

```bash
pi-native --serve --extension ./extensions/my-tool.ts
# grant an extension read-only workspace access
pi-native --serve --extension ./my.ts --extension-allow read
```

## Unit gateway (HTTP + SSE)

`--gateway` serves the unit host over HTTP so a web UI or remote client can
attach to long-lived agents. Sessions are addressed by id, not by a file
descriptor; any number of clients can attach, and an idle unit releases its
in-memory agent until the next command.

```bash
pi-native --gateway --gateway-addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …
```

| Method | Path | Meaning |
| --- | --- | --- |
| `GET` | `/sessions` | List session ids |
| `POST` | `/sessions` | Open/create a session (`{"sessionPath"?: …, "cwd"?: …}`) |
| `GET` | `/sessions/:id` | Resolved state |
| `GET` | `/sessions/:id/events` | SSE stream (replay + live) |
| `POST` | `/sessions/:id/commands` | Send a command (`prompt`, `steer`, `abort`, …) |
| `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |

By default the SSE stream carries the native event envelope; add `?format=pi`
to receive pi's canonical event stream (`agent_start`, `message_update`,
`tool_execution_*`, `agent_settled`) through `pi_rpc::PiEventAdapter`.

### Triggers

`--triggers <file>` (with `--gateway`) fires scheduled prompts into long-lived
sessions. Each trigger has a stable session, so context accumulates across
runs; run records are appended to `.pi-native/trigger-runs.jsonl` (or
`--trigger-runs <path>`) and are the dedupe/idempotency layer across restarts.

```json
[
  {"id":"hourly-sweep","interval_secs":3600,"prompt":"check the queue","dedupe_window_secs":3600},
  {"id":"standup","cron":"0 9 * * 1-5","prompt":"summarize yesterday","max_runs_per_window":1,"budget_window_secs":86400}
]
```

```bash
pi-native --gateway --triggers ./triggers.json --trigger-interval 1 \
  --provider openai-completions --base-url … --model … --api-key …
```

Schedules are `interval_secs` or a 5-field `cron` (`min hour day month weekday`).
Budgets are dedupe windows and `max_runs_per_window`; model-level budgets
(iterations, tokens, wall-clock) come from the agent configuration.

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
| **`pi-native` (our build, headless)** | **4.1 MB** | 6.3 MB (stress) |
| `pi-node` (interactive TUI) | 117.2 MB | **207.1 MB** |

`pi-native` now runs the full agent loop, tools, RPC, and plugins, so its idle
footprint grew from the idle-only scaffold; it is still ~27x below pi-node. The reference `pi-rust` is the third-party port, included
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

## Lean build

The release profile is tuned for size and speed: `lto = "fat"`, `opt-level = 2`,
`codegen-units = 1`, `panic = "abort"`, stripped. Measured (host, x86_64):

| | before (thin/3) | now (fat/2) |
| --- | --- | --- |
| binary | ~8.7 MB | **~7.5 MB** |
| idle RSS (`--rpc`) | 4.4 MB | **4.1 MB** |
| idle RSS (`--serve`) | 5.8 MB | **5.3 MB** |
| idle CPU (3 s) | ~0 | **0.001 s** |

The process is near the floor for a Rust binary (a bare `fn main` reports
~2.3 MB, mostly shared libc). A static build reports ~3.3 MB but would stop
sharing libc pages across a swarm, so the dynamic build is kept.

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
