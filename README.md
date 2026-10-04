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
crates/pi-providers/   Anthropic/OpenAI request builders with pi's cache placement
crates/pi-cli/         `pi-native` CLI (early scaffold)
scripts/mem_bench.py   runtime memory benchmark (VED-302)
config/                target and benchmark configuration
artifacts/             generated measurement artifacts (committed for history)
docs/adr/              architecture decision records
```

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
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

The gate requires byte-identical provider requests. `harness/capture.mjs`
captures pi's real outgoing request by injecting a fake `fetch` into pi's
provider, then writes canonical JSON fixtures. The Rust builder is tested
against those fixtures.

```bash
./scripts/parity-check.sh
```

Scenarios live in `harness/scenarios/` and select a `provider`
(`anthropic`, `openai-completions`, `openai-responses`). Fixtures live in
`harness/fixtures/`. JSON key order is canonicalized so only semantic changes
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

TUI numbers: `python3 scripts/measure_tui.py [--session ...]` (pty-based).

## Image pipeline (`pi-image`)

Native decode/orient/resize/encode, replacing photon WASM. Matches pi's strategy:
keep the original if within limits, otherwise fit to `maxWidth`/`maxHeight`,
then return the first encoding under `maxBytes` in pi's order (PNG, then JPEG at
the configured quality and 85/70/55/40), shrinking by 25% per retry.

Comparison on a 6000x4000 PNG (`harness/compare-image-resize.mjs` vs
`cargo run --release -p pi-image --example resize`):

| | pi (photon WASM) | pi-image (native) |
| --- | --- | --- |
| dimensions | 2000x1333 | 2000x1333 |
| format | image/jpeg | image/jpeg |
| base64 size | 2,405,452 B | 2,405,532 B |
| time | 1,485 ms | 336 ms |
| peak RSS | 493 MB | 207 MB |
