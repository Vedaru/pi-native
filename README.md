# pipelets

**A swarm-friendly, low-memory, low-CPU agent bare core — ~4 MB per idle unit.**

pipelets is the headless-first Rust core that agent swarms run on: one small
self-contained binary per unit, no Node/V8, and byte-identical provider requests
to pi so prompt caching keeps working. A unit is addressed over JSON-lines RPC or
the HTTP+SSE gateway; a conductor drives many of them at once.

- **Bare core.** The agent loop, tools, session store, provider transport, and
  plugin host — and nothing else. No editor, no cloud, no per-user daemon.
- **Low memory.** ~4.6 MB idle in `--rpc`, ~5.1 MB serving a unit; bounded I/O,
  token-based compaction, and a windowed transcript keep long sessions flat.
- **Low CPU.** Idle costs ~0.001 s of CPU per 3 s (no busy-wait); tools stream
  bounded buffers and a 20k-call tool run stays under a second of CPU.
- **Swarm friendly.** 32 idle units ≈ 4.4 MB each (~142 MB total); the dynamic
  build shares libc pages, so a swarm costs less than the per-unit sum.
- **pi-compatible.** Outbound request bytes and prompt-cache behavior match pi.

Linear project: *pi native runtime: Rust memory-heavy rewrite, drop Node*
(team `VED`).

## Why pipelets

One small self-contained binary per unit, no Node/V8, provider-identical to pi.
Measured 2026-10-05 (release build, x86_64; see [performance](docs/performance.md)):

| | pipelets | pi-node |
| --- | --- | --- |
| shipped runtime | one **11.9 MB** binary | Node + `node_modules` |
| idle RSS (`--rpc`) | **4.6 MB** | 111.3 MB |
| idle RSS (`--gateway`) | **5.1 MB** | — |
| idle CPU | **~0.001 s / 3 s** | — |

## Non-negotiable constraint

The native runtime MUST be indistinguishable from pi at the provider API for an
equivalent session. Any change that alters outbound request bytes or lowers the
prompt-cache hit rate versus pi is a regression that blocks merge and release
(gate VED-315). See [providers and wire parity](docs/providers.md).

## Layout

```
crates/pi-cli/         the `pipelets` binary
crates/pi-agent/       agent loop: turns, tool calls, events
crates/pi-host/        unit host: addressable agents, fan-out, idle suspend
crates/pi-gateway/     HTTP + SSE gateway
crates/pi-triggers/    schedules, episodes, budgets, run receipts
crates/pi-session/     pi JSONL session store
crates/pi-providers/   request builders with pi's cache placement
crates/pi-tools/       core tools (read, bash, edit, write, …)
crates/pi-plugins/     embedded QuickJS + pi extension API bridge
crates/pi-image/       native image decode/resize/encode for `read` attachments
docs/                  guides and ADRs
scripts/               benchmark, stress, parity, and memory gates
```

## Build, test, package

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

scripts/package.sh                              # dist/pipelets-<version>-<target>.tar.gz
scripts/package.sh x86_64-unknown-linux-musl    # static build (needs musl-tools)
```

`pipelets --version` reports the version, git revision, and target triple.

## Run a unit

One generic path: pick a provider; the loop and tools are the same.

```bash
# one-shot (provider, base URL, model, and key are explicit — no baked-in defaults)
OPENAI_API_KEY=… pipelets \
  --provider openai-completions --base-url https://api.deepseek.com \
  --model deepseek-flash --thinking-format deepseek -p "summarize README.md"

# a unit: protocol on stdio
pipelets --serve --provider openai-completions --base-url … --model …
```

Sessions persist in pi's layout so pi and pi-web can list and resume them
([session store](docs/session-store.md)). Tools are jailed to the working
directory by default; `--yolo` is the explicit opt-out, and tools run without
approval, matching pi.

## Gateway and triggers

`--gateway` serves the unit host over HTTP + SSE so a web UI or remote client can
attach to long-lived agents; `--triggers` fires scheduled prompts into sessions
with per-run and cumulative budgets. Routes, the SSE `?format=pi` stream, the
trigger schema, and the host-lock contract are in
[gateway and triggers](docs/gateway.md).

```bash
pipelets --gateway --gateway-addr 127.0.0.1:30142 \
  --provider openai-completions --base-url … --model … --api-key …
```

## Extensions

Load pi extensions/plugins with `--extension <path>` (repeatable). Tools they
register via `pi.registerTool` are exposed to the agent and run in QuickJS.
Extensions are deny-by-default for ambient access: filesystem, process, and
network access must be granted with `--extension-allow read,write,exec,http`,
and file/process paths are jailed to the working directory.

```bash
pipelets --serve --extension ./extensions/my-tool.ts
pipelets --serve --extension ./my.ts --extension-allow read
```

[plugin-conformance.md](docs/plugin-conformance.md) records how many of pi's own
example extensions load through the host (84/87).

## Docs

| Doc | Contents |
| --- | --- |
| [performance.md](docs/performance.md) | memory/CPU figures, stress and memory gates |
| [gateway.md](docs/gateway.md) | HTTP/SSE routes, triggers, budgets |
| [providers.md](docs/providers.md) | provider flags, prompt cache, wire parity |
| [session-store.md](docs/session-store.md) | JSONL format, context building, compaction |
| [images.md](docs/images.md) | native image pipeline (read attachments) |
| [rpc-protocol.md](docs/rpc-protocol.md) | JSON-lines unit protocol |
| [plugin-conformance.md](docs/plugin-conformance.md) | extension compatibility |
| [scope.md](docs/scope.md) | in/out of scope and hard constraints |
| [adr/](docs/adr/) | architecture decision records |

## Attribution

Derived in part from pi (<https://github.com/earendil-works/pi>), MIT. See
`LICENSE`.
